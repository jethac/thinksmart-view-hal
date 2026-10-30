//! The panel: backlight level and power through `/sys/class/backlight`, and what
//! mode the DSI connector is in.
//!
//! Unlike the Hub 500 there is a real backlight class device. The 8" 800x1280
//! panel (FocalTech FT8201 TDDI, stock name `boyift8201_800p_video`) is lit by
//! the PMI8950 WLED, four strings at 20 mA, exposed as
//! `/sys/class/backlight/backlight` with `max_brightness` 4095 and `type raw`
//! (measured on the 7.3 image). Writes are plain sysfs writes and return at once;
//! nothing here needs a child process or a cache.
//!
//! Quirks, and where each comes from:
//!
//! * **The low end is not settled.** The previous port (rickx, Plasma 6) said
//!   values below about 1200 of 4095 turn the panel off, and the idle screen
//!   never goes below 1400 on that basis (`rootfs/README.md`). A sweep on stock
//!   Android found the panel readable all the way down to about 1 of 255 and dark
//!   only at the very bottom (`devices/backlight/ANALYSIS.md` §4), so the 1200
//!   figure looks like a policy artefact rather than a hardware limit. Mainline
//!   has not been swept yet, and mainline still runs the WLED with OVP and boost
//!   limits above stock (§3). Until that sweep exists, [`Display::set_brightness`]
//!   clamps to a floor that defaults to the idle screen's 1400, and the caller can
//!   move it with [`Display::set_floor`]. It is a choice inherited from the rest
//!   of the image, not a measurement.
//! * **Blank with `bl_power`, not DPMS.** On the previous port, turning the DRM
//!   connector off was unrecoverable without restarting the compositor
//!   (`devices/display-panel/ANALYSIS.md` §7 and §8: the TDDI shares its reset
//!   with touch, and the cause -- DSI PLL, TDDI reset or touch -- is unknown).
//!   [`Display::set_power`] therefore only switches the backlight (`bl_power` 4 /
//!   0); the panel keeps scanning out. Nothing in this crate writes
//!   `/sys/class/drm/*/dpms` or does a KMS modeset. Measured on the 7.3 image:
//!   `bl_power` 4 then 0 is accepted with no WLED fault interrupt, but
//!   `actual_brightness` keeps reading the level throughout (the driver has no
//!   `get_brightness`), so it cannot tell you the backlight is off; `power` is
//!   read from `bl_power` for that reason. Whether 4 is fully dark has not been
//!   checked by eye.
//! * **Native orientation is portrait**, 800 wide by 1280 high. A compositor
//!   running it in landscape gives a 1280x800 window, and [`Panel::covers`]
//!   accepts either orientation.
//! * **The mode comes from sysfs plus debugfs**, never from opening
//!   `/dev/dri/card0`. On this image nothing holds DRM master most of the time
//!   (the idle screen takes it only while its wordmark turns), and a plain open of
//!   a card node with no master present makes the opener master. If the idle
//!   screen then wants to turn, it finds master taken and steps aside. debugfs
//!   (`/sys/kernel/debug/dri/0/state`, root only) gives the active CRTC mode,
//!   refresh rate included, with no such side effect. Without root the refresh
//!   rate is `None`, not 60.
//! * **The idle screen also manages this backlight.** `tsv-idle-screen start`
//!   lowers it to 2000 and `stop` restores what it found. Anything that sets the
//!   brightness while it runs should expect to be overwritten when it stops.

use crate::util::{now_ms, read_i64, read_trim, write_attr};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const BACKLIGHT: &str = "/sys/class/backlight/backlight";
const BACKLIGHT_CLASS: &str = "/sys/class/backlight";
const DRM: &str = "/sys/class/drm";
const DRM_DEBUG_STATE: &str = "/sys/kernel/debug/dri/0/state";
const REFRESH: Duration = Duration::from_secs(5);

/// The idle screen's floor (`tsv-idle-screen`), inherited until mainline has been
/// swept. See the module docs: the hardware may well go much lower.
pub const DEFAULT_FLOOR: u32 = 1400;

/// `FB_BLANK_UNBLANK` and `FB_BLANK_POWERDOWN`, the two `bl_power` values used.
const BL_ON: i64 = 0;
const BL_OFF: i64 = 4;

