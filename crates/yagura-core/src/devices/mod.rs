//! Devices: what Device Manager lists (USB, PCI and the rest of the
//! hardware), read on the sampler thread and only while the devices screen
//! shows.
//!
//! Each OS has its own source: SetupAPI and the configuration manager on
//! Windows, sysfs on Linux, the IOKit registry on macOS. The parsing (the
//! hardware ids, the class codes, the `pci.ids` / `usb.ids` tables, the tree)
//! is kept in pure functions so the tests run everywhere.

#[cfg(target_os = "macos")]
mod iokit;
// Its tests make symlinks, so they run on unix only.
#[cfg(any(target_os = "linux", all(test, unix)))]
pub mod linux;
#[cfg(any(target_os = "macos", test))]
pub mod mac;
#[cfg(windows)]
mod setupapi;
#[cfg(any(windows, test))]
pub mod windows;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// What a device is, for its class when the OS gives no class name of its
/// own and for the details.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Kind {
    /// Controllers and hubs.
    Usb,
    Display,
    Net,
    Audio,
    Storage,
    Input,
    Camera,
    Bluetooth,
    Monitor,
    Printer,
    Processor,
    Battery,
    /// Bridges, timers, the chipset's own parts.
    System,
    #[default]
    Other,
}

impl Kind {
    pub const ALL: [Kind; 14] = [
        Self::Usb,
        Self::Display,
        Self::Net,
        Self::Audio,
        Self::Storage,
        Self::Input,
        Self::Camera,
        Self::Bluetooth,
        Self::Monitor,
        Self::Printer,
        Self::Processor,
        Self::Battery,
        Self::System,
        Self::Other,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::Usb => "dev.kind.usb",
            Self::Display => "dev.kind.display",
            Self::Net => "dev.kind.net",
            Self::Audio => "dev.kind.audio",
            Self::Storage => "dev.kind.storage",
            Self::Input => "dev.kind.input",
            Self::Camera => "dev.kind.camera",
            Self::Bluetooth => "dev.kind.bluetooth",
            Self::Monitor => "dev.kind.monitor",
            Self::Printer => "dev.kind.printer",
            Self::Processor => "dev.kind.processor",
            Self::Battery => "dev.kind.battery",
            Self::System => "dev.kind.system",
            Self::Other => "dev.kind.other",
        }
    }
}

/// Whether it works, as Device Manager would mark it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    #[default]
    Ok,
    /// Windows' problem code (`CM_PROB_*`).
    // Only the configuration manager gives one.
    #[cfg_attr(not(windows), allow(dead_code))]
    Problem(u32),
    Disabled,
    /// Present but no driver bound to it: normal for a bridge on Linux.
    // IOKit lists only matched devices.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    NoDriver,
}

impl Status {
    pub fn key(self) -> &'static str {
        match self {
            Self::Ok => "dev.status.ok",
            Self::Problem(_) => "dev.status.problem",
            Self::Disabled => "dev.status.disabled",
            Self::NoDriver => "dev.status.no_driver",
        }
    }

    pub fn is_problem(self) -> bool {
        matches!(self, Self::Problem(_))
    }
}

/// The `tr` key of a problem code's meaning; `None` for the rare ones, shown
/// by their number alone.
pub fn problem_key(code: u32) -> Option<&'static str> {
    Some(match code {
        1 => "dev.prob.not_configured",
        3 => "dev.prob.out_of_memory",
        10 => "dev.prob.cannot_start",
        12 => "dev.prob.no_resources",
        14 => "dev.prob.restart",
        18 => "dev.prob.reinstall",
        19 => "dev.prob.registry",
        21 => "dev.prob.removing",
        22 => "dev.prob.disabled",
        24 => "dev.prob.not_present",
        28 => "dev.prob.no_driver",
        29 => "dev.prob.firmware_disabled",
        31 => "dev.prob.failed_add",
        37 => "dev.prob.driver_failed",
        39 => "dev.prob.driver_load",
        43 => "dev.prob.reported",
        45 => "dev.prob.not_connected",
        47 => "dev.prob.safe_remove",
        48 => "dev.prob.blocked",
        52 => "dev.prob.signature",
        _ => return None,
    })
}

