//! Which way up the tablet is, from the BMA253 accelerometer.
//!
//! The angle is the tablet's turn in the screen plane, clockwise as the viewer
//! sees it: `θ = atan2(a_x, −a_y)` in degrees, 0 upright in native portrait,
//! +90 turned a quarter clockwise. Only the in-plane components count, so the
//! stand's lean-back (gravity partly on z) does not move it. This is the same
//! formula, filter and flat rule as the idle screen (`tsv-idle-rotate.c`), so
//! the two agree on what "upright" is.
//!
//! Quirks, and where each comes from:
//!
//! * **Bit-banged bus.** On stock the ADSP owns the sensor bus; mainline drives
//!   SDA/SCL on GPIO 6/7 with `i2c-gpio` instead, so every sysfs read busy-waits
//!   in the kernel for about 1 ms of CPU (`CHIP.md`, measured in
//!   `streams/idle-rotate/NOTES.md`: 100 reads cost 10 ticks). A poll here reads
//!   x, y and z -- three transfers, about 3 ms -- so poll slowly. The idle screen
//!   is already reading the same sensor at 2 Hz (25 Hz while turning); a second
//!   reader doubles the bus traffic, which is why the voice app is meant to take
//!   this over rather than run beside it.
//! * **Axes.** There is no `mount-matrix` in the 7.3 device tree, so
//!   `in_mount_matrix` reads identity and the axes are the PCB's
//!   (`devices/accelerometer/ANALYSIS.md` §5). The matrix is still applied, so a
//!   later DT that adds one keeps this correct. The direction of θ was
//!   calibrated by hand: upright x +0.6, y −9.5 m/s²; turned clockwise x +6.4,
//!   y −3.4 (`streams/idle-rotate/NOTES.md`).
//! * **Flat.** Below 3 m/s² of in-plane gravity the tablet is lying down and θ
//!   is noise. `flat` goes true there and only clears above 3.5 m/s²; while flat
//!   the last angle and quadrant are held, not recomputed.
//! * **Motion interrupt unproven.** GPIO 42 delivers data-ready interrupts
//!   (255 in 2 s at 125 Hz), but the slope ("any-motion", IIO `roc`) event has
//!   never been seen to fire. This module does not use events; it polls, and
//!   never writes the sensor's attributes (sampling rate, event enables), which
//!   the idle screen owns.
//! * **Naming.** The board calls it BMA253, Android's ADSP called it BMA255; the
//!   IIO device is named `bma253` and is found by that name.

use crate::util::{entries, now_ms, read_trim};
use serde::Serialize;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const IIO: &str = "/sys/bus/iio/devices";
const NAME: &str = "bma253";

/// One-pole low-pass time constant for the gravity vector.
pub const TAU_S: f64 = 0.12;
/// Below this in-plane gravity the tablet is flat...
pub const FLAT_MS2: f64 = 3.0;
/// ...until it is back above this.
pub const UNFLAT_MS2: f64 = 3.5;
/// How far past a quadrant's 45° edge θ must go before `upright` changes. A
/// choice for a UI that rotates, not a measurement: without it, a tablet held
/// near 45° flips back and forth on noise.
pub const QUADRANT_HYSTERESIS_DEG: f64 = 10.0;
/// Default poll period for `start`: the idle screen's still-rate.
pub const DEFAULT_PERIOD: Duration = Duration::from_millis(500);

#[derive(Clone, Default, Serialize)]
pub struct State {
    /// The IIO device directory.
    pub path: Option<String>,
    /// θ, degrees in (−180, 180], 0 upright portrait, positive clockwise, from
    /// the filtered vector. Held while `flat`. None until the first upright read.
    pub angle_deg: Option<f64>,
    /// `portrait`, `landscape-cw`, `portrait-flipped` or `landscape-ccw`, with
    /// hysteresis. Held while `flat`.
    pub upright: Option<&'static str>,
    pub flat: bool,
    /// Filtered in-plane gravity magnitude, the quantity `flat` is decided on.
    pub in_plane_ms2: Option<f64>,
    /// Total gravity magnitude of the last sample; about 9.8 at rest.
    pub gravity_ms2: Option<f64>,
    /// The last sample in m/s², device frame (after the mount matrix).
    pub x_ms2: Option<f64>,
    pub y_ms2: Option<f64>,
    pub z_ms2: Option<f64>,
    /// The last sample as the driver gave it, before scale and matrix.
    pub raw: Option<[i64; 3]>,
    /// `in_accel_scale`, m/s² per count (0.019154 at ±4 g).
    pub scale: Option<f64>,
    pub mount_matrix: Option<[f64; 9]>,
    /// How long the last three-axis read took, wall clock. About 3 ms here.
    pub last_read_us: Option<u64>,
    pub reads: u64,
    pub updated_ms: Option<u64>,
    pub error: Option<String>,
}

