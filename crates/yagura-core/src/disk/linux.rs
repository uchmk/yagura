//! Linux: the drives from `/proc/diskstats` (traffic and busy time) and
//! `/sys/block` (which entries are whole drives, their size, model, bus and
//! kind, and the temperature of a drive whose driver registers a hwmon chip:
//! `nvme`, or `drivetemp` for SATA). SMART health is asked of `smartctl`,
//! which also gives the wear and error counts, and the temperature of a SATA
//! drive without `drivetemp`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[cfg(target_os = "linux")]
use super::health::Ask;
use super::health::Check;
use super::{Drive, Health, Kind, Note, Temp, Why};

/// One line of `/proc/diskstats`, in bytes and milliseconds.
#[derive(Clone, Debug, PartialEq)]
pub struct Stat {
    pub name: String,
    pub read: u64,
    pub write: u64,
    /// Milliseconds spent with I/O in flight (`io_ticks`).
    pub ticks: u64,
}

/// Sectors in diskstats and sysfs are 512 bytes whatever the drive's own.
const SECTOR: u64 = 512;

pub fn parse(text: &str) -> Vec<Stat> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            // major minor name, then reads merged sectors ms, writes merged
            // sectors ms, in-flight, io_ticks, …
            let num = |i: usize| f.get(i)?.parse::<u64>().ok();
            Some(Stat { name: f.get(2)?.to_string(), read: num(5)? * SECTOR, write: num(9)? * SECTOR, ticks: num(12)? })
        })
        .collect()
}

/// Block devices that are not drives: loop files, RAM disks, compressed swap.
pub fn skip(name: &str) -> bool {
    ["loop", "ram", "zram"].iter().any(|p| name.starts_with(p))
}

/// The bus from the device name and where it sits in sysfs.
pub fn bus(name: &str, path: &str) -> Option<&'static str> {
    let starts = |p: &str| name.starts_with(p);
    Some(if path.contains("/usb") {
        "USB"
    } else if starts("nvme") {
        "NVMe"
    } else if starts("mmcblk") {
        "SD / MMC"
    } else if starts("vd") {
        "virtio"
    } else if starts("xvd") {
        "Xen"
    } else if starts("dm-") {
        "device-mapper"
    } else if starts("md") {
        "md RAID"
    } else if starts("sr") {
        "optical"
    } else if starts("sd") {
        "SATA / SCSI"
    } else {
        return None;
    })
}

/// Spinning or not from `queue/rotational`. A virtual disk's answer is the
/// host's guess, so it reads as unknown.
pub fn kind(rotational: Option<&str>, bus: Option<&str>) -> Kind {
    if matches!(bus, Some("virtio" | "Xen" | "device-mapper" | "md RAID")) {
        return Kind::Unknown;
    }
    match rotational.map(str::trim) {
        Some("1") => Kind::Hdd,
        Some("0") => Kind::Ssd,
        _ => Kind::Unknown,
    }
}

/// The drive a device node is, or is a partition of: `sda2` on `sda`,
/// `nvme0n1p1` on `nvme0n1`, `mmcblk0p2` on `mmcblk0`.
pub fn owner<'a>(dev: &str, ids: &[&'a str]) -> Option<&'a str> {
    ids.iter()
        .filter(|id| match dev.strip_prefix(**id) {
            Some("") => true,
            Some(rest) => {
                let rest = rest.strip_prefix('p').filter(|_| id.ends_with(|c: char| c.is_ascii_digit())).unwrap_or(rest);
                !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
            }
            None => false,
        })
        .max_by_key(|id| id.len())
        .copied()
}

/// The hwmon chip a drive's driver registered: on the NVMe controller
/// (`device/hwmon3`) or on the SCSI device (`device/hwmon/hwmon4`).
fn hwmon(dir: &Path) -> Option<PathBuf> {
    let dev = dir.join("device");
    let mut found: Vec<PathBuf> = [dev.clone(), dev.join("hwmon")]
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flat_map(|rd| rd.flatten().map(|e| e.path()))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.len() > 5 && n.starts_with("hwmon")))
        .collect();
    found.sort();
    found.into_iter().next()
}

