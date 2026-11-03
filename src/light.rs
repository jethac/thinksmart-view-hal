//! Ambient light and proximity, from the Vishay VCNL4200 above the panel.
//!
//! The sensor is at `0x51` on the bit-banged sensor bus and is read through the
//! mainline `vcnl4000` IIO driver (its IIO name is the driver's, `vcnl4000`).
//! Lux comes from `in_illuminance_raw` times `in_illuminance_scale`; proximity
//! is the raw PS count (the driver reports `in_proximity_raw` as counts / 16).
//!
//! Quirks, and where each comes from (`lenovo-thinksmart-view`,
//! `devices/proximity-light/ANALYSIS.md`, `streams/screen-wake/NOTES.md`):
//!
//! * **The IR LED only fires for a proximity read.** The project's kernel runs
//!   the sensor's proximity side in active force mode (patch 0601): a read
//!   triggers one measurement, one burst of the LED, and nothing else does --
//!   not an ambient light read, not the sensor being powered, not this process
//!   crashing. Upstream 7.3 runs it continuously at a 1/160 duty whenever the
//!   chip is powered, which includes every ambient light read. [`Light::open`]
//!   tells the two apart by timing (see [`check_on_demand`]) and refuses the
//!   sensor -- both channels -- under a driver that would leave the LED running.
//! * **LED_I does not set the LED current on this board.** The IRED is driven
//!   through an external FET whose resistor sets the current (datasheet
//!   Fig. 11); LED_I only drives the FET's gate. Measured: crosstalk counts are
//!   the same at 50, 120 and 200 mA, and 8x with 8 pulses. LED_I is set to the
//!   lowest step, 50 mA, the datasheet's value for driving the FET. What sets
//!   the light emitted is the integration time (pulse width) times pulses times
//!   reads: here 4T (384 us by the driver's table), one pulse, two reads a
//!   second while the screen is blanked and none while it is on, about the LED
//!   duty of the previous port's 9T at 1/1280 in normal mode.
//! * **Settings are written before the first read.** The driver's probe leaves
//!   8 pulses and 120 mA in the registers; in active force mode they do nothing
//!   until a read, so [`Light::open`] sets 1 pulse, 50 mA and 4T first.
//! * **Each proximity read takes about 25 ms** (a 20 ms wait for the triggered
//!   conversion, measured at 9-10 ms, plus three transfers on the bit-banged
//!   bus) and about 3 ms of CPU. Each light read can wait up to one integration
//!   time (100 ms here) after the sensor wakes. Both block: call them from a
//!   thread of their own.
//! * **Baseline.** With nothing in front, this unit reads 0-2 counts at 4T and
//!   one pulse (crosstalk through the cover glass). The previous port saw 87 to
//!   1044 for a hand at 9T on its unit; a threshold of a few tens of counts
//!   over the baseline is the starting point, not a calibration.
//! * **Dark ink.** In a dim room the light channel reads single counts at
//!   100 ms (0.012 lx per count), so brightness curves should treat anything
//!   under about 1 lx as dark.

use crate::util::{entries, now_ms, read_trim, write_attr};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const IIO: &str = "/sys/bus/iio/devices";
/// The IIO name the `vcnl4000` driver gives every chip it handles.
pub const NAME: &str = "vcnl4000";

/// Proximity settings written at open, as the driver's sysfs spells them.
pub const PS_PULSES: &str = "1";
pub const PS_LED_A: &str = "0.050000";
pub const PS_INTEGRATION_S: &str = "0.000384";
/// Light integration time: 0.012 lx per count, saturating at about 786 lx.
pub const ALS_INTEGRATION_S: &str = "0.100000";
/// `in_proximity_raw` is counts / 16 (16-bit PS output, the driver's `ps_scale`).
const PS_RAW_DIVISOR: f64 = 16.0;

/// Fastest a proximity read may return for the driver to count as triggering
/// one measurement per read: 20 ms wait plus transfers in active force mode,
/// under 8 ms with the upstream driver at 1T. See [`check_on_demand`].
const ON_DEMAND_MIN_US: u64 = 15_000;

#[derive(Clone, Default, Serialize)]
pub struct State {
    /// The IIO device directory.
    pub path: Option<String>,
    /// Ambient light from the last light read, lux.
    pub lux: Option<f64>,
    pub als_raw: Option<u32>,
    /// The last proximity read, sensor counts (16-bit).
    pub proximity: Option<u32>,
    /// What the driver reports back after `open` set them.
    pub ps_pulses: Option<u32>,
    pub ps_led_ua: Option<u32>,
    pub ps_integration_us: Option<u32>,
    /// True once the driver has been seen to measure proximity only on a read.
    pub on_demand: bool,
    /// How long the last read took, wall clock.
    pub last_read_us: Option<u64>,
    pub reads: u64,
    pub updated_ms: Option<u64>,
    pub error: Option<String>,
}

