//! The two side keys: Volume Up and Volume Down. There is no power key.
//!
//! Quirks, and where each comes from:
//!
//! * **Two devices.** Volume Up is a TLMM line (GPIO 85, active low) on the
//!   `gpio-keys` input device, shared with the lens cover. Volume Down is not a
//!   GPIO at all: it is the PM8953 power-on block's `resin` input, which the
//!   kernel names `pm8941_resin` after the driver's first PMIC
//!   (`/proc/bus/input/devices` on the 7.3 image; `devices/buttons/ANALYSIS.md`
//!   §2). [`Buttons::events`] merges both with `poll(2)`.
//! * **No power key.** The PON block's `kpdpwr` line probes but no key is wired
//!   to it, so mainline disables `pwrkey` (`ANALYSIS.md` §1, §5). `KEY_POWER`
//!   is never reported; on stock it arrived from the lens cover instead.
//! * **Volume Down is also the reset gesture.** A long hold on `resin` is the
//!   PMIC's `system-reset` on stock (`ANALYSIS.md` §4; not yet re-checked on
//!   mainline). Do not bind a long-press action to it that a user would hold
//!   for ten seconds.
//! * **Both are wakeup sources** in the 7.3 device tree.
//! * **Permissions.** The nodes are `root:input 0660`; the default user on the
//!   image is not in `input`.

use crate::evdev::{self, Device, Stream, EV_KEY, KEY_VOLUMEDOWN, KEY_VOLUMEUP};
use crate::util::now_ms;
use serde::Serialize;
use std::sync::{Arc, Mutex};

const UP_DEVICE: &str = "gpio-keys";

fn is_resin(name: &str) -> bool {
    name.contains("resin")
}

#[derive(Clone, Default, Serialize)]
pub struct State {
    /// The input device carrying each key, e.g. `/dev/input/event0`.
    pub volume_up_device: Option<String>,
    pub volume_down_device: Option<String>,
    /// Held down right now (`EVIOCGKEY`). None when the device is missing or
    /// cannot be opened.
    pub volume_up: Option<bool>,
    pub volume_down: Option<bool>,
    /// Always false on the CD-18781Y.
    pub power_key_fitted: bool,
    pub updated_ms: Option<u64>,
    pub error: Option<String>,
}

/// A key going down or coming up.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Event {
    /// `volume_up` or `volume_down`.
    pub key: &'static str,
    /// `KEY_VOLUMEUP` (115) or `KEY_VOLUMEDOWN` (114).
    pub code: u16,
    pub pressed: bool,
    /// Autorepeat (evdev value 2). Neither driver here enables it today.
    pub repeat: bool,
    /// The kernel's timestamp for the event.
    pub updated_ms: u64,
}

fn key_name(code: u16) -> Option<&'static str> {
    match code {
        KEY_VOLUMEUP => Some("volume_up"),
        KEY_VOLUMEDOWN => Some("volume_down"),
        _ => None,
    }
}

/// An iterator over key presses and releases. Blocks in `next`.
pub struct Events {
    stream: Stream,
}

impl Events {
    pub fn sources(&self) -> Vec<String> {
        self.stream.sources()
    }
}

impl Iterator for Events {
    type Item = Result<Event, String>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.stream.next()? {
                Err(e) => return Some(Err(e)),
                Ok(ev) if ev.kind == EV_KEY => {
                    let Some(key) = key_name(ev.code) else { continue };
                    return Some(Ok(Event {
                        key,
                        code: ev.code,
                        pressed: ev.value != 0,
                        repeat: ev.value == 2,
                        updated_ms: ev.updated_ms,
                    }));
                }
                Ok(_) => continue,
            }
        }
    }
}

fn up_device() -> Option<Device> {
    evdev::find(|d| d.name == UP_DEVICE && d.keys.contains(&KEY_VOLUMEUP))
}

fn down_device() -> Option<Device> {
    evdev::find(|d| is_resin(&d.name) && d.keys.contains(&KEY_VOLUMEDOWN))
}

fn held(dev: &Option<Device>, code: u16, what: &str) -> Result<bool, String> {
    let d = dev.as_ref().ok_or_else(|| format!("no input device for {what}"))?;
    let f = evdev::open(&d.node)?;
    Ok(evdev::keys_down(&f, &[code])?[0])
}

pub struct Buttons {
    st: Mutex<State>,
}

impl Buttons {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { st: Mutex::new(State::default()) })
    }

    pub fn state(&self) -> State {
        self.st.lock().unwrap().clone()
    }

    pub fn refresh(&self) {
        let up = up_device();
        let down = down_device();
        let up_held = held(&up, KEY_VOLUMEUP, "Volume Up");
        let down_held = held(&down, KEY_VOLUMEDOWN, "Volume Down");
        let mut st = self.st.lock().unwrap();
        st.volume_up_device = up.map(|d| d.node);
        st.volume_down_device = down.map(|d| d.node);
        st.power_key_fitted = false;
        st.updated_ms = Some(now_ms());
        let errors: Vec<String> =
            [&up_held, &down_held].iter().filter_map(|r| r.as_ref().err().cloned()).collect();
        st.volume_up = up_held.ok();
        st.volume_down = down_held.ok();
        st.error = (!errors.is_empty()).then(|| errors.join("; "));
    }

    /// Open both key devices for a blocking stream of presses. Fails if either
    /// is missing, rather than quietly delivering half the keys.
    pub fn events(&self) -> Result<Events, String> {
        let up = up_device().ok_or("no gpio-keys device with KEY_VOLUMEUP")?;
        let down = down_device().ok_or("no resin device with KEY_VOLUMEDOWN")?;
        Ok(Events { stream: Stream::open(&[up, down])? })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_resin_by_its_kernel_name() {
        assert!(is_resin("pm8941_resin"));
        assert!(is_resin("pm8953_resin"));
        assert!(!is_resin("gpio-keys"));
    }

    #[test]
    fn names_only_the_two_keys() {
        assert_eq!(key_name(115), Some("volume_up"));
        assert_eq!(key_name(114), Some("volume_down"));
        assert_eq!(key_name(evdev::KEY_POWER), None);
    }
}