/// The drive's temperature from its hwmon chip, with the chip's name so the
/// sensors screen does not list it twice.
fn hwmon_temp(dir: &Path) -> Option<Temp> {
    let chip = hwmon(dir)?;
    let milli = |name: &str| {
        let v: f32 = std::fs::read_to_string(chip.join(name)).ok()?.trim().parse().ok()?;
        Some(v / 1000.0).filter(|c| (-60.0..250.0).contains(c))
    };
    let input = chip.join("temp1_input");
    Some(Temp {
        value: milli("temp1_input")?,
        high: milli("temp1_max").filter(|&c| c > 0.0),
        crit: milli("temp1_crit").filter(|&c| c > 0.0),
        source: input.display().to_string(),
        chip: chip.file_name().map(|n| n.to_string_lossy().into_owned()),
    })
}

/// The whole drives among the stats, filled in from sysfs. Each comes with
/// its busy milliseconds, for the next reading's rate.
pub fn drives(sys_block: &Path, stats: &[Stat]) -> Vec<(Drive, u64)> {
    let read = |p: &Path| std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let mut out: Vec<(Drive, u64)> = stats
        .iter()
        .filter(|s| !skip(&s.name))
        .filter_map(|s| {
            let dir = sys_block.join(&s.name);
            // Partitions are in diskstats but not in /sys/block.
            if !dir.is_dir() {
                return None;
            }
            let size = read(&dir.join("size")).and_then(|v| v.parse::<u64>().ok()).map(|n| n * SECTOR);
            // An empty card reader or optical drive.
            if size == Some(0) {
                return None;
            }
            let path = std::fs::canonicalize(&dir).map(|p| p.display().to_string()).unwrap_or_default();
            let bus = bus(&s.name, &path);
            let model = read(&dir.join("device/model"))
                .or_else(|| read(&dir.join("dm/name")))
                .or_else(|| read(&dir.join("md/level")))
                .unwrap_or_default();
            let drive = Drive {
                id: s.name.clone(),
                name: s.name.clone(),
                model,
                kind: kind(read(&dir.join("queue/rotational")).as_deref(), bus),
                bus,
                size,
                removable: read(&dir.join("removable")).as_deref() == Some("1"),
                read_total: Some(s.read),
                write_total: Some(s.write),
                temp: hwmon_temp(&dir),
                ..Drive::default()
            };
            Some((drive, s.ticks))
        })
        .collect();
    out.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    out
}

/// Fill in the rates and busy share from the counts of the last reading,
/// `secs` ago.
pub fn rates(list: &mut [(Drive, u64)], last: &HashMap<String, (u64, u64, u64)>, secs: Option<f64>) {
    let Some(secs) = secs.filter(|&s| s > 0.0) else { return };
    for (d, ticks) in list {
        let Some(&(r, w, t)) = last.get(&d.id) else { continue };
        d.read_rate = d.read_total.unwrap_or(0).saturating_sub(r) as f64 / secs;
        d.write_rate = d.write_total.unwrap_or(0).saturating_sub(w) as f64 / secs;
        d.busy = Some((ticks.saturating_sub(t) as f64 / (secs * 10.0)).min(100.0) as f32);
    }
}

/// Buses whose drives may keep SMART data. A virtual disk, a card, an
/// optical drive and the block layers over other drives keep none; an
/// unknown bus is tried.
pub fn smart_bus(bus: Option<&str>) -> bool {
    matches!(bus, None | Some("NVMe" | "SATA / SCSI" | "USB"))
}

/// The first number in a field: `36` from `36 (Min/Max 21/49)`.
fn lead_num(s: &str) -> Option<f32> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    s[..end].parse().ok()
}