#[cfg(test)]
const PROBLEMS: [u32; 20] = [1, 3, 10, 12, 14, 18, 19, 21, 22, 24, 28, 29, 31, 37, 39, 43, 45, 47, 48, 52];

/// The bus a device is on, with the ids that name it there.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Bus {
    Usb {
        vid: u16,
        pid: u16,
        /// `bcdDevice`: the device's release.
        rev: Option<u16>,
        /// The link's speed in kbit/s (low speed is 1.5 Mbit/s); Linux and
        /// macOS say it.
        speed_kbps: Option<u32>,
    },
    Pci {
        vendor: u16,
        device: u16,
    },
    #[default]
    Other,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Device {
    /// Windows' instance id, the sysfs name on Linux, the registry entry id
    /// on macOS; unique within the list.
    pub id: String,
    /// The `id` of the device it hangs off, when that one is listed too.
    pub parent: Option<String>,
    pub name: String,
    /// The maker; empty when not known.
    pub vendor: String,
    pub kind: Kind,
    /// The OS's own class name (Windows', in its language); empty where the
    /// OS has none and the kind names the class.
    pub class_name: String,
    /// The driver (Windows' service, the kernel module on Linux, the IOKit
    /// class on macOS); empty when none.
    pub driver: String,
    pub driver_version: String,
    pub status: Status,
    pub bus: Bus,
    /// Where it is plugged in, in the OS's words; empty when not known.
    pub location: String,
}

impl Device {
    /// What the classes column groups it by: the OS's class name, else the
    /// kind's.
    pub fn class(&self) -> String {
        if self.class_name.is_empty() { crate::tr(self.kind.key()) } else { self.class_name.clone() }
    }

    /// The ids as `VID:PID` / `vendor:device`, for the details and the filter.
    pub fn ids(&self) -> Option<String> {
        match self.bus {
            Bus::Usb { vid, pid, .. } => Some(format!("{vid:04x}:{pid:04x}")),
            Bus::Pci { vendor, device } => Some(format!("{vendor:04x}:{device:04x}")),
            Bus::Other => None,
        }
    }
}

/// Why the list is short or empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)]
pub enum Note {
    /// Neither `/sys/bus/usb` nor `/sys/bus/pci` could be read.
    NoSysfs,
    /// No `pci.ids` / `usb.ids`: the devices are named by their ids.
    NoIds,
    /// SetupAPI gave no list.
    NoSetupApi,
    /// The IOKit registry could not be asked.
    NoIoKit,
    OtherOs,
}

impl Note {
    pub const ALL: [Note; 5] = [Self::NoSysfs, Self::NoIds, Self::NoSetupApi, Self::NoIoKit, Self::OtherOs];