#[derive(Clone, Default, Serialize)]
pub struct State {
    /// DRM connector, as sysfs names it: `card0-DSI-1`. Same meaning as
    /// `hub-hal`'s, where it is `card0-DP-1`.
    pub connector: Option<String>,
    /// `connected`, from the connector's `status`.
    pub status: Option<String>,
    /// DPMS as the connector reports it. Read only; see the module docs.
    pub dpms: Option<String>,
    /// The mode the CRTC is running (debugfs), or the connector's preferred mode
    /// (sysfs) when debugfs is not readable.
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub refresh_hz: Option<u32>,
    /// Backlight class device, `/sys/class/backlight/backlight`.
    pub backlight: Option<String>,
    /// Current level, in the device's own units out of `brightness_max` (4095).
    /// `hub-hal`'s is out of 100 for the same reason: it is the panel's scale.
    pub brightness: Option<u32>,
    pub brightness_max: Option<u32>,
    /// The lowest level `set_brightness` will write. Not a hardware limit.
    pub brightness_floor: u32,
    /// `on`, `standby`, `suspend` or `off`, from `bl_power`, in `hub-hal`'s
    /// vocabulary. Only the backlight; the panel itself stays powered.
    pub power: Option<String>,
    /// When a `blank` started by this process will end.
    pub blanked_until: Option<u64>,
    pub updated_ms: Option<u64>,
    pub error: Option<String>,
}

pub struct Display {
    st: Mutex<State>,
}

impl Display {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            st: Mutex::new(State { brightness_floor: DEFAULT_FLOOR, ..Default::default() }),
        })
    }

    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        thread::spawn(move || loop {
            me.refresh();
            thread::sleep(REFRESH);
        });
    }

    pub fn state(&self) -> State {
        self.st.lock().unwrap().clone()
    }

    pub fn refresh(&self) {
        let panel = panel();
        let mode = read_trim(DRM_DEBUG_STATE).and_then(|s| parse_debugfs_mode(&s));
        let bl = backlight_dir();
        let mut st = self.st.lock().unwrap();
        st.updated_ms = Some(now_ms());
        st.connector = panel.as_ref().map(|p| p.connector.clone());
        st.status = panel.as_ref().map(|p| p.status.clone());
        st.dpms = panel.as_ref().and_then(|p| p.dpms.clone());
        match (&mode, &panel) {
            (Some(m), _) => {
                st.width = Some(m.width);
                st.height = Some(m.height);
                st.refresh_hz = Some(m.refresh_hz);
            }
            (None, Some(p)) => {
                st.width = Some(p.width);
                st.height = Some(p.height);
                st.refresh_hz = None;
            }
            (None, None) => {
                st.width = None;
                st.height = None;
                st.refresh_hz = None;
            }
        }
        let Some(bl) = bl else {
            st.backlight = None;
            st.brightness = None;
            st.brightness_max = None;
            st.power = None;
            st.error = Some("no backlight in /sys/class/backlight".into());
            return;
        };
        st.backlight = Some(bl.display().to_string());
        st.brightness = read_i64(bl.join("brightness")).map(|v| v as u32);
        st.brightness_max = read_i64(bl.join("max_brightness")).map(|v| v as u32);
        st.power = read_i64(bl.join("bl_power")).map(power_name);
        st.error = match (&panel, st.brightness) {
            (None, _) => Some("no connected DSI panel in /sys/class/drm".into()),
            (_, None) => Some(format!("{}: brightness unreadable", bl.display())),
            _ => None,
        };
    }

    /// Change the floor `set_brightness` clamps to. 0 is allowed and means the
    /// caller has measured, or accepts, a level that may light nothing.
    pub fn set_floor(&self, floor: u32) {
        self.st.lock().unwrap().brightness_floor = floor;
    }

    /// Set the backlight level in device units (0..=`brightness_max`), clamped to
    /// the floor. Returns once the kernel has taken the write.
    pub fn set_brightness(&self, value: u32) -> Result<(), String> {
        let bl = backlight_dir().ok_or("no backlight in /sys/class/backlight")?;
        let max = read_i64(bl.join("max_brightness")).ok_or("max_brightness unreadable")? as u32;
        let floor = self.st.lock().unwrap().brightness_floor;
        let v = clamp_level(value, floor, max);
        write_attr(bl.join("brightness"), &v.to_string())?;
        let mut st = self.st.lock().unwrap();
        st.brightness = Some(v);
        st.brightness_max = Some(max);
        st.updated_ms = Some(now_ms());
        st.error = None;
        Ok(())
    }

    /// Backlight on or off through `bl_power`. The level is kept, so on brings
    /// back what was there. Never DPMS; see the module docs.
    pub fn set_power(&self, on: bool) -> Result<(), String> {
        let bl = backlight_dir().ok_or("no backlight in /sys/class/backlight")?;
        write_attr(bl.join("bl_power"), &(if on { BL_ON } else { BL_OFF }).to_string())?;
        let mut st = self.st.lock().unwrap();
        st.power = Some(power_name(if on { BL_ON } else { BL_OFF }));
        st.updated_ms = Some(now_ms());
        Ok(())
    }

    /// Switch the backlight off for `seconds`, then back on, from a thread of
    /// this process -- with the screen dark there may be nothing to tap. Same
    /// contract as `hub-hal`'s `blank`.
    pub fn blank(self: &Arc<Self>, seconds: u64) -> Result<(), String> {
        if self.st.lock().unwrap().blanked_until.is_some() {
            return Err("already blanked".into());
        }
        let seconds = seconds.clamp(1, 30);
        self.set_power(false)?;
        self.st.lock().unwrap().blanked_until = Some(now_ms() + seconds * 1000);
        let me = self.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(seconds));
            let r = me.set_power(true);
            {
                let mut st = me.st.lock().unwrap();
                st.blanked_until = None;
                if let Err(e) = r {
                    st.error = Some(format!("backlight did not come back on: {e}"));
                }
            }
            me.refresh();
        });
        Ok(())
    }
}

