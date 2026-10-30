//! A one-shot description of the hardware, from /sys and /proc. Cheap enough to
//! regenerate on demand; never polled.
//!
//! Every check here only looks for a device in sysfs. None of them reads the
//! accelerometer (a bus transfer each, see [`crate::orientation`]) or opens an
//! input node, so `collect` costs nothing on the hardware and works without
//! root. It deliberately leaves out the SoC serial number
//! (`/sys/devices/soc0/serial_number`) and anything derived from it -- the USB
//! serial string and the gadget's MAC addresses -- so its output can be pasted
//! into a bug report.

use crate::util::{entries, name_of, read_trim};
use crate::{display, evdev, orientation};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

pub fn collect() -> Value {
    let input = evdev::devices();
    let has_key = |name: &str, code: u16| input.iter().any(|d| d.name.contains(name) && d.keys.contains(&code));
    let panel = display::panel();
    let backlight = Path::new("/sys/class/backlight/backlight");
    let gadget = entries("/sys/kernel/config/usb_gadget").first().map(|p| name_of(p));
    let zones = entries("/sys/class/thermal").iter().filter(|p| name_of(p).starts_with("thermal_zone")).count();
    let cpufreq = entries("/sys/devices/system/cpu/cpufreq").iter().any(|p| name_of(p).starts_with("policy"));
    let touch = input.iter().find(|d| d.name.contains("ft5x06") || d.name.contains("fts"));

    let check = |id: &str, module: &str, label: &str, ok: bool, detail: String| {
        json!({"id": id, "module": module, "label": label, "ok": ok, "detail": detail})
    };
    let checks = vec![
        check(
            "panel",
            "display",
            "DSI panel",
            panel.as_ref().is_some_and(|p| p.internal),
            panel.as_ref().map(|p| format!("{} {}x{}", p.connector, p.width, p.height)).unwrap_or_default(),
        ),
        check(
            "backlight",
            "display",
            "WLED backlight",
            backlight.join("brightness").exists(),
            read_trim(backlight.join("max_brightness")).map(|m| format!("0..{m}")).unwrap_or_default(),
        ),
        check(
            "accelerometer",
            "orientation",
            "BMA253 accelerometer",
            orientation::present(),
            orientation::find().map(|p| p.display().to_string()).unwrap_or_default(),
        ),
        check(
            "lens-cover",
            "switches",
            "Camera lens cover",
            input.iter().any(|d| d.name == "gpio-keys" && d.switches.contains(&evdev::SW_CAMERA_LENS_COVER)),
            "gpio-keys SW_CAMERA_LENS_COVER".into(),
        ),
        check("volume-up", "buttons", "Volume Up", has_key("gpio-keys", evdev::KEY_VOLUMEUP), "gpio-keys GPIO 85".into()),
        check(
            "volume-down",
            "buttons",
            "Volume Down",
            has_key("resin", evdev::KEY_VOLUMEDOWN),
            input.iter().find(|d| d.name.contains("resin")).map(|d| d.name.clone()).unwrap_or_default(),
        ),
        check("usb-gadget", "usb", "USB gadget", gadget.is_some(), gadget.clone().unwrap_or_default()),
        check("thermal", "thermal", "Thermal zones", zones > 0, format!("{zones} zones")),
        check("cpufreq", "thermal", "CPU frequency scaling", cpufreq, String::new()),
        check(
            "touch",
            "",
            "Touch controller",
            touch.is_some(),
            touch.map(|d| d.name.clone()).unwrap_or_default(),
        ),
    ];

    json!({
        "system": system(),
        "soc": soc(),
        "cpu": cpu(),
        "panel": panel,
        "input": input,
        "iio": iio(),
        "checks": checks,
    })
}

