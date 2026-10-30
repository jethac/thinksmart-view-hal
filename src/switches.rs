//! The camera lens cover, and the microphone mute switch this unit does not have.
//!
//! Quirks, and where each comes from:
//!
//! * **One switch, not two.** The cover slider is `SW_CAMERA_LENS_COVER` on the
//!   `gpio-keys` input device (GPIO 87, active low), as the 7.3 device tree
//!   models it. The same device also advertises `SW_MICROPHONE_INSERT` for
//!   GPIO 86 ("Mic Mute" in `/proc/interrupts`), but nothing is fitted there:
//!   recordings on stock either side of a cover flip sat at the same level
//!   (−32.3 dB against −34.2 dB, room noise), and stock's DT entry for the line
//!   matches no hardware (`devices/privacy-switches/ANALYSIS.md` §1, §4). So
//!   `mic_mute_fitted` is always false and the GPIO 86 bit is never reported:
//!   a value from an unconnected pulled-up line would look like a real answer.
//! * **There is no hardware microphone mute.** A crashed audio stack can leave
//!   the microphones live with no physical fallback. Say so in any UI that shows
//!   a mute.
//! * **Not yet confirmed by a flip on mainline.** That the cover is GPIO 87 and
//!   not 86 rests on the device tree plus stock's key events; stock could not
//!   read the pin level (`ANALYSIS.md` §3, §8). `lens_covered` follows the kernel's
//!   convention (1 = covered); if a flip on mainline moves the other line, the
//!   two DT nodes swap and this module does not change.
//! * **Stock reported it differently.** Android's kernel delivered the cover as
//!   `KEY_POWER` on `gpio-keys`, which nothing consumed. Code written against
//!   stock behaviour will not see that here.

use crate::evdev::{self, Device, Stream, EV_SW, SW_CAMERA_LENS_COVER};
use crate::util::now_ms;
use serde::Serialize;
use std::sync::{Arc, Mutex};

const DEVICE: &str = "gpio-keys";

pub const MIC_MUTE_NOTE: &str = "not fitted on this unit: GPIO 86 is advertised by gpio-keys \
     but no switch is connected, and the cover does not mute the microphones";

#[derive(Clone, Default, Serialize)]
pub struct State {
    /// The input device carrying the switch, e.g. `/dev/input/event0`.
    pub device: Option<String>,
    /// True when the cover is closed over the camera.
    pub lens_covered: Option<bool>,
    /// Always false on the CD-18781Y; see `mic_mute_note`.
    pub mic_mute_fitted: bool,
    pub mic_mute_note: &'static str,
    pub updated_ms: Option<u64>,
    pub error: Option<String>,
}

/// A switch changing state.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Event {
    /// `lens_cover`.
    pub switch: &'static str,
    pub code: u16,
    /// For the lens cover: true when it closed.
    pub on: bool,
    /// The kernel's timestamp for the change.
    pub updated_ms: u64,
}

/// An iterator over switch changes. Blocks in `next` until one happens.
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
                Ok(ev) if ev.kind == EV_SW && ev.code == SW_CAMERA_LENS_COVER => {
                    return Some(Ok(Event {
                        switch: "lens_cover",
                        code: ev.code,
                        on: ev.value != 0,
                        updated_ms: ev.updated_ms,
                    }))
                }
                Ok(_) => continue,
            }
        }
    }
}

pub struct Switches {
    st: Mutex<State>,
}

fn device() -> Option<Device> {
    evdev::find(|d| d.name == DEVICE && d.switches.contains(&SW_CAMERA_LENS_COVER))
}

impl Switches {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            st: Mutex::new(State { mic_mute_fitted: false, mic_mute_note: MIC_MUTE_NOTE, ..Default::default() }),
        })
    }

    pub fn state(&self) -> State {
        self.st.lock().unwrap().clone()
    }

    /// Ask the kernel for the switch's current position (`EVIOCGSW`).
    pub fn refresh(&self) {
        let dev = device();
        let reading = dev
            .as_ref()
            .ok_or_else(|| format!("no {DEVICE} input device with SW_CAMERA_LENS_COVER"))
            .and_then(|d| evdev::open(&d.node))
            .and_then(|f| evdev::switches_on(&f, &[SW_CAMERA_LENS_COVER]));
        let mut st = self.st.lock().unwrap();
        st.device = dev.map(|d| d.node);
        st.updated_ms = Some(now_ms());
        match reading {
            Ok(v) => {
                st.lens_covered = Some(v[0]);
                st.error = None;
            }
            Err(e) => {
                st.lens_covered = None;
                st.error = Some(e);
            }
        }
    }

    /// Open the device for a blocking stream of changes. Takes no state lock;
    /// `state()` stays answerable while an iterator blocks on another thread.
    pub fn events(&self) -> Result<Events, String> {
        let dev = device().ok_or_else(|| format!("no {DEVICE} input device with SW_CAMERA_LENS_COVER"))?;
        Ok(Events { stream: Stream::open(&[dev])? })
    }
}