/// The idle screen's smoothing: one pole on (x, y), with the flat hysteresis.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    gx: f64,
    gy: f64,
    primed: bool,
    flat: bool,
    angle: Option<f64>,
    quadrant: Option<Quadrant>,
}

impl Filter {
    /// Feed one in-plane sample taken `dt_s` after the previous one. The first
    /// sample primes the filter rather than being averaged with zero.
    pub fn update(&mut self, x: f64, y: f64, dt_s: f64) {
        if self.primed {
            let a = 1.0 - (-dt_s.max(0.0) / TAU_S).exp();
            self.gx += a * (x - self.gx);
            self.gy += a * (y - self.gy);
        } else {
            self.gx = x;
            self.gy = y;
            self.primed = true;
        }
        let mag = self.in_plane();
        self.flat = if self.flat { mag <= UNFLAT_MS2 } else { mag < FLAT_MS2 };
        if !self.flat {
            let a = tilt_deg(self.gx, self.gy);
            self.angle = Some(a);
            self.quadrant = Some(Quadrant::with_hysteresis(self.quadrant, a, QUADRANT_HYSTERESIS_DEG));
        }
    }

    pub fn in_plane(&self) -> f64 {
        self.gx.hypot(self.gy)
    }
    pub fn flat(&self) -> bool {
        self.flat
    }
    pub fn angle_deg(&self) -> Option<f64> {
        self.angle
    }
    pub fn quadrant(&self) -> Option<Quadrant> {
        self.quadrant
    }
}

/// θ for an in-plane gravity vector, degrees in (−180, 180].
pub fn tilt_deg(x: f64, y: f64) -> f64 {
    x.atan2(-y).to_degrees()
}

/// Which edge of the panel is up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Quadrant {
    /// Native orientation: 800 wide, 1280 high.
    Portrait,
    /// Turned a quarter clockwise; the panel's top edge is on the viewer's right.
    LandscapeCw,
    PortraitFlipped,
    /// Turned a quarter anticlockwise.
    LandscapeCcw,
}

impl Quadrant {
    pub fn name(self) -> &'static str {
        match self {
            Quadrant::Portrait => "portrait",
            Quadrant::LandscapeCw => "landscape-cw",
            Quadrant::PortraitFlipped => "portrait-flipped",
            Quadrant::LandscapeCcw => "landscape-ccw",
        }
    }

    /// The quadrant's centre angle.
    pub fn centre_deg(self) -> f64 {
        match self {
            Quadrant::Portrait => 0.0,
            Quadrant::LandscapeCw => 90.0,
            Quadrant::PortraitFlipped => 180.0,
            Quadrant::LandscapeCcw => -90.0,
        }
    }

    /// The nearest quadrant, no memory. An angle exactly on an edge belongs to
    /// the quadrant clockwise of it: 45° is landscape-cw, −45° is portrait.
    pub fn nearest(deg: f64) -> Quadrant {
        let d = wrap180(deg);
        if (-45.0..45.0).contains(&d) {
            Quadrant::Portrait
        } else if (45.0..135.0).contains(&d) {
            Quadrant::LandscapeCw
        } else if (-135.0..-45.0).contains(&d) {
            Quadrant::LandscapeCcw
        } else {
            Quadrant::PortraitFlipped
        }
    }

    /// Stay in `prev` until θ is more than 45° + `margin` from its centre.
    pub fn with_hysteresis(prev: Option<Quadrant>, deg: f64, margin: f64) -> Quadrant {
        match prev {
            Some(q) if wrap180(deg - q.centre_deg()).abs() <= 45.0 + margin => q,
            _ => Quadrant::nearest(deg),
        }
    }
}

