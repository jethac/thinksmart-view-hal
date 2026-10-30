//! The USB-C port as a gadget: which functions are configured, whether a host
//! has enumerated it, and the address on `usb0`.
//!
//! Quirks, and where each comes from:
//!
//! * **Device mode only, and no CC.** The port is DWC3 at `7000000`, USB 2.0
//!   high speed. The board presents no Rd on CC: stock's Type-C driver read the
//!   lines as OPEN with a cable attached and never saw a detect interrupt
//!   (`devices/usb/ANALYSIS.md` §4). A host has to supply VBUS unconditionally,
//!   so it works from a USB-A port (or a powered hub) through an A-to-C cable,
//!   and not from a Type-C host port that waits for CC. Role comes from straps
//!   and `role-switch-default-mode = "peripheral"`; host mode is unproven (no
//!   battery to boost VBUS from).
//! * **One gadget, built by the image.** `tsv-usb-gadget` builds configfs gadget
//!   `tsv` (`1d6b:0104`) with NCM (`ncm.usb0`), ACM (`acm.GS0`) and adb
//!   (`ffs.adb`, linked only while adbd is ready), and sets `172.16.42.1/24` on
//!   `usb0` (`rootfs/README.md`, "USB networking"). This module only reads it.
//! * **adb comes and goes.** The kernel unbinds the whole gadget whenever adbd
//!   closes FunctionFS, which it does on every host disconnect; the supervisor
//!   rebinds within seconds. A momentary `udc: None` is that, not a fault.
//! * **Host connected** is the UDC's `state` reading `configured`: a host has
//!   enumerated the device and chosen a configuration. VBUS alone is not visible
//!   here.
//! * **Not reported:** the USB serial number and the NCM MAC addresses, which are
//!   derived from the SoC serial number, and IPv6 link-local addresses, which
//!   embed the MAC.

use crate::util::{entries, name_of, now_ms, read_trim};
use serde::Serialize;
use std::ffi::CStr;
use std::path::Path;
use std::sync::{Arc, Mutex};

const GADGETS: &str = "/sys/kernel/config/usb_gadget";
const UDC_CLASS: &str = "/sys/class/udc";

#[derive(Clone, Default, Serialize)]
pub struct State {
    /// The configfs gadget, e.g. `tsv`.
    pub gadget: Option<String>,
    /// `idVendor:idProduct`, e.g. `1d6b:0104`.
    pub id: Option<String>,
    /// Functions defined under `functions/`, e.g. `acm.GS0`, `ffs.adb`, `ncm.usb0`.
    pub functions: Vec<String>,
    /// Functions linked into a configuration, i.e. offered to the host.
    pub linked: Vec<String>,
    pub ncm: bool,
    pub acm: bool,
    pub adb: bool,
    /// The UDC the gadget is bound to, e.g. `7000000.usb`. None when unbound.
    pub udc: Option<String>,
    /// The UDC's `state`: `configured`, `not attached`, `suspended`...
    pub udc_state: Option<String>,
    pub speed: Option<String>,
    /// A host has enumerated and configured the gadget. None with no UDC bound.
    pub host_connected: Option<bool>,
    /// The NCM network interface, from `functions/ncm.*/ifname`.
    pub interface: Option<String>,
    pub operstate: Option<String>,
    /// IPv4 addresses on that interface, `a.b.c.d/prefix`.
    pub addresses: Vec<String>,
    pub updated_ms: Option<u64>,
    pub error: Option<String>,
}

pub struct Usb {
    st: Mutex<State>,
}

impl Usb {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { st: Mutex::new(State::default()) })
    }

    pub fn state(&self) -> State {
        self.st.lock().unwrap().clone()
    }

    pub fn refresh(&self) {
        let mut next = read(Path::new(GADGETS), Path::new(UDC_CLASS));
        if let Some(ifname) = &next.interface {
            next.operstate = read_trim(format!("/sys/class/net/{ifname}/operstate"));
            next.addresses = ipv4_addresses(ifname);
        }
        next.updated_ms = Some(now_ms());
        *self.st.lock().unwrap() = next;
    }
}

