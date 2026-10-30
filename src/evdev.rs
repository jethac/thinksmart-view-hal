//! The minimum of evdev that the switches and buttons need, in plain `ioctl(2)`
//! and `read(2)` on `/dev/input/event*`.
//!
//! No libevdev: it is a C library, and this crate is built on a laptop and copied
//! to a tablet with no compiler. What is needed is small -- find a device by its
//! kernel name, ask for the current key and switch bitmaps (`EVIOCGKEY`,
//! `EVIOCGSW`), and read `struct input_event` records -- so it is written out
//! here against the kernel's UAPI (`include/uapi/linux/input.h`).
//!
//! Devices are found by name, never by number. On the 7.3 image the numbering is
//! `event0` gpio-keys, `event1` touch, `event2` `pm8941_resin`, but that is probe
//! order and the touch controller probes late on some boots.
//!
//! The nodes are `root:input 0660`. A process that is neither root nor in the
//! `input` group sees the devices in sysfs and fails to open them, which is
//! reported as an error rather than as "no buttons".

use crate::util::{entries, name_of, read_trim};
use serde::Serialize;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_SW: u16 = 0x05;

pub const KEY_VOLUMEDOWN: u16 = 114;
pub const KEY_VOLUMEUP: u16 = 115;
pub const KEY_POWER: u16 = 116;

pub const SW_MICROPHONE_INSERT: u16 = 0x04;
pub const SW_CAMERA_LENS_COVER: u16 = 0x09;

const KEY_MAX: usize = 0x2ff;
const SW_MAX: usize = 0x10;

/// `_IOC(_IOC_READ, 'E', nr, len)` as the asm-generic layout used on arm64.
const fn eviocg(nr: u32, len: usize) -> u32 {
    (2 << 30) | ((len as u32) << 16) | ((b'E' as u32) << 8) | nr
}

const SYSFS_INPUT: &str = "/sys/class/input";

/// An input device as sysfs describes it, before anything is opened.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Device {
    /// The kernel's name for it: `gpio-keys`, `pm8941_resin`.
    pub name: String,
    /// `/dev/input/eventN`.
    pub node: String,
    /// The `capabilities/key` and `capabilities/sw` bitmaps from sysfs, as the
    /// key and switch codes they contain.
    pub keys: Vec<u16>,
    pub switches: Vec<u16>,
}

/// Every event device, from `/sys/class/input/event*`.
pub fn devices() -> Vec<Device> {
    devices_in(Path::new(SYSFS_INPUT))
}

fn devices_in(root: &Path) -> Vec<Device> {
    entries(root)
        .into_iter()
        .filter(|p| name_of(p).starts_with("event"))
        .filter_map(|p| {
            let dev = p.join("device");
            Some(Device {
                name: read_trim(dev.join("name"))?,
                node: format!("/dev/input/{}", name_of(&p)),
                keys: parse_bitmap(&read_trim(dev.join("capabilities/key")).unwrap_or_default()),
                switches: parse_bitmap(&read_trim(dev.join("capabilities/sw")).unwrap_or_default()),
            })
        })
        .collect()
}

/// The first event device whose name satisfies `pred`.
pub fn find(pred: impl Fn(&Device) -> bool) -> Option<Device> {
    devices().into_iter().find(|d| pred(d))
}

/// A sysfs capability bitmap: space-separated hex words, most significant word
/// first, each `BITS_PER_LONG` wide (64 on arm64). `8000000000000 0` is bit 115.
pub fn parse_bitmap(text: &str) -> Vec<u16> {
    let words: Vec<u64> = text
        .split_whitespace()
        .filter_map(|w| u64::from_str_radix(w, 16).ok())
        .collect();
    let mut bits = Vec::new();
    for (i, w) in words.iter().rev().enumerate() {
        for b in 0..64 {
            if w & (1u64 << b) != 0 {
                bits.push((i * 64 + b) as u16);
            }
        }
    }
    bits
}

fn bit(map: &[u8], code: u16) -> bool {
    map.get(code as usize / 8).is_some_and(|b| b & (1 << (code % 8)) != 0)
}

pub fn open(node: &str) -> Result<File, String> {
    File::open(node).map_err(|e| match e.kind() {
        ErrorKind::PermissionDenied => format!("{node}: permission denied (needs root or the input group)"),
        _ => format!("{node}: {e}"),
    })
}

fn ioctl_bitmap(f: &File, nr: u32, len: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; len];
    // The request type is c_ulong on glibc and c_int on musl; `as _` fits both.
    let r = unsafe { libc::ioctl(f.as_raw_fd(), eviocg(nr, len) as _, buf.as_mut_ptr()) };
    if r < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(buf)
}

/// Whether each of `codes` is currently held down (`EVIOCGKEY`).
pub fn keys_down(f: &File, codes: &[u16]) -> Result<Vec<bool>, String> {
    let map = ioctl_bitmap(f, 0x18, KEY_MAX / 8 + 1)?;
    Ok(codes.iter().map(|&c| bit(&map, c)).collect())
}

/// Whether each of `codes` is currently on (`EVIOCGSW`).
pub fn switches_on(f: &File, codes: &[u16]) -> Result<Vec<bool>, String> {
    let map = ioctl_bitmap(f, 0x1b, SW_MAX / 8 + 1)?;
    Ok(codes.iter().map(|&c| bit(&map, c)).collect())
}