/// An angle folded into (−180, 180].
pub fn wrap180(d: f64) -> f64 {
    let r = (d + 180.0).rem_euclid(360.0) - 180.0;
    if r == -180.0 {
        180.0
    } else {
        r
    }
}

/// `in_mount_matrix`: `"1, 0, 0; 0, 1, 0; 0, 0, 1"`, rows separated by `;`.
pub fn parse_mount_matrix(text: &str) -> Option<[f64; 9]> {
    let v: Vec<f64> = text
        .split([',', ';'])
        .map(|t| t.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .ok()?;
    v.try_into().ok()
}

/// Raw counts through scale and matrix to m/s² in the device frame.
pub fn to_ms2(raw: [i64; 3], scale: f64, m: &[f64; 9]) -> [f64; 3] {
    let r = raw.map(|v| v as f64 * scale);
    [0, 1, 2].map(|i| m[3 * i] * r[0] + m[3 * i + 1] * r[1] + m[3 * i + 2] * r[2])
}

/// The IIO directory of the accelerometer, by name. Reading `name` is answered
/// by the IIO core and costs no bus transfer.
pub fn find() -> Option<PathBuf> {
    entries(IIO).into_iter().find(|p| read_trim(p.join("name")).as_deref() == Some(NAME))
}

struct Sensor {
    dir: PathBuf,
    axes: [File; 3],
    scale: f64,
    matrix: [f64; 9],
}

impl Sensor {
    fn open(dir: &Path) -> Result<Self, String> {
        let scale = read_trim(dir.join("in_accel_scale"))
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|s| *s > 0.0)
            .ok_or_else(|| format!("{}: no in_accel_scale", dir.display()))?;
        let identity = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let matrix = read_trim(dir.join("in_mount_matrix"))
            .and_then(|s| parse_mount_matrix(&s))
            .unwrap_or(identity);
        let open = |a: char| {
            let p = dir.join(format!("in_accel_{a}_raw"));
            File::open(&p).map_err(|e| format!("{}: {e}", p.display()))
        };
        Ok(Self { dir: dir.to_path_buf(), axes: [open('x')?, open('y')?, open('z')?], scale, matrix })
    }

    /// pread-style: rewind and read each kept-open attribute, so a poll costs
    /// three bus transfers and no path lookups.
    fn read(&mut self) -> Result<[i64; 3], String> {
        let mut out = [0i64; 3];
        for (i, f) in self.axes.iter_mut().enumerate() {
            let mut s = String::new();
            f.seek(SeekFrom::Start(0)).and_then(|_| f.read_to_string(&mut s)).map_err(|e| {
                format!("{}/in_accel_{}_raw: {e}", self.dir.display(), ['x', 'y', 'z'][i])
            })?;
            out[i] = s.trim().parse().map_err(|_| format!("unparseable sample {s:?}"))?;
        }
        Ok(out)
    }
}

pub struct Orientation {
    st: Mutex<State>,
    inner: Mutex<(Option<Sensor>, Filter, Option<Instant>)>,
}

