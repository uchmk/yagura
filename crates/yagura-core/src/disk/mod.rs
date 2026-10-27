//! Disks: the physical drives with their traffic and how busy they are, and
//! the mounted volumes with their space and traffic. Read every tick, so the
//! graphs have a minute behind them when the disk screen opens.
//!
//! The volumes come from sysinfo on every OS. The drives come from
//! `/proc/diskstats` and `/sys/block` on Linux, and from the `PhysicalDisk`
//! performance counters (PDH) on Windows with each drive's model, bus and
//! kind asked of it once, and from IOKit's block storage drivers on macOS.
//! Each drive's temperature comes with the reading where the OS has one at
//! hand (hwmon on Linux, `IOCTL_STORAGE_QUERY_PROPERTY` on Windows); its SMART
//! health is asked once a minute in a thread of its own (`health.rs`).
//! The parsing is kept in pure functions so the tests run everywhere.

mod health;
#[cfg(windows)]
mod ioctl;
#[cfg(target_os = "macos")]
mod iokit;
#[cfg(any(target_os = "linux", test))]
pub mod linux;
#[cfg(any(target_os = "macos", test))]
pub mod mac;
#[cfg(any(windows, test))]
pub mod windows;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sysinfo::DiskRefreshKind;

/// Solid state or spinning, where the OS says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Ssd,
    Hdd,
    #[default]
    Unknown,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Self::Ssd, Self::Hdd, Self::Unknown];

    pub fn key(self) -> &'static str {
        match self {
            Self::Ssd => "disk.kind.ssd",
            Self::Hdd => "disk.kind.hdd",
            Self::Unknown => "disk.kind.unknown",
        }
    }

    fn from_sysinfo(k: sysinfo::DiskKind) -> Self {
        match k {
            sysinfo::DiskKind::SSD => Self::Ssd,
            sysinfo::DiskKind::HDD => Self::Hdd,
            sysinfo::DiskKind::Unknown(_) => Self::Unknown,
        }
    }
}

/// Why the drive list is empty or short.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Note {
    /// Another OS: the drives are not read; each volume shows its own
    /// traffic.
    #[cfg_attr(any(windows, target_os = "linux", target_os = "macos"), allow(dead_code))]
    NotRead,
    /// Windows: the `PhysicalDisk` counters are not there (disabled).
    #[cfg_attr(not(windows), allow(dead_code))]
    CountersOff,
    /// Linux: `/proc/diskstats` could not be read.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    NoDiskstats,
    /// macOS: IOKit could not list the block storage drivers.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    NoIoKit,
}

impl Note {
    pub const ALL: [Note; 4] = [Self::NotRead, Self::CountersOff, Self::NoDiskstats, Self::NoIoKit];

    pub fn key(self) -> &'static str {
        match self {
            Self::NotRead => "disk.note.not_read",
            Self::CountersOff => "disk.note.counters_off",
            Self::NoDiskstats => "disk.note.no_diskstats",
            Self::NoIoKit => "disk.note.no_iokit",
        }
    }
}

/// What SMART says of a drive as a whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Health {
    Good,
    /// The drive predicts its own failure, or has a critical warning.
    Failing,
}

impl Health {
    pub const ALL: [Health; 2] = [Self::Good, Self::Failing];

    pub fn key(self) -> &'static str {
        match self {
            Self::Good => "disk.health.good",
            Self::Failing => "disk.health.failing",
        }
    }
}

/// Why a drive has no health.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Why {
    /// Not asked yet: the first check is under way.
    Pending,
    /// The drive keeps no SMART data: a virtual disk, a card, a RAID volume.
    Unsupported,
    /// Linux: `smartctl` is not installed.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    NoSmartctl,
    /// Linux: the device node can be opened only by root (or the `disk` group).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    NeedsRoot,
    /// Windows: the drive answers only an administrator.
    #[cfg_attr(not(windows), allow(dead_code))]
    NeedsAdmin,
    /// The drive was spun down and is left asleep (Linux and Windows).
    #[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
    Asleep,
    /// Another OS: SMART is not read.
    #[cfg_attr(any(windows, target_os = "linux", target_os = "macos"), allow(dead_code))]
    NotRead,
}

