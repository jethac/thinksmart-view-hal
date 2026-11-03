//! Whether a finger is on the touchscreen, for waking the screen and counting
//! activity. Positions and gestures are libinput's business, not this crate's.
//!
//! The FocalTech FT8201 (the panel's own TDDI controller) is driven by
//! `edt-ft5x06` and named `generic ft5x06 (09)` on the 7.3 image; it is found
//! here by its `BTN_TOUCH` capability rather than by that name.
//!
//! Quirks:
//!
//! * **Touch keeps working with the backlight off.** The panel and its touch
//!   controller share silicon and a reset line, so blanking the panel itself
//!   (DPMS, a modeset) would also stop touch. `display::set_power(false)` only
//!   switches the WLED off; the panel keeps scanning out and touch keeps
//!   reporting, which is what makes wake on touch possible.
//! * **Not grabbed.** The device is read with plain `read(2)`, so libinput (the
//!   app's own touch input) still gets every event. A caller that wakes the
//!   screen on a touch must keep that touch from also pressing whatever is under
//!   the finger.

use crate::evdev::{self, Device, Stream, EV_KEY};

/// `BTN_TOUCH`: 1 when the first finger lands, 0 when the last one lifts.
pub const BTN_TOUCH: u16 = 0x14a;

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// True when a finger lands on an empty screen, false when the last lifts.
    pub down: bool,
    /// The kernel's timestamp, epoch milliseconds.
    pub updated_ms: u64,
}

/// The touchscreen's event device.
pub fn device() -> Option<Device> {
    evdev::find(|d| d.keys.contains(&BTN_TOUCH))
}

pub struct Events {
    stream: Stream,
    name: String,
}

impl Events {
    pub fn source(&self) -> &str {
        &self.name
    }
}

/// A blocking stream of finger down and up.
pub fn events() -> Result<Events, String> {
    let d = device().ok_or("no touchscreen (no input device reports BTN_TOUCH)")?;
    Ok(Events { name: d.name.clone(), stream: Stream::open(std::slice::from_ref(&d))? })
}

impl Iterator for Events {
    type Item = Result<Event, String>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.stream.next()? {
                Ok(e) if e.kind == EV_KEY && e.code == BTN_TOUCH => {
                    return Some(Ok(Event { down: e.value != 0, updated_ms: e.updated_ms }))
                }
                Ok(_) => {}
                Err(e) => return Some(Err(e)),
            }
        }
    }
}