    pub fn key(self) -> &'static str {
        match self {
            Self::NoSysfs => "dev.note.no_sysfs",
            Self::NoIds => "dev.note.no_ids",
            Self::NoSetupApi => "dev.note.no_setupapi",
            Self::NoIoKit => "dev.note.no_iokit",
            Self::OtherOs => "dev.note.other_os",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Devices {
    /// By name.
    pub list: Vec<Device>,
    pub note: Option<Note>,
}

/// Reads the devices; it keeps what it looked up in the ids tables on
/// Linux and the class names on Windows.
#[derive(Default)]
pub struct Reader {
    #[cfg(target_os = "linux")]
    ids: linux::Ids,
    #[cfg(windows)]
    classes: setupapi::Classes,
}

impl Reader {
    pub fn read(&mut self) -> Devices {
        let mut out = self.read_os();
        out.list.sort_by_cached_key(|d| (d.name.to_lowercase(), d.id.clone()));
        out
    }

    #[cfg(target_os = "linux")]
    fn read_os(&mut self) -> Devices {
        linux::read(std::path::Path::new("/"), &mut self.ids)
    }

    #[cfg(windows)]
    fn read_os(&mut self) -> Devices {
        setupapi::read(&mut self.classes)
    }

    #[cfg(target_os = "macos")]
    fn read_os(&mut self) -> Devices {
        iokit::read()
    }

    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    fn read_os(&mut self) -> Devices {
        Devices { list: Vec::new(), note: Some(Note::OtherOs) }
    }
}

/// A USB device class (`bDeviceClass`, or an interface's when the device
/// leaves it to them) as a kind.
#[cfg_attr(windows, allow(dead_code))]
pub fn usb_kind(class: u8) -> Kind {
    match class {
        0x01 => Kind::Audio,
        0x02 | 0x0a => Kind::Net,
        0x03 => Kind::Input,
        0x06 | 0x0e => Kind::Camera,
        0x07 => Kind::Printer,
        0x08 => Kind::Storage,
        0x09 => Kind::Usb,
        0xe0 => Kind::Bluetooth,
        _ => Kind::Other,
    }
}

/// A PCI class code (`0xCCSSPP`: class, subclass, interface) as a kind.
#[cfg_attr(windows, allow(dead_code))]
pub fn pci_kind(class: u32) -> Kind {
    let (c, s) = ((class >> 16) as u8, (class >> 8) as u8);
    match (c, s) {
        (0x01, _) => Kind::Storage,
        (0x02, _) => Kind::Net,
        (0x03, _) => Kind::Display,
        (0x04, 0x00) => Kind::Camera,
        (0x04, _) => Kind::Audio,
        (0x05 | 0x06 | 0x08, _) => Kind::System,
        (0x09, _) => Kind::Input,
        (0x0b | 0x12, _) => Kind::Processor,
        (0x0c, 0x03) => Kind::Usb,
        (0x0c, _) => Kind::System,
        (0x0d, 0x11) => Kind::Bluetooth,
        (0x0d, _) => Kind::Net,
        _ => Kind::Other,
    }
}

/// A Windows hardware id (`USB\VID_046D&PID_C52B&REV_1211`,
/// `PCI\VEN_8086&DEV_A0F0&SUBSYS_…&REV_20`) as the bus and its ids.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn hardware_id(id: &str) -> Bus {
    let (bus, rest) = id.split_once('\\').unwrap_or((id, ""));
    let field = |name: &str| {
        rest.split('&').find_map(|f| {
            let (k, v) = f.split_once('_')?;
            k.eq_ignore_ascii_case(name).then(|| u16::from_str_radix(v.get(..4)?, 16).ok()).flatten()
        })
    };
    match bus.to_ascii_uppercase().as_str() {
        "USB" => match (field("VID"), field("PID")) {
            (Some(vid), Some(pid)) => Bus::Usb { vid, pid, rev: field("REV"), speed_kbps: None },
            _ => Bus::Other,
        },
        "PCI" => match (field("VEN"), field("DEV")) {
            (Some(vendor), Some(device)) => Bus::Pci { vendor, device },
            _ => Bus::Other,
        },
        _ => Bus::Other,
    }
}

/// A vendor's name and, when `device` is in the table, the device's.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Named {
    pub vendor: Option<String>,
    pub device: Option<String>,
}