impl Why {
    pub const ALL: [Why; 7] =
        [Self::Pending, Self::Unsupported, Self::NoSmartctl, Self::NeedsRoot, Self::NeedsAdmin, Self::Asleep, Self::NotRead];

    pub fn key(self) -> &'static str {
        match self {
            Self::Pending => "disk.why.pending",
            Self::Unsupported => "disk.why.unsupported",
            Self::NoSmartctl => "disk.why.no_smartctl",
            Self::NeedsRoot => "disk.why.needs_root",
            Self::NeedsAdmin => "disk.why.needs_admin",
            Self::Asleep => "disk.why.asleep",
            Self::NotRead => "disk.why.not_read",
        }
    }
}

/// A drive's temperature in degrees Celsius.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Temp {
    pub value: f32,
    /// Where the drive warns.
    pub high: Option<f32>,
    /// Where the drive throttles hard or shuts down.
    pub crit: Option<f32>,
    /// Where it was read: a sysfs file, an IOCTL, `smartctl`.
    pub source: String,
    /// The sensors chip (`hwmon3`) that already lists it, so the sensors
    /// screen does not list it twice.
    pub chip: Option<String>,
}

/// What a drive's SMART data says beyond its health: how worn it is and the
/// counts that go up before a drive fails. Each is `None` where the drive
/// keeps no such count.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Smart {
    /// Percent of the rated endurance used (NVMe can go past 100).
    pub life_used: Option<u32>,
    pub power_on_hours: Option<u64>,
    /// Bytes the host wrote over the drive's life.
    pub written: Option<u64>,
    /// Sectors moved to spares (ATA attribute 5, SCSI's grown defects).
    pub remapped: Option<u64>,
    /// Sectors waiting to be remapped (ATA attribute 197).
    pub pending: Option<u64>,
    /// NVMe: unrecovered data integrity errors.
    pub media_errors: Option<u64>,
    /// Why there are no counts where the drive keeps them (Windows: a SATA
    /// drive answers only an administrator).
    pub why: Option<Why>,
}

/// The SMART / health information log of an NVMe drive (log page 2, 512
/// bytes, the same on every OS): the critical warning at 0, the composite
/// temperature in kelvin at 1, percentage used at 5, and 16-byte counters
/// from 32: data units written (thousands of 512-byte blocks) at 48, power-on
/// hours at 128 and media errors at 160.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub fn nvme_log(buf: &[u8]) -> Option<(Health, Option<f32>, Smart)> {
    if buf.len() < 192 {
        return None;
    }
    let n128 = |i: usize| u128::from_le_bytes(buf[i..i + 16].try_into().unwrap_or_default());
    let n64 = |i: usize| u64::try_from(n128(i)).unwrap_or(u64::MAX);
    let health = if buf[0] == 0 { Health::Good } else { Health::Failing };
    let kelvin = u16::from_le_bytes([buf[1], buf[2]]);
    let temp = (kelvin > 0).then(|| f32::from(kelvin) - 273.0).filter(|t| (-60.0..250.0).contains(t));
    let smart = Smart {
        life_used: Some(u32::from(buf[5])),
        power_on_hours: Some(n64(128)),
        written: Some(u64::try_from(n128(48).saturating_mul(512_000)).unwrap_or(u64::MAX)),
        media_errors: Some(n64(160)),
        ..Smart::default()
    };
    Some((health, temp, smart))
}

/// ATA attributes whose normalized value is the life left of an SSD, in
/// percent, by ID, most specific first: `SSD_Life_Left`, Intel's
/// `Media_Wearout_Indicator`, Samsung's `Wear_Leveling_Count`, Micron's
/// `Percent_Lifetime_Remain`, `Remaining_Lifetime_Perc`.
const ATA_LIFE_LEFT: [u8; 5] = [231, 233, 177, 202, 169];

