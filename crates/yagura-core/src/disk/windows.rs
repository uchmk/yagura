//! Windows: folding the `PhysicalDisk` counters into drives, and reading what
//! `IOCTL_STORAGE_QUERY_PROPERTY` says about each (what it is, how hot it
//! is) and what `IOCTL_STORAGE_PREDICT_FAILURE` says of its health. Pure, so
//! the tests run everywhere; the calls are in `ioctl.rs`.

use std::collections::HashMap;

use super::{Drive, Health, Kind, Why};

/// What a drive says about itself, asked once.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Info {
    pub model: String,
    pub bus: Option<&'static str>,
    pub kind: Kind,
    pub size: Option<u64>,
    pub removable: bool,
}

/// `0 C: D:` → disk 0 with its letters. `_Total` is not a disk.
pub fn parse_instance(name: &str) -> Option<(u32, Vec<String>)> {
    let mut words = name.split_whitespace();
    let index = words.next()?.parse().ok()?;
    Some((index, words.filter(|w| w.ends_with(':')).map(str::to_string).collect()))
}

/// `C:\` → `C:`.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn letter(mount: &str) -> Option<String> {
    let mut c = mount.chars();
    let l = c.next().filter(char::is_ascii_alphabetic)?;
    (c.next() == Some(':')).then(|| format!("{}:", l.to_ascii_uppercase()))
}

/// `STORAGE_BUS_TYPE`.
pub fn bus_name(t: u32) -> Option<&'static str> {
    Some(match t {
        1 => "SCSI",
        2 => "ATAPI",
        3 => "ATA",
        4 => "FireWire",
        5 => "SSA",
        6 => "Fibre Channel",
        7 => "USB",
        8 => "RAID",
        9 => "iSCSI",
        10 => "SAS",
        11 => "SATA",
        12 => "SD",
        13 => "MMC",
        14 => "Virtual",
        15 => "File-backed virtual",
        16 => "Storage Spaces",
        17 => "NVMe",
        18 => "SCM",
        19 => "UFS",
        _ => return None,
    })
}

/// The model, bus and removability from a `STORAGE_DEVICE_DESCRIPTOR`:
/// vendor, product offsets at 12 and 16 (0 for none), removable at 10, the
/// bus type at 28.
pub fn describe(buf: &[u8]) -> Option<(String, Option<&'static str>, bool)> {
    let u32_at = |i: usize| Some(u32::from_le_bytes(buf.get(i..i + 4)?.try_into().ok()?));
    let text = |off: u32| -> String {
        let off = off as usize;
        if off == 0 || off >= buf.len() {
            return String::new();
        }
        let end = buf[off..].iter().position(|&b| b == 0).map_or(buf.len(), |n| off + n);
        String::from_utf8_lossy(&buf[off..end]).trim().to_string()
    };
    let (vendor, product) = (text(u32_at(12)?), text(u32_at(16)?));
    let bus = bus_name(u32_at(28)?);
    let removable = *buf.get(10)? != 0;
    // NVMe drives often put "NVMe" as the vendor, or repeat it in the product.
    let model = if vendor.is_empty() || vendor.eq_ignore_ascii_case("nvme") || product.starts_with(&vendor) {
        product
    } else {
        format!("{vendor} {product}")
    };
    Some((model, bus, removable))
}

/// The temperature and the drive's warning and critical marks from a
/// `STORAGE_TEMPERATURE_DATA_DESCRIPTOR`: critical at 8, warning at 10, the
/// count of readings at 12 and the first 16-byte reading at 24, its value 2
/// bytes in. A mark of 0 is none; a reading of 0 is a drive that keeps none.
pub fn temperature(buf: &[u8]) -> Option<(f32, Option<f32>, Option<f32>)> {
    let i16_at = |i: usize| Some(i16::from_le_bytes(buf.get(i..i + 2)?.try_into().ok()?));
    let count = u16::from_le_bytes(buf.get(12..14)?.try_into().ok()?);
    let value = i16_at(26).filter(|&t| count > 0 && t != 0 && (-60..250).contains(&t))?;
    let mark = |i: usize| i16_at(i).filter(|&t| t > 0).map(f32::from);
    Some((f32::from(value), mark(10), mark(8)))
}

/// The health from a `STORAGE_PREDICT_FAILURE`: a nonzero `PredictFailure`
/// is a drive expecting to fail.
pub fn predict(buf: &[u8]) -> Option<Health> {
    let v = u32::from_le_bytes(buf.get(..4)?.try_into().ok()?);
    Some(if v == 0 { Health::Good } else { Health::Failing })
}

