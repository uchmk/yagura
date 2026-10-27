//! macOS: what the IOKit registry says about a USB or PCI device
//! (`iokit.rs` reads it), folded into a device. Pure, so the tests run
//! everywhere.

use super::{Bus, Device, Kind, Status, pci_kind, usb_kind};

/// One `IOUSBHostDevice` or `IOPCIDevice`, as read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Raw {
    /// The registry entry id.
    pub entry: u64,
    /// The nearest USB or PCI device above it.
    pub parent: Option<u64>,
    pub usb: bool,
    /// `USB Product Name`, or a PCI device's `model` / IOName.
    pub name: String,
    /// `USB Vendor Name`.
    pub vendor: String,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    /// `bcdDevice`.
    pub rev: Option<u16>,
    /// `Device Speed`: IOKit's speed code.
    pub speed: Option<u64>,
    /// `bDeviceClass`.
    pub usb_class: Option<u8>,
    /// `class-code`.
    pub pci_class: Option<u32>,
    /// `locationID`.
    pub location: Option<u64>,
    /// The class of the driver matched to it (its first child's).
    pub driver: String,
}

/// IOKit's `Device Speed` code as kbit/s: low, full, high, super and
/// super+ speed.
pub fn speed_kbps(code: u64) -> Option<u32> {
    Some(match code {
        0 => 1_500,
        1 => 12_000,
        2 => 480_000,
        3 => 5_000_000,
        4 => 10_000_000,
        _ => return None,
    })
}

/// A little-endian number in a PCI property's bytes (`vendor-id`,
/// `class-code`).
pub fn le(bytes: &[u8]) -> Option<u32> {
    let mut b = [0u8; 4];
    let n = bytes.len().min(4);
    if n == 0 {
        return None;
    }
    b[..n].copy_from_slice(&bytes[..n]);
    Some(u32::from_le_bytes(b))
}

/// A NUL-terminated string in a property's bytes (a PCI device's `model`).
pub fn bytes_text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

pub fn id(entry: u64) -> String {
    format!("io:{entry:x}")
}

pub fn fold(r: Raw) -> Device {
    let bus = match (r.usb, r.vid, r.pid) {
        (true, Some(vid), Some(pid)) => Bus::Usb { vid, pid, rev: r.rev, speed_kbps: r.speed.and_then(speed_kbps) },
        (false, Some(vendor), Some(device)) => Bus::Pci { vendor, device },
        _ => Bus::Other,
    };
    let kind = if r.usb {
        // A composite device leaves its class to its interfaces; it is
        // still a USB device.
        match r.usb_class.map(usb_kind) {
            Some(Kind::Other) | None => Kind::Usb,
            Some(k) => k,
        }
    } else {
        r.pci_class.map_or(Kind::Other, pci_kind)
    };
    let name = if !r.name.is_empty() {
        r.name
    } else {
        match bus {
            Bus::Usb { vid, pid, .. } => format!("USB {vid:04x}:{pid:04x}"),
            Bus::Pci { vendor, device } => format!("PCI {vendor:04x}:{device:04x}"),
            Bus::Other => id(r.entry),
        }
    };
    Device {
        id: id(r.entry),
        parent: r.parent.map(id),
        name,
        vendor: r.vendor,
        kind,
        class_name: String::new(),
        driver: r.driver,
        driver_version: String::new(),
        status: Status::Ok,
        bus,
        location: r.location.filter(|&l| l != 0).map(|l| format!("0x{l:08x}")).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usb_devices_fold_with_their_speed_and_kind() {
        let d = fold(Raw {
            entry: 0x1000_04a2,
            parent: Some(0x1000_0300),
            usb: true,
            name: "USB Receiver".into(),
            vendor: "Logitech".into(),
            vid: Some(0x046d),
            pid: Some(0xc52b),
            rev: Some(0x1211),
            speed: Some(1),
            usb_class: Some(0),
            location: Some(0x0110_0000),
            driver: "IOUSBHostInterface".into(),
            ..Raw::default()
        });
        assert_eq!(d.id, "io:100004a2");
        assert_eq!(d.parent.as_deref(), Some("io:10000300"));
        assert_eq!(d.kind, Kind::Usb);
        assert_eq!(d.bus, Bus::Usb { vid: 0x046d, pid: 0xc52b, rev: Some(0x1211), speed_kbps: Some(12_000) });
        assert_eq!(d.location, "0x01100000");
        let hub =
            fold(Raw { entry: 1, usb: true, vid: Some(0x05e3), pid: Some(0x0610), usb_class: Some(9), speed: Some(9), ..Raw::default() });
        assert_eq!(hub.kind, Kind::Usb);
        assert_eq!(hub.name, "USB 05e3:0610");
        assert_eq!(hub.bus, Bus::Usb { vid: 0x05e3, pid: 0x0610, rev: None, speed_kbps: None });
        assert_eq!(hub.location, "");
        let disk = fold(Raw { entry: 2, usb: true, usb_class: Some(8), ..Raw::default() });
        assert_eq!((disk.kind, disk.bus, disk.name.as_str()), (Kind::Storage, Bus::Other, "io:2"));
    }

    #[test]
    fn pci_devices_fold_from_their_bytes() {
        assert_eq!(le(&[0x86, 0x80, 0, 0]), Some(0x8086));
        assert_eq!(le(&[0x00, 0x00, 0x03, 0x00]), Some(0x030000));
        assert_eq!(le(&[]), None);
        assert_eq!(bytes_text(b"Apple M2 GPU\0\0"), "Apple M2 GPU");
        let d = fold(Raw {
            entry: 3,
            vid: Some(0x106b),
            pid: Some(0x1003),
            pci_class: Some(0x020000),
            driver: "AppleBCMWLAN".into(),
            ..Raw::default()
        });
        assert_eq!((d.kind, d.bus), (Kind::Net, Bus::Pci { vendor: 0x106b, device: 0x1003 }));
        assert_eq!(d.name, "PCI 106b:1003");
        assert_eq!(d.driver, "AppleBCMWLAN");
        assert_eq!(speed_kbps(4), Some(10_000_000));
    }
}
