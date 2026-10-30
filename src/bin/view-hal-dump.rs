//! Print what `view-hal` sees, as JSON.
//!
//! ```text
//! view-hal-dump                        inventory and every module's state
//! view-hal-dump --events               stream button and switch events, one JSON object per line
//! view-hal-dump --set-brightness N     set the backlight (clamped to the floor), print display state
//! view-hal-dump --floor N              floor for --set-brightness (default 1400)
//! view-hal-dump --power on|off         switch the backlight through bl_power
//! ```
//!
//! The default mode only reads. It reads the accelerometer a handful of times
//! (about 3 ms of CPU each) so the filter has something to settle on.

use serde_json::json;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use view_hal::{buttons, display, inventory, orientation, switches, thermal, usb};

fn usage() -> ! {
    eprintln!("usage: view-hal-dump [--events | --set-brightness N [--floor N] | --power on|off]");
    std::process::exit(2)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut events = false;
    let mut brightness: Option<u32> = None;
    let mut floor: Option<u32> = None;
    let mut power: Option<bool> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--events" => events = true,
            "--set-brightness" => brightness = Some(it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage())),
            "--floor" => floor = Some(it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage())),
            "--power" => {
                power = Some(match it.next().map(String::as_str) {
                    Some("on") => true,
                    Some("off") => false,
                    _ => usage(),
                })
            }
            "-h" | "--help" => usage(),
            _ => usage(),
        }
    }

    if events {
        stream_events();
    }

    if brightness.is_some() || power.is_some() {
        let d = display::Display::new();
        if let Some(f) = floor {
            d.set_floor(f);
        }
        let result = match (brightness, power) {
            (Some(b), _) => d.set_brightness(b),
            (None, Some(p)) => d.set_power(p),
            _ => unreachable!(),
        };
        d.refresh();
        let out = json!({"ok": result.is_ok(), "error": result.err(), "display": d.state()});
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
        return;
    }

    let disp = display::Display::new();
    disp.refresh();
    let orient = orientation::Orientation::new();
    for _ in 0..5 {
        orient.poll();
        thread::sleep(Duration::from_millis(40));
    }
    let sw = switches::Switches::new();
    sw.refresh();
    let btn = buttons::Buttons::new();
    btn.refresh();
    let u = usb::Usb::new();
    u.refresh();
    let th = thermal::Thermal::new();
    th.refresh();

    let out = json!({
        "inventory": inventory::collect(),
        "display": disp.state(),
        "orientation": orient.state(),
        "switches": sw.state(),
        "buttons": btn.state(),
        "usb": u.state(),
        "thermal": th.state(),
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}

/// One line per event, from both iterators, until killed.
fn stream_events() -> ! {
    let (tx, rx) = mpsc::channel::<serde_json::Value>();
    let btn = buttons::Buttons::new();
    match btn.events() {
        Ok(evs) => {
            eprintln!("view-hal-dump: buttons: reading {}", evs.sources().join(", "));
            let tx = tx.clone();
            thread::spawn(move || {
                for e in evs {
                    let v = match e {
                        Ok(e) => json!({"buttons": e}),
                        Err(e) => json!({"buttons": {"error": e}}),
                    };
                    if tx.send(v).is_err() {
                        break;
                    }
                }
            });
        }
        Err(e) => eprintln!("view-hal-dump: buttons: {e}"),
    }
    let sw = switches::Switches::new();
    match sw.events() {
        Ok(evs) => {
            eprintln!("view-hal-dump: switches: reading {}", evs.sources().join(", "));
            let tx = tx.clone();
            thread::spawn(move || {
                for e in evs {
                    let v = match e {
                        Ok(e) => json!({"switches": e}),
                        Err(e) => json!({"switches": {"error": e}}),
                    };
                    if tx.send(v).is_err() {
                        break;
                    }
                }
            });
        }
        Err(e) => eprintln!("view-hal-dump: switches: {e}"),
    }
    drop(tx);
    for v in rx {
        println!("{v}");
    }
    eprintln!("view-hal-dump: no event source left");
    std::process::exit(1)
}
