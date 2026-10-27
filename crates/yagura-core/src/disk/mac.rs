//! macOS: the drives from IOKit. Each `IOBlockStorageDriver` keeps the bytes
//! read and written since boot in its `Statistics`; the device above it names
//! the model, the medium and the bus, and the whole-disk `IOMedia` below it
//! the BSD name and the size. Disk images have drivers too and are left out.
//! macOS keeps no busy time for a drive, so there is none. The IOKit calls
//! are in `iokit.rs`; what they read is folded here, where the tests run on
//! every OS. SMART health is `diskutil info`'s `SMARTStatus`; macOS gives no
//! drive temperature outside the SMC sensors.

use std::collections::HashMap;

#[cfg(target_os = "macos")]
use super::health::Ask;
use super::health::Check;
use super::{Drive, Health, Kind, Why};

/// What IOKit says about one block storage driver.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Raw {
    /// The whole disk below the driver: `disk0`.
    pub bsd: String,
    /// `Product Name` in the device's characteristics.
    pub product: String,
    /// `Medium Type`: `Solid State` or `Rotational`.
    pub medium: String,
    /// `Physical Interconnect`: `Apple Fabric`, `PCI-Express`, `USB`, …
    pub interconnect: String,
    /// `Physical Interconnect Location`: `Internal`, `External` or `File`.
    pub location: String,
    pub size: Option<u64>,
    /// The disk says it is removable or ejectable.
    pub removable: bool,
    pub read: u64,
    pub write: u64,
}

pub fn kind(medium: &str) -> Kind {
    match medium.trim() {
        "Solid State" => Kind::Ssd,
        "Rotational" => Kind::Hdd,
        _ => Kind::Unknown,
    }
}

pub fn bus(interconnect: &str) -> Option<&'static str> {
    Some(match interconnect.trim() {
        "Apple Fabric" => "Apple Fabric",
        "PCI-Express" | "PCI" => "PCIe",
        "SATA" => "SATA",
        "SAS" => "SAS",
        "USB" => "USB",
        "Thunderbolt" => "Thunderbolt",
        "FireWire" => "FireWire",
        "Secure Digital" => "SD / MMC",
        "Virtual Interface" => "virtual",
        _ => return None,
    })
}

/// The drives among the drivers, by their disk number.
pub fn drives(raws: Vec<Raw>) -> Vec<Drive> {
    let mut out: Vec<Drive> = raws
        .into_iter()
        // A mounted disk image is a file on another drive; an empty card
        // reader has no size.
        .filter(|r| !r.bsd.is_empty() && r.location != "File" && r.size != Some(0))
        .map(|r| {
            let bus = bus(&r.interconnect);
            Drive {
                id: r.bsd.clone(),
                name: r.bsd,
                model: r.product.trim().to_string(),
                kind: kind(&r.medium),
                bus,
                size: r.size,
                removable: r.removable || r.location == "External",
                read_total: Some(r.read),
                write_total: Some(r.write),
                ..Drive::default()
            }
        })
        .collect();
    // `disk2` before `disk10`.
    out.sort_by(|a, b| (a.name.len(), &a.name).cmp(&(b.name.len(), &b.name)));
    out
}

/// Fill in the rates from the counts of the last reading, `secs` ago.
pub fn rates(list: &mut [Drive], last: &HashMap<String, (u64, u64)>, secs: Option<f64>) {
    let Some(secs) = secs.filter(|&s| s > 0.0) else { return };
    for d in list {
        let Some(&(r, w)) = last.get(&d.id) else { continue };
        d.read_rate = d.read_total.unwrap_or(0).saturating_sub(r) as f64 / secs;
        d.write_rate = d.write_total.unwrap_or(0).saturating_sub(w) as f64 / secs;
    }
}

/// The BSD name a volume was mounted from (`statfs`'s `f_mntfromname`):
/// `disk3s1s1` from `/dev/disk3s1s1`. A network share or `devfs` has none.
pub fn bsd_of(from: &str) -> Option<&str> {
    from.strip_prefix("/dev/").filter(|n| n.starts_with("disk"))
}

/// The health from `diskutil info -plist`: `SMARTStatus` is `Verified`,
/// `Failing` or `Not Supported` (an external or virtual disk).
pub fn smart_status(plist_bytes: &[u8]) -> Check {
    let status = plist::Value::from_reader(std::io::Cursor::new(plist_bytes))
        .ok()
        .and_then(|v| v.into_dictionary())
        .and_then(|d| d.get("SMARTStatus").and_then(|v| v.as_string()).map(str::to_string));
    match status.as_deref() {
        Some("Verified") => Check { health: Some(Health::Good), ..Check::default() },
        Some("Failing") => Check { health: Some(Health::Failing), ..Check::default() },
        _ => Check::why(Why::Unsupported),
    }
}