/// Looks the wanted `(vendor, device)` pairs up in the text of `pci.ids` or
/// `usb.ids` (the same layout: a vendor line, its devices indented by a tab,
/// their subsystems by two). The vendor list ends where the class tables
/// begin. Every wanted pair is in the answer, unnamed when not found.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn lookup(text: &str, want: &[(u16, u16)]) -> HashMap<(u16, u16), Named> {
    let mut out: HashMap<(u16, u16), Named> = want.iter().map(|&p| (p, Named::default())).collect();
    let vendors: std::collections::HashSet<u16> = want.iter().map(|p| p.0).collect();
    fn hex(s: &str) -> Option<(u16, &str)> {
        s.get(..4).and_then(|h| u16::from_str_radix(h, 16).ok()).map(|n| (n, s.get(4..).unwrap_or("").trim()))
    }
    let mut vendor: Option<(u16, &str)> = None;
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if let Some(dev) = line.strip_prefix('\t') {
            if dev.starts_with('\t') {
                continue;
            }
            let (Some((v, _)), Some((d, name))) = (vendor, hex(dev)) else { continue };
            if let Some(n) = out.get_mut(&(v, d)) {
                n.device = Some(name.to_string());
            }
        } else {
            // A two-letter tag (`C 03`, `AT 0`, `HID 01`) starts the class and
            // other tables after the vendors.
            if line.split_whitespace().next().is_some_and(|w| w.len() < 4 || u16::from_str_radix(w, 16).is_err()) {
                break;
            }
            vendor = hex(line).filter(|(v, _)| vendors.contains(v));
            if let Some((v, name)) = vendor {
                for (_, n) in out.iter_mut().filter(|(p, _)| p.0 == v) {
                    n.vendor = Some(name.to_string());
                }
            }
        }
    }
    out
}