/// Why a drive gave no health, from the Win32 error: access denied wants an
/// administrator; the rest (invalid function, not supported) is a drive or
/// driver that keeps no SMART data.
pub fn why_of(code: u32) -> Why {
    match code {
        5 => Why::NeedsAdmin,
        _ => Why::Unsupported,
    }
}

/// The length of the NVMe SMART / health log page.
pub const NVME_LOG_LEN: usize = 512;

/// An `IOCTL_STORAGE_QUERY_PROPERTY` input asking an NVMe drive for its
/// SMART / health log, with room for the answer: the query's property id
/// (`StorageDeviceProtocolSpecificProperty`, 50) and type (standard, 0), then
/// a `STORAGE_PROTOCOL_SPECIFIC_DATA` at 8 (ten u32s: NVMe, a log page, page
/// 2, sub-value 0, the data's offset from this struct and its length), then
/// the log. The same buffer takes the answer.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn nvme_query() -> Vec<u8> {
    let mut b = vec![0u8; 8 + 40 + NVME_LOG_LEN];
    let words: [u32; 8] = [50, 0, 3, 2, 2, 0, 40, NVME_LOG_LEN as u32];
    for (i, w) in words.iter().enumerate() {
        b[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
    }
    b
}

/// The log out of the answer, a `STORAGE_PROTOCOL_DATA_DESCRIPTOR`: its
/// version and size (both 48), then the protocol data at 8, whose offset
/// (at 8 + 16) and length (at 8 + 20) place the log from the protocol data.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn nvme_reply(buf: &[u8]) -> Option<&[u8]> {
    let u32_at = |i: usize| Some(u32::from_le_bytes(buf.get(i..i + 4)?.try_into().ok()?) as usize);
    if u32_at(0)? != 48 || u32_at(4)? != 48 {
        return None;
    }
    let (off, len) = (u32_at(24)?, u32_at(28)?);
    if off < 40 || len < NVME_LOG_LEN {
        return None;
    }
    buf.get(8 + off..8 + off + NVME_LOG_LEN)
}

/// The length of an ATA drive's SMART data.
pub const ATA_DATA_LEN: usize = 512;

/// A `SENDCMDINPARAMS` (packed, without its one-byte buffer) asking for SMART
/// READ DATA through `SMART_RCV_DRIVE_DATA`: the buffer size, then the IDE
/// registers at 4 (feature `READ_ATTRIBUTES`, one sector, the SMART cylinder
/// signature 0x4F / 0xC2, the master drive, command `SMART_CMD`), then the
/// drive number and reserved bytes.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn smart_query() -> [u8; 32] {
    let mut b = [0u8; 32];
    b[..4].copy_from_slice(&(ATA_DATA_LEN as u32).to_le_bytes());
    b[4..12].copy_from_slice(&[0xD0, 1, 1, 0x4F, 0xC2, 0xA0, 0xB0, 0]);
    b
}

/// The data out of a `SENDCMDOUTPARAMS` (packed): the buffer size, the
/// driver's status at 4 (its error first), the data at 16. `None` when the
/// driver says it failed or the data is short.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn smart_reply(buf: &[u8]) -> Option<&[u8]> {
    if *buf.get(4)? != 0 {
        return None;
    }
    buf.get(16..16 + ATA_DATA_LEN)
}