/// The attribute table of an ATA drive's SMART data (512 bytes from SMART
/// READ DATA, the same on every OS): 30 entries of 12 bytes from offset 2,
/// each an ID, two flag bytes, the normalized and worst values, six raw
/// bytes and a reserved one. The temperature is attribute 194 (190 where
/// there is no 194), in the raw value's low byte; the counts are 5
/// (remapped), 9 (hours) and 197 (pending), and the life used comes from the
/// first of `ATA_LIFE_LEFT` the drive has. Names are not in the data, so the
/// IDs are read as smartctl's default names for them.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub fn ata_attrs(buf: &[u8]) -> Option<(Option<f32>, Smart)> {
    let table = buf.get(2..2 + 30 * 12)?;
    let attrs: Vec<&[u8; 12]> = table.as_chunks::<12>().0.iter().filter(|a| a[0] != 0).collect();
    if attrs.is_empty() {
        return None;
    }
    let find = |id: u8| attrs.iter().find(|a| a[0] == id);
    let raw = |id: u8| find(id).map(|a| u64::from_le_bytes([a[5], a[6], a[7], a[8], a[9], a[10], 0, 0]));
    // Drives put the lowest and highest seen in the upper raw bytes.
    let temp = |id: u8| find(id).map(|a| f32::from(a[5])).filter(|t| *t > 0.0 && *t < 125.0);
    let life_left = ATA_LIFE_LEFT.iter().find_map(|&id| find(id)).map(|a| a[3]).filter(|&v| v <= 100);
    let smart = Smart {
        life_used: life_left.map(|left| 100 - u32::from(left)),
        // Some drives keep minutes or milliseconds in the upper bytes.
        power_on_hours: raw(9).map(|h| h & 0xFFFF_FFFF),
        remapped: raw(5),
        pending: raw(197),
        ..Smart::default()
    };
    Some((temp(194).or_else(|| temp(190)), smart))
}

/// A physical drive (or what the OS shows as one: a virtual disk, an LVM or
/// RAID device).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Drive {
    /// `sda`, `nvme0n1`; Windows' disk number.
    pub id: String,
    /// `sda`; `Disk 0` on Windows.
    pub name: String,
    /// The model the drive reports; empty when there is none.
    pub model: String,
    pub kind: Kind,
    #[serde(deserialize_with = "crate::wire::opt_name")]
    pub bus: Option<crate::Name>,
    pub size: Option<u64>,
    pub removable: bool,
    /// Bytes since boot, where the OS keeps a count (not Windows).
    pub read_total: Option<u64>,
    pub write_total: Option<u64>,
    /// Bytes per second since the last reading; 0 on the first.
    pub read_rate: f64,
    pub write_rate: f64,
    /// Percent of the time the drive had work.
    pub busy: Option<f32>,
    /// The mount points of the volumes on it.
    pub volumes: Vec<String>,
    pub temp: Option<Temp>,
    pub health: Option<Health>,
    /// Why there is no health.
    pub why: Option<Why>,
    pub smart: Smart,
}

/// The life used from which the disk screen draws it yellow.
pub const WORN: u32 = 80;

impl Smart {
    /// What the disk screen draws yellow or red: worn to [`WORN`], or any
    /// remapped, pending or media error.
    pub fn warns(&self) -> bool {
        self.life_used.is_some_and(|p| p >= WORN) || [self.remapped, self.pending, self.media_errors].into_iter().flatten().any(|n| n > 0)
    }
}

impl Drive {
    /// Failing, or its SMART counts warn; the fleet card counts these (a hot
    /// drive is counted with the temperatures).
    pub fn warns(&self) -> bool {
        self.health == Some(Health::Failing) || self.smart.warns()
    }
}

/// A mounted file system.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Volume {
    /// `/`, `C:\`.
    pub mount: String,
    /// The device (`/dev/sda2`) or the label (`Windows`).
    pub name: String,
    pub fs: String,
    pub total: u64,
    pub avail: u64,
    pub kind: Kind,
    pub removable: bool,
    pub read_only: bool,
    /// Bytes per second since the last reading; 0 on the first.
    pub read_rate: f64,
    pub write_rate: f64,
    /// The [`Drive::id`] it is on, when known.
    pub drive: Option<String>,
}