/// The rows of `shown` (indices into `list`) as a tree: each device under its
/// parent, siblings in `shown`'s order, with the branch lines to draw before
/// its name. A device whose parent is not shown is a root.
pub fn tree(list: &[Device], shown: &[usize]) -> Vec<(usize, String)> {
    let by_id: HashMap<&str, usize> = shown.iter().map(|&i| (list[i].id.as_str(), i)).collect();
    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut roots = Vec::new();
    for &i in shown {
        match list[i].parent.as_deref().and_then(|p| by_id.get(p)).filter(|&&p| p != i) {
            Some(&p) => children.entry(p).or_default().push(i),
            None => roots.push(i),
        }
    }
    let mut out = Vec::with_capacity(shown.len());
    let mut seen = vec![false; list.len()];
    // Roots first; then anything a parent loop left unreached, as a root.
    for &r in roots.iter().chain(shown) {
        let mut stack = vec![(r, String::new(), String::new())];
        while let Some((i, line, under)) = stack.pop() {
            if std::mem::replace(&mut seen[i], true) {
                continue;
            }
            out.push((i, line));
            let kids: Vec<usize> = children.get(&i).map_or_else(Vec::new, |k| k.iter().copied().filter(|&k| !seen[k]).collect());
            for (n, &k) in kids.iter().enumerate().rev() {
                let last = n + 1 == kids.len();
                stack.push((k, format!("{under}{}", if last { "└ " } else { "├ " }), format!("{under}{}", if last { "  " } else { "│ " })));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::tr;

    #[test]
    fn every_kind_status_problem_and_note_has_english() {
        for k in Kind::ALL {
            assert_ne!(tr(k.key()), k.key(), "{k:?}");
        }
        for s in [Status::Ok, Status::Problem(10), Status::Disabled, Status::NoDriver] {
            assert_ne!(tr(s.key()), s.key(), "{s:?}");
        }
        for c in PROBLEMS {
            let k = problem_key(c).unwrap();
            assert_ne!(tr(k), k, "{c}");
        }
        assert_eq!(problem_key(2), None);
        for n in Note::ALL {
            assert_ne!(tr(n.key()), n.key(), "{n:?}");
        }
    }

    #[test]
    fn hardware_ids_give_the_bus_and_ids() {
        assert_eq!(
            hardware_id(r"USB\VID_046D&PID_C52B&REV_1211"),
            Bus::Usb { vid: 0x046d, pid: 0xc52b, rev: Some(0x1211), speed_kbps: None }
        );
        assert_eq!(hardware_id(r"USB\VID_8087&PID_0029"), Bus::Usb { vid: 0x8087, pid: 0x0029, rev: None, speed_kbps: None });
        assert_eq!(hardware_id(r"PCI\VEN_8086&DEV_A0F0&SUBSYS_00748086&REV_20"), Bus::Pci { vendor: 0x8086, device: 0xa0f0 });
        assert_eq!(hardware_id(r"pci\ven_10de&dev_2484"), Bus::Pci { vendor: 0x10de, device: 0x2484 });
        assert_eq!(hardware_id(r"ACPI\PNP0A08"), Bus::Other);
        assert_eq!(hardware_id(r"USB\ROOT_HUB30"), Bus::Other);
        assert_eq!(hardware_id(""), Bus::Other);
    }

    #[test]
    fn class_codes_give_kinds() {
        assert_eq!(usb_kind(0x09), Kind::Usb);
        assert_eq!(usb_kind(0x08), Kind::Storage);
        assert_eq!(usb_kind(0x03), Kind::Input);
        assert_eq!(usb_kind(0x0e), Kind::Camera);
        assert_eq!(usb_kind(0xe0), Kind::Bluetooth);
        assert_eq!(usb_kind(0xff), Kind::Other);
        assert_eq!(pci_kind(0x030000), Kind::Display);
        assert_eq!(pci_kind(0x020000), Kind::Net);
        assert_eq!(pci_kind(0x010802), Kind::Storage);
        assert_eq!(pci_kind(0x040300), Kind::Audio);
        assert_eq!(pci_kind(0x0c0330), Kind::Usb);
        assert_eq!(pci_kind(0x060400), Kind::System);
        assert_eq!(pci_kind(0x120000), Kind::Processor);
        assert_eq!(pci_kind(0xff0000), Kind::Other);
    }

    const IDS: &str = "\
# comment
8086  Intel Corporation
\t0029  AX200 Bluetooth
\t1234  Something
\t\t8086 0001  A subsystem
10de  NVIDIA Corporation
\t2484  GA104 [GeForce RTX 3070]
C 00  Unclassified device
\t00  Non-VGA unclassified device
";

    #[test]
    fn the_ids_table_names_vendors_and_devices() {
        let got = lookup(IDS, &[(0x8086, 0x0029), (0x10de, 0x2484), (0x10de, 0x9999), (0x1234, 0x0000)]);
        assert_eq!(got[&(0x8086, 0x0029)], Named { vendor: Some("Intel Corporation".into()), device: Some("AX200 Bluetooth".into()) });
        assert_eq!(got[&(0x10de, 0x2484)].device.as_deref(), Some("GA104 [GeForce RTX 3070]"));
        assert_eq!(got[&(0x10de, 0x9999)], Named { vendor: Some("NVIDIA Corporation".into()), device: None });
        assert_eq!(got[&(0x1234, 0x0000)], Named::default());
        // The class table's "\t00" is not a device of the last vendor.
        let got = lookup(IDS, &[(0x10de, 0x0000)]);
        assert_eq!(got[&(0x10de, 0x0000)].device, None);
    }

    fn d(id: &str, parent: Option<&str>) -> Device {
        Device { id: id.into(), parent: parent.map(Into::into), name: id.into(), ..Device::default() }
    }

    #[test]
    fn the_tree_puts_devices_under_their_parent() {
        let list = vec![d("pci", None), d("hub", Some("pci")), d("mouse", Some("hub")), d("key", Some("hub")), d("gpu", Some("gone"))];
        let shown: Vec<usize> = (0..list.len()).collect();
        let t = tree(&list, &shown);
        let rows: Vec<String> = t.iter().map(|(i, l)| format!("{l}{}", list[*i].id)).collect();
        assert_eq!(rows, ["pci", "└ hub", "  ├ mouse", "  └ key", "gpu"]);
        // A loop of parents still shows every device once.
        let list = vec![d("a", Some("b")), d("b", Some("a"))];
        assert_eq!(tree(&list, &[0, 1]).len(), 2);
    }
}
