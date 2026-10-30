//! Temperatures and CPU clocks: the thermal zones, and cpufreq where it exists.
//!
//! Quirks, and where each comes from:
//!
//! * **Zones are TSENS plus the PMIC.** The 7.3 image has ten:
//!   `cpu0-thermal`..`cpu7-thermal` (one TSENS sensor per core), `gpu-thermal`
//!   and `pm8953-thermal`, all in millidegrees. Zones are reported by `type`,
//!   never by number.
//! * **Trips are read, not assumed.** The analysis of the 7.3 tree found 80 °C
//!   passive and 100 °C critical (`devices/cpufreq-thermal/ANALYSIS.md`), but the
//!   baseline kernel on the device on 2026-10-30 had CPU passive trips at 50 and
//!   56 °C (critical 100), GPU 70/80 (critical 90), and the CPUs idled at
//!   47-51 °C. So `policies[].max_mhz` read 1612.8 against a hardware maximum of
//!   1804.8: the cap was thermal. Stock's `thermal-engine.conf` is empty, so there
//!   is no stock policy to compare against.
//! * **cpufreq has been broken and may be again.** Early 7.3 boots logged
//!   `Failed to set OPP config` and sat at about 1.23 GHz with no policy
//!   (`CHIP.md`); the `cpufreq` workstream is fixing it, and its test kernels may
//!   be on the device at any time. Everything here is read as found: no policy
//!   directory gives an empty list, an unreadable attribute gives `None`, and
//!   nothing is assumed from the 652.8-1804.8 MHz table. On the current
//!   baseline all eight cores share one policy (`policy0`, `cpufreq-dt`,
//!   `schedutil`).
//! * **Clocks are in MHz with a fraction** (sysfs gives kHz; 1804800 kHz is
//!   1804.8 MHz), so the `_mhz` fields are floating point.

use crate::util::{entries, name_of, now_ms, read_i64, read_trim};
use serde::Serialize;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const ZONES: &str = "/sys/class/thermal";
const CPUS: &str = "/sys/devices/system/cpu";

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Trip {
    /// `passive`, `hot`, `critical`...
    #[serde(rename = "type")]
    pub kind: String,
    pub temp_mdegc: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Zone {
    /// `thermal_zone4`. Probe order; use `type`.
    pub zone: String,
    /// `cpu0-thermal`, `gpu-thermal`, `pm8953-thermal`.
    #[serde(rename = "type")]
    pub kind: String,
    pub temp_mdegc: Option<i64>,
    pub enabled: Option<bool>,
    pub trips: Vec<Trip>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Policy {
    /// `policy0`.
    pub policy: String,
    pub cpus: Vec<u32>,
    pub driver: Option<String>,
    pub governor: Option<String>,
    pub cur_mhz: Option<f64>,
    pub min_mhz: Option<f64>,
    pub max_mhz: Option<f64>,
    /// The hardware range, before governor limits.
    pub hw_min_mhz: Option<f64>,
    pub hw_max_mhz: Option<f64>,
    pub available_mhz: Vec<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Cpu {
    pub cpu: u32,
    pub online: Option<bool>,
    /// None when the core has no cpufreq policy.
    pub cur_mhz: Option<f64>,
}

#[derive(Clone, Default, Serialize)]
pub struct State {
    pub zones: Vec<Zone>,
    /// The hottest zone, for a one-number display.
    pub max_temp_mdegc: Option<i64>,
    pub policies: Vec<Policy>,
    pub cpus: Vec<Cpu>,
    pub updated_ms: Option<u64>,
    pub error: Option<String>,
}

pub struct Thermal {
    st: Mutex<State>,
}

impl Thermal {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { st: Mutex::new(State::default()) })
    }

    pub fn start(self: &Arc<Self>, period: Duration) {
        let me = self.clone();
        thread::spawn(move || loop {
            me.refresh();
            thread::sleep(period);
        });
    }

    pub fn state(&self) -> State {
        self.st.lock().unwrap().clone()
    }

    pub fn refresh(&self) {
        let next = read(Path::new(ZONES), Path::new(CPUS));
        *self.st.lock().unwrap() = next;
    }
}

fn khz_to_mhz(v: i64) -> f64 {
    v as f64 / 1000.0
}

fn mhz(p: &Path) -> Option<f64> {
    read_i64(p).map(khz_to_mhz)
}

/// `0-3,6` -> [0, 1, 2, 3, 6]; also accepts `affected_cpus`' `0 1 2 3`.
pub fn parse_cpu_list(text: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for part in text.split([',', ' ']).map(str::trim).filter(|s| !s.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
                    out.extend(a..=b);
                }
            }
            None => out.extend(part.parse::<u32>().ok()),
        }
    }
    out
}

