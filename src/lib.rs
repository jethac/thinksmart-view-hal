//! Hardware support for the Lenovo ThinkSmart View (CD-18781Y: Qualcomm APQ8053,
//! 2 GB) running Arch Linux ARM on mainline Linux 7.3.
//!
//! The sibling of `hub-hal` for the ThinkSmart Hub 500, built the same way: one
//! module per piece of hardware, each with a handle whose `state()` returns a
//! cloned, serialisable `State`, `refresh()` or `poll()` to update it, setters
//! that return `Result<(), String>`, and `None` for anything not known rather
//! than a plausible zero. Where the View has the same kind of thing as the Hub,
//! the field names, units and meanings are the Hub's (`STATE.md` lists which).
//!
//! - [`display`] The DSI panel's mode, and the WLED backlight: level, a
//!   configurable floor (the low end is an open question), and blanking through
//!   `bl_power` -- never DPMS, which did not come back on the previous port.
//! - [`orientation`] The BMA253 accelerometer as an angle in the screen plane and
//!   a quadrant, with the idle screen's filter. Each read is about 1 ms of CPU on
//!   a bit-banged bus.
//! - [`switches`] The camera lens cover. There is no microphone mute switch on
//!   this unit, and the cover does not mute anything.
//! - [`buttons`] Volume Up and Volume Down, on two different input devices. No
//!   power key.
//! - [`usb`] The USB gadget: functions, whether a host has configured it, and the
//!   address on `usb0`. Device mode only, with no CC negotiation.
//! - [`thermal`] Thermal zones, and CPU clocks where cpufreq works.
//! - [`inventory`] A one-shot description of the hardware.
//! - [`evdev`] The `ioctl`/`read` layer under the switches and buttons.
//! - [`util`] Process and file helpers, including the bounded-child-process
//!   primitive shared with `hub-hal`.
//!
//! Hardware facts and their provenance live in `jethac/lenovo-thinksmart-view`
//! (`CHIP.md`, `devices/*/ANALYSIS.md`, `rootfs/README.md`). This crate turns
//! them into code, and each module's documentation says which fact came from
//! where.

pub mod buttons;
pub mod display;
pub mod evdev;
pub mod inventory;
pub mod orientation;
pub mod switches;
pub mod thermal;
pub mod usb;
pub mod util;