fn system() -> Value {
    let dt = |f: &str| {
        fs::read(format!("/proc/device-tree/{f}")).ok().map(|b| {
            b.split(|c| *c == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).trim().to_string())
                .collect::<Vec<_>>()
        })
    };
    let mem_kb = fs::read_to_string("/proc/meminfo").ok().and_then(|m| {
        m.lines()
            .find_map(|l| l.strip_prefix("MemTotal:"))
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
    });
    let os = fs::read_to_string("/etc/os-release").ok().and_then(|s| {
        s.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))
    });
    let uptime = read_trim("/proc/uptime")
        .and_then(|u| u.split_whitespace().next()?.parse::<f64>().ok())
        .map(|s| s as u64);
    json!({
        "hostname": read_trim("/proc/sys/kernel/hostname"),
        "model": dt("model").and_then(|v| v.into_iter().next()),
        "compatible": dt("compatible"),
        "os": os,
        "kernel": read_trim("/proc/sys/kernel/osrelease"),
        "mem_kb": mem_kb,
        "mem_gb": mem_kb.map(|kb| (kb as f64 / 1_048_576.0 * 100.0).round() / 100.0),
        "uptime_s": uptime,
    })
}

/// `/sys/devices/soc0`, without `serial_number`.
fn soc() -> Value {
    let f = |n: &str| read_trim(format!("/sys/devices/soc0/{n}"));
    json!({
        "machine": f("machine"),
        "family": f("family"),
        "soc_id": f("soc_id").and_then(|s| s.parse::<u32>().ok()),
        "revision": f("revision"),
    })
}

/// What `/proc/cpuinfo` says about the cores. Parsed separately so it can be
/// tested against this unit's output.
fn cpu() -> Value {
    parse_cpuinfo(&fs::read_to_string("/proc/cpuinfo").unwrap_or_default())
}

pub(crate) fn parse_cpuinfo(text: &str) -> Value {
    let field = |key: &str| {
        text.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            (k.trim() == key).then(|| v.trim().to_string())
        })
    };
    let cores = text.lines().filter(|l| l.starts_with("processor")).count();
    let features: Vec<String> =
        field("Features").map(|f| f.split_whitespace().map(str::to_string).collect()).unwrap_or_default();
    let part = field("CPU part");
    let name = match (field("CPU implementer").as_deref(), part.as_deref()) {
        (Some("0x41"), Some("0xd03")) => Some("Cortex-A53"),
        _ => None,
    };
    json!({
        "cores": cores,
        "core": name,
        "implementer": field("CPU implementer"),
        "part": part,
        "revision": field("CPU revision").and_then(|r| r.parse::<u32>().ok()),
        "features": features,
        // The ones that matter for speech models: no dot product, no FP16 maths.
        "has_asimd": features.iter().any(|f| f == "asimd"),
        "has_dotprod": features.iter().any(|f| f == "asimddp"),
        "has_fp16": features.iter().any(|f| f == "fphp" || f == "asimdhp"),
    })
}

fn iio() -> Value {
    Value::Array(
        entries("/sys/bus/iio/devices")
            .iter()
            .map(|p| json!({"device": name_of(p), "name": read_trim(p.join("name"))}))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_this_units_cpuinfo() {
        let mut text = String::new();
        for i in 0..8 {
            text += &format!(
                "processor\t: {i}\nBogoMIPS\t: 38.40\nFeatures\t: fp asimd evtstrm aes pmull sha1 sha2 crc32 cpuid\n\
                 CPU implementer\t: 0x41\nCPU architecture: 8\nCPU variant\t: 0x0\nCPU part\t: 0xd03\nCPU revision\t: 4\n\n"
            );
        }
        // The first block must match what the device printed.
        let fixture = include_str!("../tests/fixtures/cpuinfo-head.txt");
        assert!(text.starts_with(fixture.split("\n\n").next().unwrap()));
        let v = parse_cpuinfo(&text);
        assert_eq!(v["cores"], 8);
        assert_eq!(v["core"], "Cortex-A53");
        assert_eq!(v["revision"], 4);
        assert_eq!(v["has_asimd"], true);
        assert_eq!(v["has_dotprod"], false);
        assert_eq!(v["has_fp16"], false);
        assert_eq!(v["features"].as_array().unwrap().len(), 9);
    }
}