fn clamp_level(value: u32, floor: u32, max: u32) -> u32 {
    value.max(floor.min(max)).min(max)
}

/// `/sys/class/backlight/backlight`, or whatever single backlight exists if a
/// later kernel names it differently (the WLED's DT label decides the name).
fn backlight_dir() -> Option<PathBuf> {
    let fixed = Path::new(BACKLIGHT);
    if fixed.join("brightness").exists() {
        return Some(fixed.to_path_buf());
    }
    let all = crate::util::entries(BACKLIGHT_CLASS);
    (all.len() == 1).then(|| all[0].clone())
}

fn power_name(v: i64) -> String {
    match v {
        0 => "on".into(),
        1 => "standby".into(),
        2 | 3 => "suspend".into(),
        4 => "off".into(),
        n => format!("{n}"),
    }
}

/// A connected display, as the kernel sees it. The same shape as `hub-hal`'s
/// `display::Panel`, plus the raw status and DPMS strings.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Panel {
    /// DRM connector, e.g. `card0-DSI-1`.
    pub connector: String,
    /// The preferred mode, which on a fixed panel is its native resolution.
    pub width: u32,
    pub height: u32,
    /// The View's own screen (DSI). It has no other display output.
    pub internal: bool,
    /// DPMS says the connector is on. A blanked backlight still reports On.
    pub powered: bool,
    pub status: String,
    pub dpms: Option<String>,
}

/// How far a window falls short of covering the panel.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Shortfall {
    pub connector: String,
    pub panel_width: u32,
    pub panel_height: u32,
    pub window_width: u32,
    pub window_height: u32,
    pub unused_width: u32,
    pub unused_height: u32,
}

impl std::fmt::Display for Shortfall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "window is {}x{} on {} which is {}x{}: {} px unused across, {} px unused down",
            self.window_width,
            self.window_height,
            self.connector,
            self.panel_width,
            self.panel_height,
            self.unused_width,
            self.unused_height
        )
    }
}

impl Panel {
    /// Whether a window of this physical size covers the whole panel, in either
    /// orientation: the panel is portrait-native and is often run rotated.
    pub fn covers(&self, window_width: u32, window_height: u32) -> bool {
        (window_width >= self.width && window_height >= self.height)
            || (window_width >= self.height && window_height >= self.width)
    }

    /// What is missing, or None when the window covers the panel. Measured
    /// against whichever orientation the window is closer to. Sizes are
    /// physical pixels.
    pub fn shortfall(&self, window_width: u32, window_height: u32) -> Option<Shortfall> {
        if self.covers(window_width, window_height) {
            return None;
        }
        let landscape_window = window_width > window_height;
        let landscape_panel = self.width > self.height;
        let (pw, ph) = if landscape_window == landscape_panel {
            (self.width, self.height)
        } else {
            (self.height, self.width)
        };
        Some(Shortfall {
            connector: self.connector.clone(),
            panel_width: pw,
            panel_height: ph,
            window_width,
            window_height,
            unused_width: pw.saturating_sub(window_width),
            unused_height: ph.saturating_sub(window_height),
        })
    }
}

/// Every connected display, in connector order.
pub fn panels() -> Vec<Panel> {
    let mut found: Vec<Panel> = crate::util::entries(DRM)
        .into_iter()
        .filter_map(|path| {
            let name = crate::util::name_of(&path);
            if !name.contains('-') {
                return None;
            }
            let status = read_trim(path.join("status"))?;
            if status != "connected" {
                return None;
            }
            let (width, height) = parse_modes(&read_trim(path.join("modes"))?)?;
            let dpms = read_trim(path.join("dpms"));
            Some(Panel {
                internal: name.to_ascii_uppercase().contains("-DSI-"),
                powered: dpms.as_deref().is_some_and(|d| d.eq_ignore_ascii_case("On")),
                connector: name,
                width,
                height,
                status,
                dpms,
            })
        })
        .collect();
    found.sort_by(|a, b| a.connector.cmp(&b.connector));
    found
}