impl Volume {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.avail)
    }

    /// Whether `name` is the volume's label rather than its device: a drive
    /// letter's (Windows), told by the mount so a viewer on another OS sees
    /// the same.
    pub fn labelled(&self) -> bool {
        self.mount.as_bytes().get(1) == Some(&b':')
    }

    /// Percent used; `None` for an empty size.
    pub fn use_pct(&self) -> Option<f32> {
        (self.total > 0).then(|| self.used() as f32 * 100.0 / self.total as f32)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Disks {
    pub drives: Vec<Drive>,
    pub volumes: Vec<Volume>,
    pub note: Option<Note>,
}

impl Disks {
    /// Bytes per second read and written, over the drives (or the volumes,
    /// where there are no drives).
    pub fn rates(&self) -> (f64, f64) {
        if self.drives.is_empty() {
            self.volumes.iter().fold((0.0, 0.0), |(r, w), v| (r + v.read_rate, w + v.write_rate))
        } else {
            self.drives.iter().fold((0.0, 0.0), |(r, w), d| (r + d.read_rate, w + d.write_rate))
        }
    }
}

/// Fill in each drive's volumes from the volumes' drives.
fn link(drives: &mut [Drive], volumes: &[Volume]) {
    for d in drives {
        d.volumes = volumes.iter().filter(|v| v.drive.as_deref() == Some(d.id.as_str())).map(|v| v.mount.clone()).collect();
    }
}

/// Reads the drives and the volumes and works out their rates.
pub struct Reader {
    vols: sysinfo::Disks,
    /// The mount points read last time: a new one has no rate yet (its first
    /// count is everything since boot).
    seen: HashSet<PathBuf>,
    vol_at: Option<Instant>,
    #[cfg(target_os = "linux")]
    linux: linux::Reader,
    #[cfg(windows)]
    win: Option<ioctl::Reader>,
    #[cfg(target_os = "macos")]
    mac: iokit::Reader,
    health: health::Checker,
}

impl Reader {
    pub fn new() -> Self {
        Self {
            vols: sysinfo::Disks::new_with_refreshed_list_specifics(DiskRefreshKind::everything()),
            seen: HashSet::new(),
            vol_at: None,
            #[cfg(target_os = "linux")]
            linux: linux::Reader::default(),
            #[cfg(windows)]
            win: ioctl::Reader::open(),
            #[cfg(target_os = "macos")]
            mac: iokit::Reader::default(),
            health: health::Checker::default(),
        }
    }

    pub fn read(&mut self) -> Disks {
        let (mut drives, note) = self.drives();
        self.health.apply(&mut drives);
        let mut volumes = self.volumes();
        for v in &mut volumes {
            v.drive = self.drive_of(v, &drives);
        }
        link(&mut drives, &volumes);
        Disks { drives, volumes, note }
    }

    fn volumes(&mut self) -> Vec<Volume> {
        // The kind is read once per new disk by sysinfo itself.
        self.vols.refresh_specifics(true, DiskRefreshKind::nothing().with_kind().with_storage().with_io_usage());
        let now = Instant::now();
        let secs = self.vol_at.map(|t| now.duration_since(t).as_secs_f64()).filter(|&s| s > 0.0);
        let mut out: Vec<Volume> = self
            .vols
            .list()
            .iter()
            .map(|d| {
                let u = d.usage();
                let known = self.seen.contains(d.mount_point());
                let rate = |b: u64| match secs {
                    Some(s) if known => b as f64 / s,
                    _ => 0.0,
                };
                Volume {
                    mount: d.mount_point().display().to_string(),
                    name: d.name().to_string_lossy().into_owned(),
                    fs: d.file_system().to_string_lossy().into_owned(),
                    total: d.total_space(),
                    avail: d.available_space(),
                    kind: Kind::from_sysinfo(d.kind()),
                    removable: d.is_removable(),
                    read_only: d.is_read_only(),
                    read_rate: rate(u.read_bytes),
                    write_rate: rate(u.written_bytes),
                    drive: None,
                }
            })
            .collect();
        self.seen = self.vols.list().iter().map(|d| d.mount_point().to_path_buf()).collect();
        self.vol_at = Some(now);
        out.sort_by(|a, b| a.mount.cmp(&b.mount));
        out
    }

    #[cfg(target_os = "linux")]
    fn drives(&mut self) -> (Vec<Drive>, Option<Note>) {
        self.linux.read(std::path::Path::new("/sys/block"))
    }

    #[cfg(windows)]
    fn drives(&mut self) -> (Vec<Drive>, Option<Note>) {
        match &mut self.win {
            Some(r) => (r.read(), None),
            None => (Vec::new(), Some(Note::CountersOff)),
        }
    }

    #[cfg(target_os = "macos")]
    fn drives(&mut self) -> (Vec<Drive>, Option<Note>) {
        self.mac.read()
    }

    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    fn drives(&mut self) -> (Vec<Drive>, Option<Note>) {
        (Vec::new(), Some(Note::NotRead))
    }

    /// Which drive a volume is on: by the device it was mounted from on
    /// Linux, by its drive letter on Windows, by walking IOKit up from its
    /// device on macOS.
    #[cfg(target_os = "linux")]
    fn drive_of(&mut self, v: &Volume, drives: &[Drive]) -> Option<String> {
        // `/dev/disk/by-uuid/…` and `/dev/mapper/…` are links to the real node.
        let dev = std::fs::canonicalize(&v.name).ok()?;
        let name = dev.strip_prefix("/dev").ok()?.to_str()?;
        let ids: Vec<&str> = drives.iter().map(|d| d.id.as_str()).collect();
        linux::owner(name, &ids).map(str::to_string)
    }

    #[cfg(windows)]
    fn drive_of(&mut self, v: &Volume, drives: &[Drive]) -> Option<String> {
        let letter = windows::letter(&v.mount)?;
        drives.iter().find(|d| d.volumes.iter().any(|l| l.eq_ignore_ascii_case(&letter))).map(|d| d.id.clone())
    }

    #[cfg(target_os = "macos")]
    fn drive_of(&mut self, v: &Volume, drives: &[Drive]) -> Option<String> {
        // The drive must still be listed: one unplugged since keeps no volumes.
        self.mac.drive_of(&v.mount).filter(|id| drives.iter().any(|d| &d.id == id))
    }

    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    fn drive_of(&mut self, _: &Volume, _: &[Drive]) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::tr;

    #[test]
    fn a_drive_warns_when_failing_worn_or_counting_errors() {
        let d = |health, smart| Drive { health, smart, ..Drive::default() };
        assert!(!d(Some(Health::Good), Smart { life_used: Some(79), remapped: Some(0), ..Smart::default() }).warns());
        assert!(d(Some(Health::Failing), Smart::default()).warns());
        assert!(d(None, Smart { life_used: Some(WORN), ..Smart::default() }).warns());
        assert!(d(None, Smart { pending: Some(1), ..Smart::default() }).warns());
        assert!(d(None, Smart { media_errors: Some(2), ..Smart::default() }).warns());
    }

    #[test]
    fn every_kind_note_health_and_reason_has_english() {
        for k in Kind::ALL {
            assert_ne!(tr(k.key()), k.key());
        }
        for n in Note::ALL {
            assert_ne!(tr(n.key()), n.key());
        }
        for h in Health::ALL {
            assert_ne!(tr(h.key()), h.key());
        }
        for w in Why::ALL {
            assert_ne!(tr(w.key()), w.key());
        }
    }

    #[test]
    fn an_nvme_health_log_gives_wear_hours_writes_and_errors() {
        let mut log = vec![0u8; 512];
        log[1..3].copy_from_slice(&314u16.to_le_bytes()); // 41 °C
        log[5] = 3;
        log[48..56].copy_from_slice(&10_000u64.to_le_bytes()); // 5.12 GB
        log[128..136].copy_from_slice(&5678u64.to_le_bytes());
        log[160] = 2;
        let (health, temp, smart) = nvme_log(&log).unwrap();
        assert_eq!((health, temp), (Health::Good, Some(41.0)));
        assert_eq!(
            smart,
            Smart {
                life_used: Some(3),
                power_on_hours: Some(5678),
                written: Some(5_120_000_000),
                media_errors: Some(2),
                ..Smart::default()
            }
        );
        log[0] = 0x04; // reliability degraded
        log[1..3].fill(0);
        let (health, temp, _) = nvme_log(&log).unwrap();
        assert_eq!((health, temp), (Health::Failing, None));
        assert_eq!(nvme_log(&log[..100]), None);
    }

    #[test]
    fn ata_smart_data_gives_temperature_wear_hours_and_sector_counts() {
        let mut data = vec![0u8; 512];
        let mut put = |slot: usize, id: u8, value: u8, raw: [u8; 6]| {
            let at = 2 + slot * 12;
            data[at] = id;
            data[at + 3] = value;
            data[at + 4] = value;
            data[at + 5..at + 11].copy_from_slice(&raw);
        };
        put(0, 5, 100, [7, 0, 0, 0, 0, 0]);
        put(1, 9, 99, [0x61, 0x53, 0, 0, 0x2C, 0x01]); // 21345 h, minutes above
        put(2, 177, 97, [42, 0, 0, 0, 0, 0]);
        put(3, 190, 64, [38, 0, 0, 0, 0, 0]);
        put(4, 194, 64, [36, 0, 21, 0, 49, 0]); // 36 °C, min 21, max 49
        put(5, 197, 100, [8, 0, 0, 0, 0, 0]);
        let (temp, smart) = ata_attrs(&data).unwrap();
        assert_eq!(temp, Some(36.0));
        let want = Smart { life_used: Some(3), power_on_hours: Some(21345), remapped: Some(7), pending: Some(8), ..Smart::default() };
        assert_eq!(smart, want);
        // Without 194 the airflow temperature stands in; a more specific
        // life attribute wins over Samsung's.
        data[2 + 4 * 12] = 0;
        data[2 + 6 * 12] = 231;
        data[2 + 6 * 12 + 3] = 90;
        let (temp, smart) = ata_attrs(&data).unwrap();
        assert_eq!((temp, smart.life_used), (Some(38.0), Some(10)));
        assert_eq!(ata_attrs(&[0u8; 512]), None, "an empty table is no data");
        assert_eq!(ata_attrs(&data[..100]), None);
    }

    #[test]
    fn a_drive_lists_the_volumes_on_it() {
        let mut drives = vec![Drive { id: "sda".into(), ..Drive::default() }, Drive { id: "sdb".into(), ..Drive::default() }];
        let vol = |m: &str, d: Option<&str>| Volume { mount: m.into(), drive: d.map(Into::into), ..Volume::default() };
        let volumes = [vol("/", Some("sda")), vol("/home", Some("sda")), vol("/mnt/usb", Some("sdb")), vol("/net", None)];
        link(&mut drives, &volumes);
        assert_eq!(drives[0].volumes, ["/", "/home"]);
        assert_eq!(drives[1].volumes, ["/mnt/usb"]);
    }

    #[test]
    fn rates_add_up_the_drives_or_else_the_volumes() {
        let mut d = Disks { volumes: vec![Volume { read_rate: 1.0, write_rate: 2.0, ..Volume::default() }; 2], ..Disks::default() };
        assert_eq!(d.rates(), (2.0, 4.0));
        d.drives = vec![Drive { read_rate: 10.0, write_rate: 20.0, ..Drive::default() }];
        assert_eq!(d.rates(), (10.0, 20.0));
    }

    #[test]
    fn a_volume_knows_how_full_it_is() {
        let v = Volume { total: 200, avail: 50, ..Volume::default() };
        assert_eq!(v.used(), 150);
        assert_eq!(v.use_pct(), Some(75.0));
        assert_eq!(Volume::default().use_pct(), None);
    }
}