impl Orientation {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { st: Mutex::new(State::default()), inner: Mutex::new((None, Filter::default(), None)) })
    }

    /// Poll every `period` on a thread of its own. Mind the bus cost above.
    pub fn start(self: &Arc<Self>, period: Duration) {
        let me = self.clone();
        thread::spawn(move || loop {
            me.poll();
            thread::sleep(period);
        });
    }

    pub fn state(&self) -> State {
        self.st.lock().unwrap().clone()
    }

    /// Take one sample and fold it into the filter.
    pub fn poll(&self) {
        let mut inner = self.inner.lock().unwrap();
        let (sensor, filter, last) = &mut *inner;
        if sensor.is_none() {
            match find().ok_or_else(|| format!("no IIO device named '{NAME}'")).and_then(|d| Sensor::open(&d)) {
                Ok(s) => {
                    let mut st = self.st.lock().unwrap();
                    st.path = Some(s.dir.display().to_string());
                    st.scale = Some(s.scale);
                    st.mount_matrix = Some(s.matrix);
                    *sensor = Some(s);
                }
                Err(e) => {
                    let mut st = self.st.lock().unwrap();
                    st.error = Some(e);
                    st.updated_ms = Some(now_ms());
                    return;
                }
            }
        }
        let s = sensor.as_mut().expect("opened above");
        let t = Instant::now();
        let raw = match s.read() {
            Ok(r) => r,
            Err(e) => {
                // Drop the handles: a re-probe (module reload) gives a new
                // iio:deviceN and these files would read the old one forever.
                *sensor = None;
                let mut st = self.st.lock().unwrap();
                st.error = Some(e);
                st.updated_ms = Some(now_ms());
                return;
            }
        };
        let read_us = t.elapsed().as_micros() as u64;
        let g = to_ms2(raw, s.scale, &s.matrix);
        let dt = last.map(|l| l.elapsed().as_secs_f64()).unwrap_or(0.0);
        *last = Some(t);
        filter.update(g[0], g[1], dt);

        let mut st = self.st.lock().unwrap();
        st.raw = Some(raw);
        st.x_ms2 = Some(g[0]);
        st.y_ms2 = Some(g[1]);
        st.z_ms2 = Some(g[2]);
        st.gravity_ms2 = Some((g[0] * g[0] + g[1] * g[1] + g[2] * g[2]).sqrt());
        st.in_plane_ms2 = Some(filter.in_plane());
        st.flat = filter.flat();
        st.angle_deg = filter.angle_deg();
        st.upright = filter.quadrant().map(Quadrant::name);
        st.last_read_us = Some(read_us);
        st.reads += 1;
        st.updated_ms = Some(now_ms());
        st.error = None;
    }
}

