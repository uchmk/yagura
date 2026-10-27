//! Windows: the `PhysicalDisk` performance counters (PDH) for the traffic and
//! how busy each drive is, and `IOCTL_STORAGE_QUERY_PROPERTY` for what each
//! drive is, asked once per drive, and how hot a solid-state one is, asked every few
//! seconds. The health check asks `IOCTL_STORAGE_PREDICT_FAILURE` once a
//! minute, after making sure the drive is not spun down, and an NVMe drive's
//! SMART / health log too (the wear, the hours, the writes, the errors), or
//! a SATA drive's SMART data (which wants an administrator). The folding is
//! in `windows.rs`.

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::time::{Duration, Instant};

use ::windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use ::windows::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use ::windows::Win32::System::IO::DeviceIoControl;
use ::windows::Win32::System::Ioctl::{
    DEVICE_SEEK_PENALTY_DESCRIPTOR, DISK_GEOMETRY_EX, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_STORAGE_PREDICT_FAILURE,
    IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, SMART_RCV_DRIVE_DATA, STORAGE_PREDICT_FAILURE, STORAGE_PROPERTY_ID,
    STORAGE_PROPERTY_QUERY, StorageDeviceProperty, StorageDeviceSeekPenaltyProperty, StorageDeviceTemperatureProperty,
};
use ::windows::Win32::System::Performance::{
    PDH_HCOUNTER, PDH_HQUERY, PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhOpenQueryW,
};
use ::windows::Win32::System::Power::GetDevicePowerState;
use ::windows::core::{BOOL, HRESULT, HSTRING, PCWSTR, w};

use super::health::{Ask, Check};
use super::windows::{
    ATA_DATA_LEN, Info, describe, fold, nvme_query, nvme_reply, parse_instance, predict, smart_query, smart_reply, temperature, why_of,
};
use super::{Drive, Health, Kind, Smart, Temp, Why, ata_attrs, nvme_log};
use crate::pdh::array;

/// How often a solid-state drive's temperature is asked.
const TEMP_EVERY: Duration = Duration::from_secs(5);

pub struct Reader {
    query: PDH_HQUERY,
    read: PDH_HCOUNTER,
    write: PDH_HCOUNTER,
    idle: Option<PDH_HCOUNTER>,
    /// What each drive said about itself, by disk number.
    info: HashMap<u32, Info>,
    /// The last temperatures, by disk number, and when they were asked.
    temps: HashMap<u32, Option<Temp>>,
    temp_at: Option<Instant>,
}

impl Reader {
    /// `None` when the counters are not there (disabled with `diskperf -n`
    /// or `lodctr /D:PerfDisk`).
    pub fn open() -> Option<Self> {
        let mut query = PDH_HQUERY(null_mut());
        // SAFETY: plain out-parameter calls; the handles are closed in Drop.
        unsafe {
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }
        }
        let add = |path: PCWSTR| {
            let mut h = PDH_HCOUNTER(null_mut());
            // SAFETY: as above.
            let ok = unsafe { PdhAddEnglishCounterW(query, path, 0, &mut h) } == 0;
            ok.then_some(h)
        };
        let (Some(read), Some(write)) =
            (add(w!("\\PhysicalDisk(*)\\Disk Read Bytes/sec")), add(w!("\\PhysicalDisk(*)\\Disk Write Bytes/sec")))
        else {
            // SAFETY: the query was opened above.
            unsafe { PdhCloseQuery(query) };
            return None;
        };
        let idle = add(w!("\\PhysicalDisk(*)\\% Idle Time"));
        // A rate counter needs a first collection to measure from.
        // SAFETY: as above.
        unsafe { PdhCollectQueryData(query) };
        Some(Self { query, read, write, idle, info: HashMap::new(), temps: HashMap::new(), temp_at: None })
    }

    pub fn read(&mut self) -> Vec<Drive> {
        // SAFETY: the query is open while self lives.
        if unsafe { PdhCollectQueryData(self.query) } != 0 {
            return Vec::new();
        }
        let read = array(self.read);
        for (inst, _) in &read {
            if let Some((index, _)) = parse_instance(inst) {
                self.info.entry(index).or_insert_with(|| query(index));
            }
        }
        let idle = self.idle.map(array).unwrap_or_default();
        let mut drives = fold(&read, &array(self.write), &idle, &self.info);
        self.temps(&mut drives);
        drives
    }

    /// Each solid-state drive's temperature, asked every few seconds. Any
    /// other drive's (spinning, or not saying) comes with the health check
    /// instead, which leaves a spun-down drive asleep.
    fn temps(&mut self, drives: &mut [Drive]) {
        let now = Instant::now();
        if self.temp_at.is_none_or(|t| now.duration_since(t) >= TEMP_EVERY) {
            self.temp_at = Some(now);
            self.temps = drives
                .iter()
                .filter(|d| d.kind == Kind::Ssd)
                .filter_map(|d| d.id.parse().ok())
                .map(|i| (i, Disk::open(i).and_then(|h| temp(&h))))
                .collect();
        }
        for d in drives {
            d.temp = d.id.parse().ok().and_then(|i: u32| self.temps.get(&i).cloned().flatten());
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        // SAFETY: opened in `open`, closed once here (it frees the counters too).
        unsafe { PdhCloseQuery(self.query) };
    }
}