pub struct Light {
    dir: PathBuf,
    als_scale: f64,
    st: Mutex<State>,
}

/// The sensor's IIO directory, if the driver is bound.
pub fn find() -> Option<PathBuf> {
    entries(IIO)
        .into_iter()
        .find(|d| read_trim(d.join("name")).as_deref() == Some(NAME) && d.join("in_proximity_raw").exists())
}

pub fn present() -> bool {
    find().is_some()
}

fn read_f64(p: &Path) -> Result<f64, String> {
    read_trim(p).ok_or_else(|| format!("{}: unreadable", p.display()))?.parse().map_err(|e| format!("{}: {e}", p.display()))
}

impl Light {
    /// Find the sensor, write the proximity and light settings, and check that
    /// the driver measures proximity only when asked. Fails, without leaving the
    /// sensor in use, if any of that does not hold.
    pub fn open() -> Result<Arc<Self>, String> {
        let dir = find().ok_or("no vcnl4000 IIO device (is the driver loaded and the node enabled?)")?;
        let attr = |n: &str| dir.join(n);
        // The check runs at 1T; the settings proper go in afterwards.
        write_attr(attr("in_proximity_oversampling_ratio"), PS_PULSES)?;
        write_attr(attr("in_proximity_calibbias"), PS_LED_A)?;
        write_attr(attr("in_proximity_integration_time"), "0.000096")?;
        write_attr(attr("in_illuminance_integration_time"), ALS_INTEGRATION_S)?;
        let raw = attr("in_proximity_raw");
        let fastest = (0..3)
            .map(|_| {
                let t = Instant::now();
                read_f64(&raw).map(|_| t.elapsed().as_micros() as u64)
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .min()
            .unwrap_or(0);
        if !check_on_demand(fastest) {
            return Err(format!(
                "{}: a proximity read returned in {fastest} us: this driver measures continuously \
                 (IR LED running whenever the sensor is powered), not on demand; not using the sensor",
                dir.display()
            ));
        }
        write_attr(attr("in_proximity_integration_time"), PS_INTEGRATION_S)?;
        let als_scale = read_f64(&attr("in_illuminance_scale"))?;
        let micro = |n: &str| read_f64(&attr(n)).ok().map(|v| (v * 1e6).round() as u32);
        let st = State {
            path: Some(dir.display().to_string()),
            ps_pulses: read_trim(attr("in_proximity_oversampling_ratio")).and_then(|v| v.parse().ok()),
            ps_led_ua: micro("in_proximity_calibbias"),
            ps_integration_us: micro("in_proximity_integration_time"),
            on_demand: true,
            updated_ms: Some(now_ms()),
            ..Default::default()
        };
        Ok(Arc::new(Self { dir, als_scale, st: Mutex::new(st) }))
    }

    pub fn state(&self) -> State {
        self.st.lock().unwrap().clone()
    }

    /// One proximity measurement (one burst of the IR LED), in sensor counts.
    pub fn proximity(&self) -> Result<u32, String> {
        let t = Instant::now();
        let r = read_f64(&self.dir.join("in_proximity_raw")).map(|v| (v * PS_RAW_DIVISOR).round() as u32);
        self.record(t, &r, |st, v| st.proximity = Some(*v));
        r
    }

    /// Ambient light, lux. Does not light the IR LED.
    pub fn lux(&self) -> Result<f64, String> {
        let t = Instant::now();
        let r = read_f64(&self.dir.join("in_illuminance_raw")).map(|raw| (raw, raw * self.als_scale));
        self.record(t, &r, |st, (raw, lux)| {
            st.als_raw = Some(*raw as u32);
            st.lux = Some(*lux);
        });
        r.map(|(_, lux)| lux)
    }

    fn record<T>(&self, t: Instant, r: &Result<T, String>, ok: impl FnOnce(&mut State, &T)) {
        let mut st = self.st.lock().unwrap();
        st.last_read_us = Some(t.elapsed().as_micros() as u64);
        st.reads += 1;
        st.updated_ms = Some(now_ms());
        match r {
            Ok(v) => {
                ok(&mut st, v);
                st.error = None;
            }
            Err(e) => st.error = Some(e.clone()),
        }
    }
}

/// Whether the fastest of a few back-to-back proximity reads at 1T was slow
/// enough to be the project's driver. In active force mode every read writes
/// the trigger and waits 20 ms for the result. The upstream driver, which runs
/// the LED continuously, waits only until one measurement period (5.76 ms at
/// 1T) has passed since the previous read.
pub fn check_on_demand(fastest_read_us: u64) -> bool {
    fastest_read_us >= ON_DEMAND_MIN_US
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_timing_check_separates_the_two_drivers() {
        // Patched, measured on the tablet: 30 ms a read including a shell.
        assert!(check_on_demand(23_000));
        // Upstream at 1T: one 5.76 ms period plus a transfer.
        assert!(!check_on_demand(7_500));
        assert!(!check_on_demand(0));
    }
}