/// Everything that comes from configfs and the UDC class, separately from the
/// network side so it can be tested against a directory tree.
fn read(gadgets: &Path, udc_class: &Path) -> State {
    let mut st = State::default();
    let all = entries(gadgets);
    // Prefer a bound gadget; a stale unbound one may linger beside it.
    let Some(g) = all
        .iter()
        .find(|g| read_trim(g.join("UDC")).is_some_and(|u| !u.is_empty()))
        .or(all.first())
    else {
        st.error = Some(if gadgets.exists() {
            format!("no gadget under {}", gadgets.display())
        } else {
            format!("{} missing (configfs not mounted?)", gadgets.display())
        });
        return st;
    };
    st.gadget = Some(name_of(g));
    let hex = |f: &str| read_trim(g.join(f)).map(|s| s.trim_start_matches("0x").to_string());
    st.id = hex("idVendor").zip(hex("idProduct")).map(|(v, p)| format!("{v}:{p}"));
    st.functions = entries(g.join("functions")).iter().map(|p| name_of(p)).collect();
    let mut linked: Vec<String> = entries(g.join("configs"))
        .iter()
        .flat_map(entries)
        .filter(|p| p.is_symlink())
        .map(|p| name_of(&p))
        .collect();
    linked.sort();
    linked.dedup();
    st.ncm = linked.iter().any(|f| f.starts_with("ncm."));
    st.acm = linked.iter().any(|f| f.starts_with("acm."));
    st.adb = linked.iter().any(|f| f.starts_with("ffs.adb"));
    st.linked = linked;
    st.interface = st
        .functions
        .iter()
        .find(|f| f.starts_with("ncm."))
        .and_then(|f| read_trim(g.join("functions").join(f).join("ifname")));
    st.udc = read_trim(g.join("UDC")).filter(|u| !u.is_empty());
    if let Some(udc) = &st.udc {
        let dir = udc_class.join(udc);
        st.udc_state = read_trim(dir.join("state"));
        st.speed = read_trim(dir.join("current_speed"));
        st.host_connected = st.udc_state.as_deref().map(|s| s == "configured");
    }
    st
}

/// IPv4 addresses of `ifname` from getifaddrs(3): no `ip` child process, and
/// nothing that can block.
fn ipv4_addresses(ifname: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return out;
    }
    let mut cur = head;
    while !cur.is_null() {
        // SAFETY: getifaddrs returned a valid list, freed below.
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        if ifa.ifa_addr.is_null() || ifa.ifa_name.is_null() {
            continue;
        }
        let name = unsafe { CStr::from_ptr(ifa.ifa_name) }.to_string_lossy();
        if name != ifname || unsafe { (*ifa.ifa_addr).sa_family } as i32 != libc::AF_INET {
            continue;
        }
        let addr = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
        let ip = std::net::Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
        let prefix = if ifa.ifa_netmask.is_null() {
            32
        } else {
            let m = unsafe { &*(ifa.ifa_netmask as *const libc::sockaddr_in) };
            u32::from_be(m.sin_addr.s_addr).count_ones()
        };
        out.push(format!("{ip}/{prefix}"));
    }
    unsafe { libc::freeifaddrs(head) };
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    /// The tree on the 7.3 image (tests/fixtures/usb-gadget-tree.txt), rebuilt
    /// in a temporary directory.
    fn fake_tree(bound: bool) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("view-hal-usb-{}-{bound}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let g = root.join("gadgets/tsv");
        for f in ["acm.GS0", "ffs.adb", "ncm.usb0"] {
            fs::create_dir_all(g.join("functions").join(f)).unwrap();
        }
        fs::write(g.join("functions/ncm.usb0/ifname"), "usb0\n").unwrap();
        fs::create_dir_all(g.join("configs/c.1")).unwrap();
        for f in ["acm.GS0", "ffs.adb", "ncm.usb0"] {
            symlink(g.join("functions").join(f), g.join("configs/c.1").join(f)).unwrap();
        }
        fs::write(g.join("configs/c.1/MaxPower"), "2\n").unwrap();
        fs::write(g.join("idVendor"), "0x1d6b\n").unwrap();
        fs::write(g.join("idProduct"), "0x0104\n").unwrap();
        fs::write(g.join("UDC"), if bound { "7000000.usb\n" } else { "\n" }).unwrap();
        let u = root.join("udc/7000000.usb");
        fs::create_dir_all(&u).unwrap();
        fs::write(u.join("state"), "configured\n").unwrap();
        fs::write(u.join("current_speed"), "high-speed\n").unwrap();
        root
    }

    #[test]
    fn reads_the_images_gadget() {
        let root = fake_tree(true);
        let st = read(&root.join("gadgets"), &root.join("udc"));
        let _ = fs::remove_dir_all(&root);
        assert_eq!(st.gadget.as_deref(), Some("tsv"));
        assert_eq!(st.id.as_deref(), Some("1d6b:0104"));
        assert_eq!(st.linked, vec!["acm.GS0", "ffs.adb", "ncm.usb0"]);
        assert!(st.ncm && st.acm && st.adb);
        assert_eq!(st.udc.as_deref(), Some("7000000.usb"));
        assert_eq!(st.host_connected, Some(true));
        assert_eq!(st.speed.as_deref(), Some("high-speed"));
        assert_eq!(st.interface.as_deref(), Some("usb0"));
    }

    #[test]
    fn an_unbound_gadget_has_no_host_answer() {
        let root = fake_tree(false);
        let st = read(&root.join("gadgets"), &root.join("udc"));
        let _ = fs::remove_dir_all(&root);
        assert_eq!(st.udc, None);
        assert_eq!(st.host_connected, None);
        assert!(st.ncm);
    }

    #[test]
    fn no_configfs_says_so() {
        let st = read(Path::new("/nonexistent/usb_gadget"), Path::new("/nonexistent"));
        assert!(st.error.unwrap().contains("configfs"));
    }

    #[test]
    fn loopback_has_an_address() {
        assert!(ipv4_addresses("lo").iter().any(|a| a == "127.0.0.1/8"));
    }
}