/// Is there an accelerometer, without reading it: no bus transfer (inventory).
pub fn present() -> bool {
    find().is_some_and(|d| d.join("in_accel_x_raw").exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() < tol
    }

    #[test]
    fn the_hand_calibration_gives_the_right_signs() {
        // streams/idle-rotate/NOTES.md: upright x +0.6, y -9.5; turned
        // clockwise x +6.4, y -3.4.
        let up = tilt_deg(0.6, -9.5);
        let cw = tilt_deg(6.4, -3.4);
        assert!(close(up, 3.6, 0.1), "{up}");
        assert!(close(cw, 62.0, 0.1), "{cw}");
        assert_eq!(Quadrant::nearest(up), Quadrant::Portrait);
        assert_eq!(Quadrant::nearest(cw), Quadrant::LandscapeCw);
    }

    #[test]
    fn readings_from_this_unit() {
        let id = parse_mount_matrix("1, 0, 0; 0, 1, 0; 0, 0, 1").unwrap();
        // tests/fixtures: the unit on its stand in landscape, raw 403, -25, 309.
        let g = to_ms2([403, -25, 309], 0.019154, &id);
        assert!(close(g[0], 7.719, 0.001) && close(g[1], -0.479, 0.001));
        let a = tilt_deg(g[0], g[1]);
        assert!(close(a, 86.4, 0.1), "{a}");
        assert_eq!(Quadrant::nearest(a), Quadrant::LandscapeCw);
        // The reference pose, upright on the stand: x +0.30, y -9.54.
        assert!(close(tilt_deg(0.30, -9.54), 1.8, 0.05));
    }

    #[test]
    fn quadrants_go_round_clockwise() {
        assert_eq!(Quadrant::nearest(0.0), Quadrant::Portrait);
        assert_eq!(Quadrant::nearest(44.9), Quadrant::Portrait);
        assert_eq!(Quadrant::nearest(45.0), Quadrant::LandscapeCw);
        assert_eq!(Quadrant::nearest(134.9), Quadrant::LandscapeCw);
        assert_eq!(Quadrant::nearest(135.0), Quadrant::PortraitFlipped);
        assert_eq!(Quadrant::nearest(180.0), Quadrant::PortraitFlipped);
        assert_eq!(Quadrant::nearest(-179.0), Quadrant::PortraitFlipped);
        assert_eq!(Quadrant::nearest(-135.0), Quadrant::LandscapeCcw);
        assert_eq!(Quadrant::nearest(-45.1), Quadrant::LandscapeCcw);
        assert_eq!(Quadrant::nearest(-45.0), Quadrant::Portrait);
        assert_eq!(Quadrant::nearest(270.0), Quadrant::LandscapeCcw);
        assert_eq!(tilt_deg(0.0, 9.8), 180.0);
        assert_eq!(tilt_deg(-9.8, 0.0), -90.0);
    }

    #[test]
    fn hysteresis_holds_near_an_edge() {
        let p = Some(Quadrant::Portrait);
        assert_eq!(Quadrant::with_hysteresis(p, 50.0, 10.0), Quadrant::Portrait);
        assert_eq!(Quadrant::with_hysteresis(p, 56.0, 10.0), Quadrant::LandscapeCw);
        let l = Some(Quadrant::LandscapeCw);
        assert_eq!(Quadrant::with_hysteresis(l, 40.0, 10.0), Quadrant::LandscapeCw);
        assert_eq!(Quadrant::with_hysteresis(l, 34.0, 10.0), Quadrant::Portrait);
        // Across the ±180 seam.
        let f = Some(Quadrant::PortraitFlipped);
        assert_eq!(Quadrant::with_hysteresis(f, -130.0, 10.0), Quadrant::PortraitFlipped);
        assert_eq!(Quadrant::with_hysteresis(None, -130.0, 10.0), Quadrant::LandscapeCcw);
    }

    #[test]
    fn wrap_folds_into_the_half_open_range() {
        assert_eq!(wrap180(190.0), -170.0);
        assert_eq!(wrap180(-180.0), 180.0);
        assert_eq!(wrap180(540.0), 180.0);
        assert_eq!(wrap180(-45.0), -45.0);
    }

    #[test]
    fn the_filter_settles_with_the_idle_screens_time_constant() {
        let mut f = Filter::default();
        f.update(0.3, -9.54, 0.0);
        assert!(close(f.angle_deg().unwrap(), 1.8, 0.05));
        // Turned to landscape: after one time constant the vector has moved
        // 1 - 1/e of the way, and after a second it is close.
        f.update(9.5, 0.0, TAU_S);
        let gx_one_tau = 0.3 + (9.5 - 0.3) * (1.0 - (-1.0f64).exp());
        assert!(close(f.gx, gx_one_tau, 1e-9));
        for _ in 0..25 {
            f.update(9.5, 0.0, 0.04);
        }
        assert!(close(f.angle_deg().unwrap(), 90.0, 1.0));
        assert_eq!(f.quadrant(), Some(Quadrant::LandscapeCw));
    }

    #[test]
    fn flat_holds_the_last_angle_with_hysteresis() {
        let mut f = Filter::default();
        f.update(6.4, -3.4, 0.0);
        let held = f.angle_deg().unwrap();
        // Laid on its back: almost all of gravity on z.
        f.update(0.5, -0.2, 0.0);
        f.update(0.5, -0.2, 10.0);
        assert!(f.flat());
        assert_eq!(f.angle_deg(), Some(held));
        // 3.2 m/s² in-plane: above FLAT but not above UNFLAT, so still flat.
        f.update(-3.2, 0.0, 10.0);
        assert!(f.flat());
        assert_eq!(f.angle_deg(), Some(held));
        f.update(-9.0, 0.0, 10.0);
        assert!(!f.flat());
        assert_eq!(f.quadrant(), Some(Quadrant::LandscapeCcw));
    }

    #[test]
    fn a_fresh_filter_that_starts_flat_knows_no_angle() {
        let mut f = Filter::default();
        f.update(0.1, 0.1, 0.0);
        assert!(f.flat());
        assert_eq!(f.angle_deg(), None);
        assert_eq!(f.quadrant(), None);
    }

    #[test]
    fn a_rotated_mount_matrix_is_applied() {
        let m = parse_mount_matrix("0, -1, 0; 1, 0, 0; 0, 0, 1").unwrap();
        let g = to_ms2([0, 500, 0], 0.02, &m);
        assert!(close(g[0], -10.0, 1e-9) && close(g[1], 0.0, 1e-9));
        assert_eq!(parse_mount_matrix("1, 0; 0, 1"), None);
        assert_eq!(parse_mount_matrix("garbage"), None);
    }
}