/// Ask `diskutil` about each drive, and read an NVMe drive's health log or
/// an ATA drive's SMART data for its counts and temperature.
#[cfg(target_os = "macos")]
pub fn check(asks: &[Ask]) -> HashMap<String, Check> {
    asks.iter()
        .map(|a| {
            let out = std::process::Command::new("diskutil").args(["info", "-plist", &a.id]).stderr(std::process::Stdio::null()).output();
            let mut c = match out {
                Ok(o) if o.status.success() => smart_status(&o.stdout),
                _ => Check::why(Why::Unsupported),
            };
            if let Some(log) = super::iokit::nvme_smart(&a.id).as_ref().and_then(|b| super::nvme_log(b)) {
                c.add_log(log, "NVMe health log");
            } else if let Some((exceeded, data)) = super::iokit::ata_smart(&a.id)
                && let Some((temp, smart)) = super::ata_attrs(&data)
            {
                let health = if exceeded { Health::Failing } else { Health::Good };
                c.add_log((health, temp, smart), "ATA SMART");
            }
            (a.id.clone(), c)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(bsd: &str, location: &str) -> Raw {
        Raw { bsd: bsd.into(), location: location.into(), size: Some(1 << 30), ..Raw::default() }
    }

    #[test]
    fn a_driver_becomes_a_drive_with_its_model_kind_and_bus() {
        let r = Raw {
            product: "APPLE SSD AP0512Z ".into(),
            medium: "Solid State".into(),
            interconnect: "Apple Fabric".into(),
            read: 4096,
            write: 8192,
            ..raw("disk0", "Internal")
        };
        let d = &drives(vec![r])[0];
        assert_eq!((d.id.as_str(), d.name.as_str(), d.model.as_str()), ("disk0", "disk0", "APPLE SSD AP0512Z"));
        assert_eq!((d.kind, d.bus, d.size, d.removable), (Kind::Ssd, Some("Apple Fabric"), Some(1 << 30), false));
        assert_eq!((d.read_total, d.write_total, d.busy), (Some(4096), Some(8192), None));
    }

    #[test]
    fn disk_images_empty_readers_and_nameless_drivers_are_left_out() {
        let empty = Raw { size: Some(0), ..raw("disk5", "External") };
        let list = drives(vec![raw("disk10", "External"), raw("disk4", "File"), empty, raw("", "Internal"), raw("disk2", "Internal")]);
        let names: Vec<&str> = list.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["disk2", "disk10"]);
        // An external drive counts as removable even when the disk does not say so.
        assert!(list[1].removable && !list[0].removable);
    }

    #[test]
    fn medium_and_interconnect_read_as_kind_and_bus() {
        assert_eq!((kind("Rotational"), kind("Solid State"), kind("")), (Kind::Hdd, Kind::Ssd, Kind::Unknown));
        assert_eq!((bus("PCI-Express"), bus("USB"), bus("SATA")), (Some("PCIe"), Some("USB"), Some("SATA")));
        assert_eq!((bus("Secure Digital"), bus("Something New")), (Some("SD / MMC"), None));
    }

    #[test]
    fn rates_come_from_the_last_counts() {
        let mut list = drives(vec![Raw { read: 3000, write: 5000, ..raw("disk0", "Internal") }, raw("disk6", "External")]);
        let last = HashMap::from([("disk0".to_string(), (1000, 1000))]);
        rates(&mut list, &last, Some(2.0));
        assert_eq!((list[0].read_rate, list[0].write_rate), (1000.0, 2000.0));
        // A drive plugged in since has no rate yet.
        assert_eq!((list[1].read_rate, list[1].write_rate), (0.0, 0.0));
        let mut first = list.clone();
        rates(&mut first, &HashMap::new(), None);
        assert_eq!(first, list);
    }

    fn info(status: &str) -> Vec<u8> {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\
<key>DeviceIdentifier</key><string>disk0</string><key>SMARTStatus</key><string>{status}</string></dict></plist>"
        )
        .into_bytes()
    }

    #[test]
    fn diskutil_says_the_smart_status() {
        assert_eq!(smart_status(&info("Verified")).health, Some(Health::Good));
        assert_eq!(smart_status(&info("Failing")).health, Some(Health::Failing));
        assert_eq!(smart_status(&info("Not Supported")), Check::why(Why::Unsupported));
        assert_eq!(smart_status(b"not a plist"), Check::why(Why::Unsupported));
    }

    #[test]
    fn a_volume_is_mounted_from_a_bsd_disk_or_not() {
        assert_eq!(bsd_of("/dev/disk3s1s1"), Some("disk3s1s1"));
        assert_eq!(bsd_of("//user@nas/share"), None);
        assert_eq!(bsd_of("devfs"), None);
        assert_eq!(bsd_of("map auto_home"), None);
    }
}