/// The View's own screen: the DSI connector, or the only one connected.
pub fn panel() -> Option<Panel> {
    let all = panels();
    all.iter()
        .find(|p| p.internal)
        .cloned()
        .or_else(|| (all.len() == 1).then(|| all[0].clone()))
}

/// First line of a connector's `modes`: the preferred mode.
fn parse_modes(text: &str) -> Option<(u32, u32)> {
    let first = text.lines().next()?.trim();
    let (w, h) = first.split_once('x')?;
    let h: String = h.chars().take_while(|c| c.is_ascii_digit()).collect();
    Some((w.trim().parse().ok()?, h.parse().ok()?))
}

#[derive(Debug, PartialEq)]
struct Mode {
    width: u32,
    height: u32,
    refresh_hz: u32,
}

/// The first active CRTC's mode from `/sys/kernel/debug/dri/0/state`:
///
/// ```text
/// crtc[61]: crtc-0
///     enable=1
///     active=1
///     mode: "800x1280": 60 78645 800 825 850 875 1280 1464 1466 1498 0x48 0x0
/// ```
///
/// The fields after the name are vrefresh, clock (kHz), the four horizontal and
/// four vertical timings, type and flags (`DRM_MODE_FMT` in drm_modes.h).
fn parse_debugfs_mode(text: &str) -> Option<Mode> {
    let mut active = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with("crtc[") {
            active = false;
        } else if line == "active=1" {
            active = true;
        } else if let Some(rest) = line.strip_prefix("mode: \"") {
            if !active {
                continue;
            }
            let (_, after) = rest.split_once('"')?;
            let t: Vec<&str> = after.trim_start_matches(':').split_whitespace().collect();
            if t.len() < 10 {
                return None;
            }
            return Some(Mode {
                refresh_hz: t[0].parse().ok()?,
                width: t[2].parse().ok()?,
                height: t[6].parse().ok()?,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATE: &str = include_str!("../tests/fixtures/dri-0-state-crtc.txt");

    #[test]
    fn reads_the_running_mode_from_debugfs() {
        assert_eq!(parse_debugfs_mode(STATE), Some(Mode { width: 800, height: 1280, refresh_hz: 60 }));
    }

    #[test]
    fn an_inactive_crtc_has_no_mode() {
        let off = STATE.replace("active=1", "active=0");
        assert_eq!(parse_debugfs_mode(&off), None);
        assert_eq!(parse_debugfs_mode(""), None);
    }

    #[test]
    fn reads_the_preferred_mode_from_sysfs() {
        assert_eq!(parse_modes("800x1280\n"), Some((800, 1280)));
        assert_eq!(parse_modes(""), None);
    }

    #[test]
    fn the_floor_holds_and_the_ceiling_is_the_panel() {
        assert_eq!(clamp_level(0, DEFAULT_FLOOR, 4095), 1400);
        assert_eq!(clamp_level(2000, DEFAULT_FLOOR, 4095), 2000);
        assert_eq!(clamp_level(9999, DEFAULT_FLOOR, 4095), 4095);
        assert_eq!(clamp_level(5, 0, 4095), 5);
        // A floor above the maximum cannot push past the maximum.
        assert_eq!(clamp_level(0, 5000, 4095), 4095);
    }

    #[test]
    fn bl_power_names_follow_hub_hal() {
        assert_eq!(power_name(0), "on");
        assert_eq!(power_name(4), "off");
    }

    fn view() -> Panel {
        Panel {
            connector: "card0-DSI-1".into(),
            width: 800,
            height: 1280,
            internal: true,
            powered: true,
            status: "connected".into(),
            dpms: Some("On".into()),
        }
    }

    #[test]
    fn a_rotated_window_still_covers_the_panel() {
        assert!(view().covers(800, 1280));
        assert!(view().covers(1280, 800));
        assert_eq!(view().shortfall(1280, 800), None);
    }

    #[test]
    fn a_short_landscape_window_is_measured_against_landscape() {
        let s = view().shortfall(1280, 765).expect("a shortfall");
        assert_eq!((s.panel_width, s.panel_height), (1280, 800));
        assert_eq!(s.unused_height, 35);
        assert_eq!(s.unused_width, 0);
    }
}