/// The first count in a field, with its thousands commas: `9876543` from
/// `9,876,543 [5.05 TB]`.
fn lead_count(s: &str) -> Option<u64> {
    let digits: String = s.trim().chars().take_while(|c| c.is_ascii_digit() || *c == ',').filter(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// ATA attributes whose normalized value is the life left, in percent.
const LIFE_LEFT: [&str; 6] = [
    "Wear_Leveling_Count",
    "SSD_Life_Left",
    "Media_Wearout_Indicator",
    "Percent_Lifetime_Remain",
    "Remaining_Lifetime_Perc",
    "Percent_Life_Remaining",
];

/// What `smartctl -H -A` printed (in the C locale): the health from the
/// overall assessment (ATA and NVMe) or the health status (SCSI), the
/// temperature from NVMe's `Temperature:`, SCSI's `Current Drive
/// Temperature:` or ATA's attribute 194 (190 where there is no 194), and the
/// wear and error counts from NVMe's health log, SCSI's pages or the ATA
/// attributes (5 remapped, 9 hours, 197 pending, the life left of an SSD).
pub fn parse_smart(text: &str) -> Check {
    if text.contains("STANDBY mode") || text.contains("SLEEP mode") {
        return Check::why(Why::Asleep);
    }
    if text.contains("Permission denied") || text.contains("Operation not permitted") {
        return Check::why(Why::NeedsRoot);
    }
    let mut c = Check::default();
    let (mut temp, mut trip, mut airflow) = (None, None, None);
    let sm = &mut c.smart;
    for line in text.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("SMART overall-health self-assessment test result:") {
            c.health = Some(if v.trim().starts_with("PASSED") { Health::Good } else { Health::Failing });
        } else if let Some(v) = line.strip_prefix("SMART Health Status:") {
            c.health = Some(if v.trim() == "OK" { Health::Good } else { Health::Failing });
        } else if let Some(v) = line.strip_prefix("Current Drive Temperature:").or_else(|| line.strip_prefix("Temperature:")) {
            temp = temp.or(lead_num(v.trim()));
        } else if let Some(v) = line.strip_prefix("Drive Trip Temperature:") {
            trip = lead_num(v.trim());
        } else if let Some(v) = line.strip_prefix("Percentage Used:").or_else(|| line.strip_prefix("Percentage used endurance indicator:"))
        {
            sm.life_used = lead_count(v).and_then(|n| u32::try_from(n).ok());
        } else if let Some(v) = line.strip_prefix("Data Units Written:") {
            // Thousands of 512-byte blocks.
            sm.written = lead_count(v).map(|n| n.saturating_mul(512_000));
        } else if let Some(v) =
            line.strip_prefix("Power On Hours:").or_else(|| line.strip_prefix("Accumulated power on time, hours:minutes"))
        {
            sm.power_on_hours = lead_count(v);
        } else if let Some(v) = line.strip_prefix("Media and Data Integrity Errors:") {
            sm.media_errors = lead_count(v);
        } else if let Some(v) = line.strip_prefix("Elements in grown defect list:") {
            sm.remapped = lead_count(v);
        } else {
            // ID# ATTRIBUTE_NAME FLAG VALUE WORST THRESH TYPE UPDATED WHEN_FAILED RAW_VALUE
            let f: Vec<&str> = line.split_whitespace().collect();
            let raw = || f.get(9).and_then(|v| lead_num(v));
            let count = || f.get(9).and_then(|v| lead_count(v));
            match f.first() {
                Some(&"194") => temp = temp.or(raw()),
                Some(&"190") => airflow = raw(),
                Some(&"5") if f.get(1) == Some(&"Reallocated_Sector_Ct") => sm.remapped = count(),
                Some(&"9") if f.get(1) == Some(&"Power_On_Hours") => sm.power_on_hours = count(),
                Some(&"197") if f.get(1) == Some(&"Current_Pending_Sector") => sm.pending = count(),
                _ if f.len() >= 10 && LIFE_LEFT.contains(&f[1]) => {
                    if let Some(left) = f[3].parse::<u32>().ok().filter(|&v| v <= 100) {
                        sm.life_used = Some(100 - left);
                    }
                }
                _ => {}
            }
        }
    }
    c.temp = temp.or(airflow).filter(|t| *t > 0.0 && *t < 250.0).map(|value| Temp {
        value,
        crit: trip.filter(|&t| t > 0.0),
        source: "smartctl".into(),
        ..Temp::default()
    });
    if c.health.is_none() {
        c.why = Some(Why::Unsupported);
    }
    c
}

/// Ask `smartctl` about each drive, leaving a sleeping drive asleep.
#[cfg(target_os = "linux")]
pub fn smart(asks: &[Ask]) -> HashMap<String, Check> {
    asks.iter().map(|a| (a.id.clone(), smart_one(a))).collect()
}

#[cfg(target_os = "linux")]
fn smart_one(a: &Ask) -> Check {
    use std::io::ErrorKind;
    use std::process::{Command, Stdio};
    if !smart_bus(a.bus) {
        return Check::why(Why::Unsupported);
    }
    let dev = format!("/dev/{}", a.id);
    // smartctl would say the same at more cost.
    match std::fs::File::open(&dev) {
        Err(e) if e.kind() == ErrorKind::PermissionDenied => return Check::why(Why::NeedsRoot),
        Err(_) => return Check::why(Why::Unsupported),
        Ok(_) => {}
    }
    let out = Command::new("smartctl")
        .args(["-H", "-A", "-n", "standby", &dev])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) => parse_smart(&String::from_utf8_lossy(&o.stdout)),
        Err(e) if e.kind() == ErrorKind::NotFound => Check::why(Why::NoSmartctl),
        Err(_) => Check::why(Why::Unsupported),
    }
}