/// `\\.\PhysicalDriveN`, opened with no access rights: that needs no
/// administrator, and what the drive will not say then stays empty.
struct Disk(HANDLE);

impl Disk {
    fn open(index: u32) -> Option<Self> {
        Self::open_with(index, 0).ok()
    }

    /// Opened to read and write, as ATA commands need: only an
    /// administrator may. The error is the Win32 one.
    fn open_rw(index: u32) -> Result<Self, u32> {
        Self::open_with(index, GENERIC_READ.0 | GENERIC_WRITE.0)
    }

    fn open_with(index: u32, access: u32) -> Result<Self, u32> {
        let path = HSTRING::from(format!("\\\\.\\PhysicalDrive{index}"));
        // SAFETY: a plain open; the handle is closed in Drop.
        let h = unsafe {
            CreateFileW(&path, access, FILE_SHARE_READ | FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_FLAGS_AND_ATTRIBUTES(0), None)
        };
        h.map(Self).map_err(|e| win32(e.code()))
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        // SAFETY: opened in `open`, closed once here.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Ask a drive what it is.
fn query(index: u32) -> Info {
    let Some(disk) = Disk::open(index) else { return Info::default() };
    let h = disk.0;
    let mut info = Info::default();
    let mut buf = vec![0u8; 1024];
    if let Some(n) = property(h, StorageDeviceProperty, &mut buf)
        && let Some((model, bus, removable)) = describe(&buf[..n])
    {
        (info.model, info.bus, info.removable) = (model, bus, removable);
    }
    let mut seek = DEVICE_SEEK_PENALTY_DESCRIPTOR::default();
    // SAFETY: the descriptor is a plain struct of its own size.
    let seek_buf = unsafe { std::slice::from_raw_parts_mut((&raw mut seek).cast::<u8>(), size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>()) };
    if property(h, StorageDeviceSeekPenaltyProperty, seek_buf).is_some_and(|n| n >= size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>()) {
        info.kind = if seek.IncursSeekPenalty { Kind::Hdd } else { Kind::Ssd };
    }
    // DISK_GEOMETRY_EX ends in partition and detection data; room for them.
    let mut geo = vec![0u64; 32];
    let mut got = 0u32;
    // SAFETY: the buffer is 256 bytes, aligned for the struct.
    let ok = unsafe {
        DeviceIoControl(
            h,
            IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
            None,
            0,
            Some(geo.as_mut_ptr().cast::<c_void>()),
            (geo.len() * 8) as u32,
            Some(&mut got),
            None,
        )
    }
    .is_ok();
    if ok && got as usize >= size_of::<DISK_GEOMETRY_EX>() - 8 {
        // SAFETY: Windows filled at least the fixed part.
        let size = unsafe { (*geo.as_ptr().cast::<DISK_GEOMETRY_EX>()).DiskSize };
        info.size = u64::try_from(size).ok().filter(|&s| s > 0);
    }
    info
}

/// The drive's temperature, where its driver answers for it (NVMe and most
/// SATA drives on the inbox drivers).
fn temp(disk: &Disk) -> Option<Temp> {
    let mut buf = vec![0u8; 256];
    let n = property(disk.0, StorageDeviceTemperatureProperty, &mut buf)?;
    let (value, high, crit) = temperature(&buf[..n])?;
    Some(Temp { value, high, crit, source: "IOCTL_STORAGE_QUERY_PROPERTY".into(), chip: None })
}

/// The Win32 error inside an HRESULT (`0x8007xxxx`).
fn win32(code: HRESULT) -> u32 {
    let c = code.0 as u32;
    if c & 0xFFFF_0000 == 0x8007_0000 { c & 0xFFFF } else { c }
}

/// The health of each drive, for the health thread.
pub fn check(asks: &[Ask]) -> HashMap<String, Check> {
    asks.iter().map(|a| (a.id.clone(), check_one(&a.id, a.bus))).collect()
}

fn check_one(id: &str, bus: Option<&str>) -> Check {
    let Some(disk) = id.parse().ok().and_then(Disk::open) else { return Check::why(Why::Unsupported) };
    let mut on = BOOL(1);
    // SAFETY: an open handle and a BOOL to write; asking does not spin the drive up.
    if unsafe { GetDevicePowerState(disk.0, &mut on) }.as_bool() && !on.as_bool() {
        return Check::why(Why::Asleep);
    }
    let mut out = STORAGE_PREDICT_FAILURE::default();
    let mut got = 0u32;
    // SAFETY: the output is a plain struct of its own size; no input.
    let asked = unsafe {
        DeviceIoControl(
            disk.0,
            IOCTL_STORAGE_PREDICT_FAILURE,
            None,
            0,
            Some((&raw mut out).cast::<c_void>()),
            size_of::<STORAGE_PREDICT_FAILURE>() as u32,
            Some(&mut got),
            None,
        )
    };
    let mut c = match asked {
        Ok(()) => Check { health: predict(&out.PredictFailure.to_le_bytes()), ..Check::default() },
        Err(e) => Check::why(why_of(win32(e.code()))),
    };
    c.temp = temp(&disk);
    if bus == Some("NVMe")
        && let Some(log) = nvme_health(&disk)
    {
        c.add_log(log, "NVMe health log");
    }
    if matches!(bus, Some("SATA" | "ATA")) {
        match id.parse().map_err(|_| Why::Unsupported).and_then(ata_smart) {
            Ok(data) => {
                if let Some((temp, smart)) = ata_attrs(&data) {
                    c.add_counts(temp, smart, "ATA SMART");
                }
            }
            // The health row says so already when it is the same reason.
            Err(Why::NeedsAdmin) if c.why != Some(Why::NeedsAdmin) => c.smart.why = Some(Why::NeedsAdmin),
            Err(_) => {}
        }
    }
    c
}

/// A SATA drive's SMART data through `SMART_RCV_DRIVE_DATA`, on a handle
/// opened to read and write: without an administrator, `NeedsAdmin`.
fn ata_smart(index: u32) -> Result<Vec<u8>, Why> {
    let disk = Disk::open_rw(index).map_err(why_of)?;
    let query = smart_query();
    let mut out = vec![0u8; 16 + ATA_DATA_LEN];
    let mut got = 0u32;
    // SAFETY: the packed query and the answer's buffer outlive the call; their
    // sizes are given.
    unsafe {
        DeviceIoControl(
            disk.0,
            SMART_RCV_DRIVE_DATA,
            Some(query.as_ptr().cast::<c_void>()),
            query.len() as u32,
            Some(out.as_mut_ptr().cast::<c_void>()),
            out.len() as u32,
            Some(&mut got),
            None,
        )
    }
    .map_err(|e| why_of(win32(e.code())))?;
    smart_reply(out.get(..got as usize).ok_or(Why::Unsupported)?).map(<[u8]>::to_vec).ok_or(Why::Unsupported)
}

/// An NVMe drive's SMART / health log, through the protocol-specific
/// property: the inbox driver answers without an administrator.
fn nvme_health(disk: &Disk) -> Option<(Health, Option<f32>, Smart)> {
    let mut buf = nvme_query();
    let mut got = 0u32;
    let len = buf.len() as u32;
    // SAFETY: the one buffer is the query going in and the answer coming out,
    // as the call allows; it outlives the call and its size is given.
    unsafe {
        DeviceIoControl(
            disk.0,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(buf.as_ptr().cast::<c_void>()),
            len,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            len,
            Some(&mut got),
            None,
        )
    }
    .ok()?;
    nvme_log(nvme_reply(buf.get(..got as usize)?)?)
}

/// One `IOCTL_STORAGE_QUERY_PROPERTY` into `out`; the bytes written.
fn property(h: HANDLE, id: STORAGE_PROPERTY_ID, out: &mut [u8]) -> Option<usize> {
    let q = STORAGE_PROPERTY_QUERY { PropertyId: id, QueryType: PropertyStandardQuery, ..Default::default() };
    let mut got = 0u32;
    // SAFETY: the query and the buffer outlive the call; their sizes are given.
    unsafe {
        DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some((&raw const q).cast::<c_void>()),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(out.as_mut_ptr().cast::<c_void>()),
            out.len() as u32,
            Some(&mut got),
            None,
        )
    }
    .ok()?;
    Some(got as usize)
}