fn read(zones: &Path, cpus: &Path) -> State {
    let mut st = State { updated_ms: Some(now_ms()), ..Default::default() };
    st.zones = entries(zones)
        .into_iter()
        .filter(|p| name_of(p).starts_with("thermal_zone"))
        .filter_map(|p| {
            let kind = read_trim(p.join("type"))?;
            let mut trips = Vec::new();
            for i in 0.. {
                let Some(k) = read_trim(p.join(format!("trip_point_{i}_type"))) else { break };
                trips.push(Trip { kind: k, temp_mdegc: read_i64(p.join(format!("trip_point_{i}_temp"))) });
            }
            Some(Zone {
                zone: name_of(&p),
                kind,
                temp_mdegc: read_i64(p.join("temp")),
                enabled: read_trim(p.join("mode")).map(|m| m == "enabled"),
                trips,
            })
        })
        .collect();
    // thermal_zone10 sorts before thermal_zone2 as text.
    st.zones.sort_by_key(|z| z.zone.trim_start_matches("thermal_zone").parse::<u32>().unwrap_or(u32::MAX));
    st.max_temp_mdegc = st.zones.iter().filter_map(|z| z.temp_mdegc).max();

    st.policies = entries(cpus.join("cpufreq"))
        .into_iter()
        .filter(|p| name_of(p).starts_with("policy"))
        .map(|p| Policy {
            policy: name_of(&p),
            cpus: read_trim(p.join("related_cpus")).map(|s| parse_cpu_list(&s)).unwrap_or_default(),
            driver: read_trim(p.join("scaling_driver")),
            governor: read_trim(p.join("scaling_governor")),
            cur_mhz: mhz(&p.join("scaling_cur_freq")),
            min_mhz: mhz(&p.join("scaling_min_freq")),
            max_mhz: mhz(&p.join("scaling_max_freq")),
            hw_min_mhz: mhz(&p.join("cpuinfo_min_freq")),
            hw_max_mhz: mhz(&p.join("cpuinfo_max_freq")),
            available_mhz: read_trim(p.join("scaling_available_frequencies"))
                .map(|s| s.split_whitespace().filter_map(|v| v.parse().ok()).map(khz_to_mhz).collect())
                .unwrap_or_default(),
        })
        .collect();

    let present = read_trim(cpus.join("present")).map(|s| parse_cpu_list(&s)).unwrap_or_default();
    let online = read_trim(cpus.join("online")).map(|s| parse_cpu_list(&s));
    st.cpus = present
        .into_iter()
        .map(|cpu| Cpu {
            cpu,
            online: online.as_ref().map(|o| o.contains(&cpu)),
            cur_mhz: st.policies.iter().find(|p| p.cpus.contains(&cpu)).and_then(|p| p.cur_mhz),
        })
        .collect();

    if st.zones.is_empty() {
        st.error = Some(format!("no thermal zones under {}", zones.display()));
    }
    st
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_cpu_lists() {
        assert_eq!(parse_cpu_list("0-7"), (0..8).collect::<Vec<_>>());
        assert_eq!(parse_cpu_list("0 1 2 3 4 5 6 7"), (0..8).collect::<Vec<_>>());
        assert_eq!(parse_cpu_list("0-3,6"), vec![0, 1, 2, 3, 6]);
        assert_eq!(parse_cpu_list(""), Vec::<u32>::new());
    }

    /// Values from this unit (tests/fixtures/view-hal-dump.json).
    fn fake(with_cpufreq: bool) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("view-hal-thermal-{}-{with_cpufreq}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for (i, kind, temp) in [(0, "cpu4-thermal", "35500"), (9, "pm8953-thermal", "32587"), (10, "x", "1")] {
            let z = root.join(format!("thermal/thermal_zone{i}"));
            fs::create_dir_all(&z).unwrap();
            fs::write(z.join("type"), kind).unwrap();
            fs::write(z.join("temp"), temp).unwrap();
            fs::write(z.join("mode"), "enabled").unwrap();
            fs::write(z.join("trip_point_0_type"), "passive").unwrap();
            fs::write(z.join("trip_point_0_temp"), "80000").unwrap();
        }
        let c = root.join("cpu");
        fs::create_dir_all(&c).unwrap();
        fs::write(c.join("present"), "0-7\n").unwrap();
        fs::write(c.join("online"), "0-7\n").unwrap();
        if with_cpufreq {
            let p = c.join("cpufreq/policy0");
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("related_cpus"), "0 1 2 3 4 5 6 7\n").unwrap();
            fs::write(p.join("scaling_cur_freq"), "1804800\n").unwrap();
            fs::write(p.join("scaling_available_frequencies"), "652800 844800 1804800 \n").unwrap();
        }
        root
    }

    #[test]
    fn reads_zones_and_one_policy_for_all_cores() {
        let root = fake(true);
        let st = read(&root.join("thermal"), &root.join("cpu"));
        let _ = fs::remove_dir_all(&root);
        assert_eq!(st.zones.len(), 3);
        assert_eq!(st.zones[1].kind, "pm8953-thermal");
        assert_eq!(st.zones[2].zone, "thermal_zone10");
        assert_eq!(st.zones[0].trips, vec![Trip { kind: "passive".into(), temp_mdegc: Some(80000) }]);
        assert_eq!(st.max_temp_mdegc, Some(35500));
        assert_eq!(st.policies[0].available_mhz, vec![652.8, 844.8, 1804.8]);
        assert_eq!(st.policies[0].governor, None);
        assert_eq!(st.cpus.len(), 8);
        assert!(st.cpus.iter().all(|c| c.cur_mhz == Some(1804.8)));
    }

    #[test]
    fn without_cpufreq_the_clocks_are_unknown_not_zero() {
        let root = fake(false);
        let st = read(&root.join("thermal"), &root.join("cpu"));
        let _ = fs::remove_dir_all(&root);
        assert!(st.policies.is_empty());
        assert_eq!(st.cpus.len(), 8);
        assert!(st.cpus.iter().all(|c| c.cur_mhz.is_none() && c.online == Some(true)));
    }
}