#[derive(Default)]
pub struct Reader {
    last: HashMap<String, (u64, u64, u64)>,
    at: Option<Instant>,
}

impl Reader {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn read(&mut self, sys_block: &Path) -> (Vec<Drive>, Option<Note>) {
        let Ok(text) = std::fs::read_to_string("/proc/diskstats") else { return (Vec::new(), Some(Note::NoDiskstats)) };
        let mut list = drives(sys_block, &parse(&text));
        let now = Instant::now();
        rates(&mut list, &self.last, self.at.map(|t| now.duration_since(t).as_secs_f64()));
        self.last = list.iter().map(|(d, t)| (d.id.clone(), (d.read_total.unwrap_or(0), d.write_total.unwrap_or(0), *t))).collect();
        self.at = Some(now);
        (list.into_iter().map(|(d, _)| d).collect(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::super::Smart;
    use super::*;

    const STATS: &str = "\
   7       0 loop0 10 0 80 0 0 0 0 0 0 0 0 0 0 0 0 0 0
 254       0 vda 53601 24113 5743114 258378 10166 10314 21495664 84461 0 229144 355162 34564 0 40964768 11763 138 557
 254       1 vda1 100 0 2000 10 5 0 40 1 0 20 11
 259       0 nvme0n1 1 0 8 0 2 0 16 0 0 7 0
 short line
";

    #[test]
    fn diskstats_lines_become_bytes_and_busy_time() {
        let s = parse(STATS);
        assert_eq!(s.len(), 4);
        assert_eq!(s[1], Stat { name: "vda".into(), read: 5743114 * 512, write: 21495664 * 512, ticks: 229144 });
        assert_eq!(s[3], Stat { name: "nvme0n1".into(), read: 8 * 512, write: 16 * 512, ticks: 7 });
    }

    #[test]
    fn loops_ram_disks_and_zram_are_not_drives() {
        assert!(skip("loop3") && skip("ram0") && skip("zram0"));
        assert!(!skip("sda") && !skip("nvme0n1"));
    }

    #[test]
    fn the_bus_comes_from_the_name_or_a_usb_path() {
        assert_eq!(bus("sda", "/sys/devices/pci0000:00/0000:00:17.0/ata1/host0/target0:0:0/0:0:0:0/block/sda"), Some("SATA / SCSI"));
        assert_eq!(bus("sdb", "/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/block/sdb"), Some("USB"));
        assert_eq!(bus("nvme0n1", ""), Some("NVMe"));
        assert_eq!(bus("mmcblk0", ""), Some("SD / MMC"));
        assert_eq!(bus("vda", ""), Some("virtio"));
        assert_eq!(bus("dm-0", ""), Some("device-mapper"));
        assert_eq!(bus("weird0", ""), None);
    }

    #[test]
    fn rotational_says_the_kind_except_on_virtual_disks() {
        assert_eq!(kind(Some("0\n"), Some("NVMe")), Kind::Ssd);
        assert_eq!(kind(Some("1"), Some("SATA / SCSI")), Kind::Hdd);
        assert_eq!(kind(Some("1"), Some("virtio")), Kind::Unknown);
        assert_eq!(kind(None, None), Kind::Unknown);
    }

    #[test]
    fn a_partition_belongs_to_its_drive() {
        let ids = ["sda", "sdab", "nvme0n1", "nvme0n10", "mmcblk0", "dm-0"];
        assert_eq!(owner("sda", &ids), Some("sda"));
        assert_eq!(owner("sda2", &ids), Some("sda"));
        assert_eq!(owner("sdab1", &ids), Some("sdab"));
        assert_eq!(owner("nvme0n1p3", &ids), Some("nvme0n1"));
        assert_eq!(owner("nvme0n10p1", &ids), Some("nvme0n10"));
        assert_eq!(owner("mmcblk0p1", &ids), Some("mmcblk0"));
        assert_eq!(owner("dm-0", &ids), Some("dm-0"));
        // `sdc` is not a partition of `sd…` anything listed, and `sdap` is no partition name.
        assert_eq!(owner("sdc1", &ids), None);
        assert_eq!(owner("sdap", &ids), None);
    }

    #[test]
    fn drives_are_the_whole_disks_in_sysfs() {
        let root = crate::util::test_dir("disk-sysfs");
        let vda = root.join("vda");
        std::fs::create_dir_all(vda.join("queue")).unwrap();
        std::fs::write(vda.join("size"), "536870912\n").unwrap();
        std::fs::write(vda.join("queue/rotational"), "1\n").unwrap();
        std::fs::write(vda.join("removable"), "0\n").unwrap();
        let nvme = root.join("nvme0n1");
        std::fs::create_dir_all(nvme.join("device")).unwrap();
        std::fs::create_dir_all(nvme.join("queue")).unwrap();
        std::fs::write(nvme.join("size"), "2000\n").unwrap();
        std::fs::write(nvme.join("device/model"), "Samsung SSD 990 PRO 2TB   \n").unwrap();
        std::fs::write(nvme.join("queue/rotational"), "0\n").unwrap();
        // loop0 is skipped by name and vda1 is a partition (not in the tree).
        std::fs::create_dir_all(root.join("loop0")).unwrap();
        let list = drives(&root, &parse(STATS));
        let names: Vec<&str> = list.iter().map(|(d, _)| d.name.as_str()).collect();
        assert_eq!(names, ["nvme0n1", "vda"]);
        let (nv, ticks) = &list[0];
        assert_eq!(nv.model, "Samsung SSD 990 PRO 2TB");
        assert_eq!((nv.kind, nv.bus, nv.size), (Kind::Ssd, Some("NVMe"), Some(2000 * 512)));
        assert_eq!(*ticks, 7);
        let (vd, _) = &list[1];
        assert_eq!((vd.kind, vd.bus, vd.removable), (Kind::Unknown, Some("virtio"), false));
        assert_eq!(vd.read_total, Some(5743114 * 512));
        assert_eq!((&nv.temp, &vd.temp), (&None, &None));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_drive_with_a_hwmon_chip_has_its_temperature() {
        let root = crate::util::test_dir("disk-hwmon");
        let put = |p: &Path, body: &str| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        // NVMe: the chip is on the controller; SATA with drivetemp: under device/hwmon.
        let nvme = root.join("nvme0n1/device/hwmon3");
        put(&nvme.join("temp1_input"), "38850\n");
        put(&nvme.join("temp1_max"), "81850\n");
        put(&nvme.join("temp1_crit"), "84850\n");
        let sda = root.join("sda/device/hwmon/hwmon4");
        put(&sda.join("temp1_input"), "31000\n");
        put(&sda.join("temp1_max"), "0\n");
        std::fs::create_dir_all(root.join("sdb/device")).unwrap();
        let nv = hwmon_temp(&root.join("nvme0n1")).unwrap();
        assert_eq!((nv.value, nv.high, nv.crit, nv.chip.as_deref()), (38.85, Some(81.85), Some(84.85), Some("hwmon3")));
        assert!(nv.source.ends_with("temp1_input"));
        let sa = hwmon_temp(&root.join("sda")).unwrap();
        assert_eq!((sa.value, sa.high, sa.chip.as_deref()), (31.0, None, Some("hwmon4")));
        assert_eq!(hwmon_temp(&root.join("sdb")), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn smart_is_asked_only_of_buses_that_keep_it() {
        assert!(smart_bus(Some("NVMe")) && smart_bus(Some("SATA / SCSI")) && smart_bus(Some("USB")) && smart_bus(None));
        assert!(!smart_bus(Some("virtio")) && !smart_bus(Some("device-mapper")) && !smart_bus(Some("SD / MMC")));
    }

    const ATA: &str = "\
smartctl 7.4 2023-08-01 r5530 [x86_64-linux-6.8.0] (local build)

=== START OF READ SMART DATA SECTION ===
SMART overall-health self-assessment test result: PASSED

SMART Attributes Data Structure revision number: 16
ID# ATTRIBUTE_NAME          FLAG     VALUE WORST THRESH TYPE      UPDATED  WHEN_FAILED RAW_VALUE
  5 Reallocated_Sector_Ct   0x0033   100   100   010    Pre-fail  Always       -       0
190 Airflow_Temperature_Cel 0x0032   067   051   000    Old_age   Always       -       33
194 Temperature_Celsius     0x0022   064   049   000    Old_age   Always       -       36 (Min/Max 21/49)
";

    #[test]
    fn smartctl_on_an_ata_drive_gives_health_and_temperature() {
        let c = parse_smart(ATA);
        assert_eq!((c.health, c.why), (Some(Health::Good), None));
        let t = c.temp.unwrap();
        assert_eq!((t.value, t.crit, t.source.as_str(), t.chip), (36.0, None, "smartctl", None));
        // Without 194 the airflow temperature stands in.
        let c = parse_smart(&ATA.replace("194 Temperature_Celsius", "999 Something_Else"));
        assert_eq!(c.temp.map(|t| t.value), Some(33.0));
        let c = parse_smart(&ATA.replace("PASSED", "FAILED!"));
        assert_eq!(c.health, Some(Health::Failing));
    }

    #[test]
    fn smartctl_on_nvme_and_scsi_drives() {
        let nvme = "=== START OF SMART DATA SECTION ===\nSMART overall-health self-assessment test result: PASSED\n\n\
SMART/Health Information (NVMe Log 0x02)\nCritical Warning:                   0x00\nTemperature:                        41 Celsius\n\
Temperature Sensor 1:               52 Celsius\n";
        let c = parse_smart(nvme);
        assert_eq!((c.health, c.temp.map(|t| t.value)), (Some(Health::Good), Some(41.0)));
        let scsi = "SMART Health Status: OK\n\nCurrent Drive Temperature:     30 C\nDrive Trip Temperature:        65 C\n";
        let c = parse_smart(scsi);
        let t = c.temp.clone().unwrap();
        assert_eq!((c.health, t.value, t.crit), (Some(Health::Good), 30.0, Some(65.0)));
        let c = parse_smart("SMART Health Status: FIRMWARE IMPENDING FAILURE TOO MANY BLOCK REASSIGNS [asc=5d, ascq=64]\n");
        assert_eq!(c.health, Some(Health::Failing));
    }

    #[test]
    fn smartctl_gives_the_wear_and_error_counts() {
        let ata = ATA.to_string()
            + "  9 Power_On_Hours          0x0032   095   095   000    Old_age   Always       -       21345\n\
177 Wear_Leveling_Count     0x0013   097   097   000    Pre-fail  Always       -       42\n\
197 Current_Pending_Sector  0x0012   100   100   000    Old_age   Always       -       8\n";
        let want = Smart { life_used: Some(3), power_on_hours: Some(21345), remapped: Some(0), pending: Some(8), ..Smart::default() };
        assert_eq!(parse_smart(&ata).smart, want);
        // Hours with minutes after them (some Seagate and WD drives).
        let c = parse_smart(&ata.replace("21345", "1234h+05m+12.345s"));
        assert_eq!(c.smart.power_on_hours, Some(1234));
        let nvme = "SMART overall-health self-assessment test result: PASSED\n\
Percentage Used:                    4%\nData Units Written:                 9,876,543 [5.05 TB]\n\
Power On Hours:                     5,678\nMedia and Data Integrity Errors:    0\n";
        let want = Smart {
            life_used: Some(4),
            power_on_hours: Some(5678),
            written: Some(9_876_543 * 512_000),
            media_errors: Some(0),
            ..Smart::default()
        };
        assert_eq!(parse_smart(nvme).smart, want);
        let scsi = "SMART Health Status: OK\nPercentage used endurance indicator: 2%\nElements in grown defect list: 7\n\
Accumulated power on time, hours:minutes 31234:56\n";
        let want = Smart { life_used: Some(2), power_on_hours: Some(31234), remapped: Some(7), ..Smart::default() };
        assert_eq!(parse_smart(scsi).smart, want);
        assert_eq!(parse_smart("SMART overall-health self-assessment test result: PASSED\n").smart, Smart::default());
    }

    #[test]
    fn smartctl_says_why_there_is_no_health() {
        let asleep = "smartctl 7.4\n\nDevice is in STANDBY mode, exit(2)\n";
        assert_eq!(parse_smart(asleep), Check::why(Why::Asleep));
        let denied = "Smartctl open device: /dev/sda failed: Permission denied\n";
        assert_eq!(parse_smart(denied), Check::why(Why::NeedsRoot));
        let raw = "Read Device Identity failed: Operation not permitted\n";
        assert_eq!(parse_smart(raw), Check::why(Why::NeedsRoot));
        let none = "SMART support is: Unavailable - device lacks SMART capability.\n";
        assert_eq!(parse_smart(none), Check::why(Why::Unsupported));
        assert_eq!(parse_smart("/dev/vda: Unable to detect device type\n"), Check::why(Why::Unsupported));
    }

    #[test]
    fn rates_and_busy_come_from_the_last_counts() {
        let d = |r: u64, w: u64| Drive { id: "sda".into(), read_total: Some(r), write_total: Some(w), ..Drive::default() };
        let mut list = vec![(d(3000, 5000), 1500), (Drive { id: "new".into(), ..Drive::default() }, 0)];
        let last = HashMap::from([("sda".to_string(), (1000, 6000, 1000))]);
        rates(&mut list, &last, Some(2.0));
        assert_eq!(list[0].0.read_rate, 1000.0);
        // A count that went back (the drive was reset) reads as 0.
        assert_eq!(list[0].0.write_rate, 0.0);
        assert_eq!(list[0].0.busy, Some(25.0));
        assert_eq!(list[1].0.busy, None);
        // Busy is capped: io_ticks can run ahead of the clock.
        let mut list = vec![(d(0, 0), 9000)];
        rates(&mut list, &HashMap::from([("sda".to_string(), (0, 0, 0))]), Some(1.0));
        assert_eq!(list[0].0.busy, Some(100.0));
    }
}