/// The drives from the three counters (by instance), with what each said
/// about itself. `volumes` holds the drive letters until they are linked to
/// the volumes.
pub fn fold(read: &[(String, f64)], write: &[(String, f64)], idle: &[(String, f64)], info: &HashMap<u32, Info>) -> Vec<Drive> {
    let value = |list: &[(String, f64)], name: &str| list.iter().find(|(n, _)| n == name).map(|(_, v)| *v);
    let mut out: Vec<(u32, Drive)> = read
        .iter()
        .filter_map(|(inst, r)| {
            let (index, letters) = parse_instance(inst)?;
            let i = info.get(&index).cloned().unwrap_or_default();
            let drive = Drive {
                id: index.to_string(),
                name: format!("Disk {index}"),
                model: i.model,
                kind: i.kind,
                bus: i.bus,
                size: i.size,
                removable: i.removable,
                read_rate: r.max(0.0),
                write_rate: value(write, inst).unwrap_or(0.0).max(0.0),
                busy: value(idle, inst).map(|v| (100.0 - v).clamp(0.0, 100.0) as f32),
                volumes: letters,
                ..Drive::default()
            };
            Some((index, drive))
        })
        .collect();
    out.sort_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, d)| d).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_instance_is_a_disk_number_and_its_letters() {
        assert_eq!(parse_instance("0 C: D:"), Some((0, vec!["C:".to_string(), "D:".to_string()])));
        assert_eq!(parse_instance("2"), Some((2, vec![])));
        assert_eq!(parse_instance("_Total"), None);
    }

    #[test]
    fn a_mount_has_a_drive_letter() {
        assert_eq!(letter("C:\\").as_deref(), Some("C:"));
        assert_eq!(letter("d:\\").as_deref(), Some("D:"));
        assert_eq!(letter("\\\\?\\Volume{x}\\"), None);
    }

    #[test]
    fn bus_types_have_names() {
        assert_eq!(bus_name(17), Some("NVMe"));
        assert_eq!(bus_name(11), Some("SATA"));
        assert_eq!(bus_name(7), Some("USB"));
        assert_eq!(bus_name(0), None);
        assert_eq!(bus_name(99), None);
    }

    fn descriptor(vendor: &str, product: &str, bus: u32, removable: bool) -> Vec<u8> {
        let mut b = vec![0u8; 40];
        b[10] = removable as u8;
        b[28..32].copy_from_slice(&bus.to_le_bytes());
        let mut put = |at: usize, s: &str| {
            if s.is_empty() {
                return;
            }
            let off = b.len() as u32;
            b[at..at + 4].copy_from_slice(&off.to_le_bytes());
            b.extend_from_slice(s.as_bytes());
            b.push(0);
        };
        put(12, vendor);
        put(16, product);
        b
    }

    #[test]
    fn a_descriptor_gives_the_model_bus_and_removability() {
        let d = descriptor("", "Samsung SSD 990 PRO 2TB ", 17, false);
        assert_eq!(describe(&d), Some(("Samsung SSD 990 PRO 2TB".to_string(), Some("NVMe"), false)));
        let d = descriptor("NVMe    ", "WD_BLACK SN850X", 17, false);
        assert_eq!(describe(&d).unwrap().0, "WD_BLACK SN850X");
        let d = descriptor("SanDisk ", "Ultra", 7, true);
        assert_eq!(describe(&d), Some(("SanDisk Ultra".to_string(), Some("USB"), true)));
        assert_eq!(describe(&[0u8; 8]), None);
    }

    fn temps(crit: i16, warn: i16, count: u16, value: i16) -> Vec<u8> {
        let mut b = vec![0u8; 40];
        b[8..10].copy_from_slice(&crit.to_le_bytes());
        b[10..12].copy_from_slice(&warn.to_le_bytes());
        b[12..14].copy_from_slice(&count.to_le_bytes());
        b[26..28].copy_from_slice(&value.to_le_bytes());
        b
    }

    #[test]
    fn a_temperature_descriptor_gives_the_reading_and_the_marks() {
        assert_eq!(temperature(&temps(84, 77, 1, 39)), Some((39.0, Some(77.0), Some(84.0))));
        assert_eq!(temperature(&temps(0, 0, 2, 41)), Some((41.0, None, None)));
        // No readings, a reading of 0 (none kept) or nonsense: no temperature.
        assert_eq!(temperature(&temps(84, 77, 0, 39)), None);
        assert_eq!(temperature(&temps(84, 77, 1, 0)), None);
        assert_eq!(temperature(&temps(84, 77, 1, 300)), None);
        assert_eq!(temperature(&[0u8; 20]), None);
    }

    #[test]
    fn predict_failure_and_errors_say_the_health() {
        assert_eq!(predict(&[0u8; 516]), Some(Health::Good));
        let mut b = vec![0u8; 516];
        b[0] = 1;
        assert_eq!(predict(&b), Some(Health::Failing));
        assert_eq!(predict(&[0u8; 2]), None);
        assert_eq!(why_of(5), Why::NeedsAdmin);
        assert_eq!((why_of(1), why_of(50)), (Why::Unsupported, Why::Unsupported));
    }

    #[test]
    fn the_nvme_query_asks_for_the_health_log_and_the_reply_gives_it() {
        let q = nvme_query();
        let word = |b: &[u8], i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        assert_eq!(q.len(), 8 + 40 + 512);
        assert_eq!((word(&q, 0), word(&q, 4)), (50, 0));
        assert_eq!((word(&q, 8), word(&q, 12), word(&q, 16)), (3, 2, 2));
        assert_eq!((word(&q, 24), word(&q, 28)), (40, 512));
        // What the driver writes back over it.
        let mut r = q.clone();
        r[0..4].copy_from_slice(&48u32.to_le_bytes());
        r[4..8].copy_from_slice(&48u32.to_le_bytes());
        r[48] = 7;
        r[48 + 511] = 9;
        let log = nvme_reply(&r).unwrap();
        assert_eq!((log.len(), log[0], log[511]), (512, 7, 9));
        // Another version, an offset into the header or a short log: nothing.
        let mut bad = r.clone();
        bad[0] = 40;
        assert_eq!(nvme_reply(&bad), None);
        let mut bad = r.clone();
        bad[24..28].copy_from_slice(&8u32.to_le_bytes());
        assert_eq!(nvme_reply(&bad), None);
        let mut bad = r.clone();
        bad[28..32].copy_from_slice(&100u32.to_le_bytes());
        assert_eq!(nvme_reply(&bad), None);
        assert_eq!(nvme_reply(&r[..100]), None);
    }

    #[cfg(windows)]
    #[test]
    fn the_protocol_structs_have_the_sizes_the_bytes_assume() {
        use ::windows::Win32::System::Ioctl::{
            READ_ATTRIBUTES, SENDCMDINPARAMS, SENDCMDOUTPARAMS, SMART_CMD, SMART_CYL_HI, SMART_CYL_LOW, STORAGE_PROTOCOL_DATA_DESCRIPTOR,
            STORAGE_PROTOCOL_SPECIFIC_DATA, StorageDeviceProtocolSpecificProperty,
        };
        assert_eq!(size_of::<STORAGE_PROTOCOL_SPECIFIC_DATA>(), 40);
        assert_eq!(size_of::<STORAGE_PROTOCOL_DATA_DESCRIPTOR>(), 48);
        assert_eq!(StorageDeviceProtocolSpecificProperty.0, 50);
        assert_eq!((size_of::<SENDCMDINPARAMS>() - 1, size_of::<SENDCMDOUTPARAMS>() - 1), (32, 16));
        let q = smart_query();
        assert_eq!(
            (u32::from(q[4]), u32::from(q[7]), u32::from(q[8]), u32::from(q[10])),
            (READ_ATTRIBUTES, SMART_CYL_LOW, SMART_CYL_HI, SMART_CMD)
        );
    }

    #[test]
    fn smart_read_data_is_asked_and_unwrapped_from_the_packed_structs() {
        let q = smart_query();
        assert_eq!(&q[..4], &512u32.to_le_bytes());
        assert_eq!(&q[4..12], &[0xD0, 1, 1, 0x4F, 0xC2, 0xA0, 0xB0, 0]);
        assert!(q[12..].iter().all(|&b| b == 0));
        let mut r = vec![0u8; 16 + 512];
        r[16] = 0x10;
        r[16 + 511] = 0x2A;
        let data = smart_reply(&r).unwrap();
        assert_eq!((data.len(), data[0], data[511]), (512, 0x10, 0x2A));
        r[4] = 1; // the driver's error
        assert_eq!(smart_reply(&r), None);
        assert_eq!(smart_reply(&[0u8; 100]), None);
    }

    #[test]
    fn counters_fold_into_drives_by_number() {
        let list = |v: &[(&str, f64)]| v.iter().map(|(n, x)| (n.to_string(), *x)).collect::<Vec<_>>();
        let read = list(&[("1 E:", 10.0), ("0 C: D:", 2048.0), ("_Total", 2058.0)]);
        let write = list(&[("0 C: D:", 512.0)]);
        let idle = list(&[("0 C: D:", 75.0), ("1 E:", 140.0)]);
        let info =
            HashMap::from([(0, Info { model: "Fast".into(), bus: Some("NVMe"), kind: Kind::Ssd, size: Some(1 << 40), removable: false })]);
        let d = fold(&read, &write, &idle, &info);
        assert_eq!(d.len(), 2);
        assert_eq!((d[0].id.as_str(), d[0].name.as_str(), d[0].model.as_str()), ("0", "Disk 0", "Fast"));
        assert_eq!((d[0].read_rate, d[0].write_rate, d[0].busy), (2048.0, 512.0, Some(25.0)));
        assert_eq!(d[0].volumes, ["C:", "D:"]);
        // Idle time over 100 (the counter overshoots) is not negative busy.
        assert_eq!(d[1].busy, Some(0.0));
        assert_eq!((d[1].kind, d[1].write_rate), (Kind::Unknown, 0.0));
    }
}