/// One `struct input_event`, with the device it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct RawEvent {
    pub device: String,
    pub kind: u16,
    pub code: u16,
    pub value: i32,
    /// The kernel's timestamp for the event, in the same epoch milliseconds as
    /// `util::now_ms` (evdev stamps with CLOCK_REALTIME unless told otherwise).
    pub updated_ms: u64,
}

const EVENT_SIZE: usize = std::mem::size_of::<libc::input_event>();

/// A blocking stream of events from one or more devices, merged with `poll(2)`.
pub struct Stream {
    files: Vec<(String, File)>,
    pending: std::collections::VecDeque<RawEvent>,
}

impl Stream {
    pub fn open(devices: &[Device]) -> Result<Self, String> {
        if devices.is_empty() {
            return Err("no input devices to read".into());
        }
        let files = devices
            .iter()
            .map(|d| Ok((d.name.clone(), open(&d.node)?)))
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Self { files, pending: Default::default() })
    }

    /// The device names this stream is reading, for a caller that wants to say so.
    pub fn sources(&self) -> Vec<String> {
        self.files.iter().map(|(n, _)| n.clone()).collect()
    }

    fn fill(&mut self) -> Result<(), String> {
        let mut fds: Vec<libc::pollfd> = self
            .files
            .iter()
            .map(|(_, f)| libc::pollfd { fd: f.as_raw_fd(), events: libc::POLLIN, revents: 0 })
            .collect();
        loop {
            let r = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
            if r >= 0 {
                break;
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != ErrorKind::Interrupted {
                return Err(format!("poll: {e}"));
            }
        }
        for (i, pfd) in fds.iter().enumerate() {
            if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(format!("{}: device went away", self.files[i].0));
            }
            if pfd.revents & libc::POLLIN == 0 {
                continue;
            }
            let (name, file) = &mut self.files[i];
            let mut buf = [0u8; EVENT_SIZE * 16];
            let n = match file.read(&mut buf) {
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("{name}: {e}")),
            };
            for chunk in buf[..n].as_chunks::<EVENT_SIZE>().0 {
                // SAFETY: input_event is plain old data and the chunk is exactly
                // its size; read_unaligned copes with the byte buffer's alignment.
                let ev: libc::input_event = unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast()) };
                let updated_ms = ev.time.tv_sec as u64 * 1000 + ev.time.tv_usec as u64 / 1000;
                self.pending.push_back(RawEvent {
                    device: name.clone(),
                    kind: ev.type_,
                    code: ev.code,
                    value: ev.value,
                    updated_ms,
                });
            }
        }
        Ok(())
    }
}

impl Iterator for Stream {
    type Item = Result<RawEvent, String>;

    /// Blocks until an event arrives. `EV_SYN` frames are dropped: every device
    /// here reports one key or switch per frame, so the frame boundary carries
    /// nothing.
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            while let Some(ev) = self.pending.pop_front() {
                if ev.kind != EV_SYN {
                    return Some(Ok(ev));
                }
            }
            if let Err(e) = self.fill() {
                return Some(Err(e));
            }
        }
    }
}

/// Where a device lives in sysfs, for inventory.
pub fn sysfs_path(node: &str) -> Option<PathBuf> {
    let ev = Path::new(node).file_name()?;
    std::fs::canonicalize(Path::new(SYSFS_INPUT).join(ev).join("device")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capability lines from this unit's `/proc/bus/input/devices`
    /// (tests/fixtures/proc-bus-input-devices.txt).
    #[test]
    fn decodes_the_capability_bitmaps_from_the_device() {
        assert_eq!(parse_bitmap("8000000000000 0"), vec![KEY_VOLUMEUP]);
        assert_eq!(parse_bitmap("4000000000000 0"), vec![KEY_VOLUMEDOWN]);
        assert_eq!(parse_bitmap("210"), vec![SW_MICROPHONE_INSERT, SW_CAMERA_LENS_COVER]);
        assert_eq!(parse_bitmap(""), Vec::<u16>::new());
    }

    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        // EVIOCGKEY(96) and EVIOCGSW(3) as the C macros compute them on arm64.
        assert_eq!(eviocg(0x18, 96), 0x8060_4518);
        assert_eq!(eviocg(0x1b, 3), 0x8003_451b);
    }

    #[test]
    fn reads_bits_little_endian_within_bytes() {
        let mut map = vec![0u8; 16];
        map[14] = 0b0000_1000; // bit 115
        assert!(bit(&map, KEY_VOLUMEUP));
        assert!(!bit(&map, KEY_VOLUMEDOWN));
        assert!(!bit(&map, 0x2ff));
    }

    #[test]
    fn finds_devices_by_name_in_a_sysfs_tree() {
        let root = std::env::temp_dir().join(format!("view-hal-input-{}", std::process::id()));
        for (ev, name, key, sw) in [
            ("event0", "gpio-keys", "8000000000000 0", "210"),
            ("event2", "pm8941_resin", "4000000000000 0", "0"),
        ] {
            let d = root.join(ev).join("device/capabilities");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(root.join(ev).join("device/name"), format!("{name}\n")).unwrap();
            std::fs::write(d.join("key"), key).unwrap();
            std::fs::write(d.join("sw"), sw).unwrap();
        }
        let found = devices_in(&root);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "gpio-keys");
        assert_eq!(found[0].node, "/dev/input/event0");
        assert_eq!(found[0].switches, vec![SW_MICROPHONE_INSERT, SW_CAMERA_LENS_COVER]);
        assert_eq!(found[1].keys, vec![KEY_VOLUMEDOWN]);
    }
}
