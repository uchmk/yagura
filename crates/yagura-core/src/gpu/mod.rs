//! GPUs and NPUs: usage, memory and sensors, read on the sampler thread.
//!
//! Each OS has its own source: sysfs on Linux (`/sys/class/drm` and
//! `/sys/class/accel`), the `GPU Engine` and `GPU Adapter Memory`
//! performance counters on Windows, and NVIDIA's NVML on both where the
//! driver ships it (loaded at run time; without it NVIDIA devices show what
//! the OS alone says), and IOKit's `IOAccelerator` services on macOS. What a
//! source cannot give is `None`, and the reason is kept beside it so the
//! screen can say why.

// Other OSes read nothing; the types are there for the screen.
#![cfg_attr(not(any(target_os = "linux", windows, target_os = "macos")), allow(dead_code))]

#[cfg(any(target_os = "linux", test))]
pub mod fdinfo;
#[cfg(target_os = "macos")]
mod ioaccel;
#[cfg(windows)]
mod kmt;
#[cfg(any(target_os = "linux", test))]
pub mod linux;
#[cfg(any(target_os = "macos", test))]
pub mod mac;
#[cfg(any(windows, target_os = "linux"))]
mod nvidia;
#[cfg(windows)]
mod pdh;
#[cfg(any(windows, test))]
pub mod windows;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    #[default]
    Gpu,
    Npu,
}

/// A reading a device may lack, for the screen's "not available" list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Field {
    Usage,
    Vram,
    Temp,
    Power,
    Clock,
}

impl Field {
    pub fn key(self) -> &'static str {
        match self {
            Self::Usage => "gpu.usage",
            Self::Vram => "gpu.vram",
            Self::Temp => "gpu.temp",
            Self::Power => "gpu.power",
            Self::Clock => "gpu.clock",
        }
    }
}

/// hwmon and D3DKMT give the speed, NVML a share of the most; macOS's AMD
/// driver either.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Fan {
    Rpm(u32),
    Percent(u32),
}

/// Why something is not shown. Each OS uses only its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)]
pub enum Why {
    /// An NVIDIA card without NVML (the driver's management library).
    NoNvml,
    /// The kernel driver keeps no busy counter in sysfs (i915, xe, nouveau).
    NoBusy,
    /// No hwmon sensor for it.
    NoSensor,
    /// Windows: the driver gives D3DKMT no such reading; NVML fills them for
    /// NVIDIA.
    WinSensors,
    /// No memory of its own reported (integrated parts use system memory).
    NoVram,
    /// The NPU driver reports little beyond that it is there.
    NpuLittle,
    /// The accelerator's statistics carry no utilisation (macOS).
    MacStats,
    /// The accelerator's statistics carry no sensors (macOS).
    MacSensors,
    MacGpu,
    MacNpu,
    LinuxGpu,
    LinuxNpu,
    WinGpu,
    WinNpu,
    OtherOs,
}

impl Why {
    pub const ALL: [Why; 15] = [
        Self::NoNvml,
        Self::NoBusy,
        Self::NoSensor,
        Self::WinSensors,
        Self::NoVram,
        Self::NpuLittle,
        Self::MacStats,
        Self::MacSensors,
        Self::MacGpu,
        Self::MacNpu,
        Self::LinuxGpu,
        Self::LinuxNpu,
        Self::WinGpu,
        Self::WinNpu,
        Self::OtherOs,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::NoNvml => "gpu.why.no_nvml",
            Self::NoBusy => "gpu.why.no_busy",
            Self::NoSensor => "gpu.why.no_sensor",
            Self::WinSensors => "gpu.why.win_sensors",
            Self::NoVram => "gpu.why.no_vram",
            Self::NpuLittle => "gpu.why.npu_little",
            Self::MacStats => "gpu.why.mac_stats",
            Self::MacSensors => "gpu.why.mac_sensors",
            Self::MacGpu => "gpu.why.mac_gpu",
            Self::MacNpu => "gpu.why.mac_npu",
            Self::LinuxGpu => "gpu.why.linux_gpu",
            Self::LinuxNpu => "gpu.why.linux_npu",
            Self::WinGpu => "gpu.why.win_gpu",
            Self::WinNpu => "gpu.why.win_npu",
            Self::OtherOs => "gpu.why.other_os",
        }
    }
}

/// One process's share of a device.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GpuProc {
    pub pid: u32,
    /// Percent of the busiest engine it uses.
    pub usage: Option<f32>,
    /// Dedicated memory it holds, in bytes.
    pub mem: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Device {
    /// Stable while running: `drm:card0`, `accel:accel0`, `luid:…`, `nvml:0`.
    pub id: String,
    pub kind: Kind,
    /// As the driver names it; empty when it does not, and the screen says
    /// "vendor GPU" instead.
    pub name: String,
    #[serde(deserialize_with = "crate::wire::opt_name")]
    pub vendor: Option<crate::Name>,
    /// `1002:744c` where the PCI ids are known.
    pub pci: Option<String>,
    /// The PCI address, `0000:03:00.0`, where it is known (Linux, NVML).
    pub bus: Option<String>,
    pub driver: Option<String>,
    /// Where the readings came from: `sysfs`, `sysfs+fdinfo`, `pdh`,
    /// `pdh+d3dkmt`, `nvml`, `pdh+nvml`, `iokit`.
    #[serde(deserialize_with = "crate::wire::name")]
    pub source: crate::Name,
    /// Percent, the busiest engine's.
    pub usage: Option<f32>,
    /// Per engine type, percent, in a fixed order.
    pub engines: Vec<(String, f32)>,
    pub vram_used: Option<u64>,
    pub vram_total: Option<u64>,
    /// System memory the device maps (GTT on Linux, shared on Windows).
    pub shared_used: Option<u64>,
    pub shared_total: Option<u64>,
    /// °C.
    pub temp: Option<f32>,
    pub fan: Option<Fan>,
    /// Watts.
    pub power: Option<f32>,
    /// MHz.
    pub clock: Option<u32>,
    /// The processes using it, busiest first.
    pub procs: Vec<GpuProc>,
    pub why: Vec<(Field, Why)>,
}

impl Device {
    /// The reason `f` is missing, when one was given.
    #[cfg(test)]
    pub fn why(&self, f: Field) -> Option<Why> {
        self.why.iter().find(|(x, _)| *x == f).map(|&(_, w)| w)
    }

    pub fn vram_pct(&self) -> Option<f32> {
        match (self.vram_used, self.vram_total) {
            (Some(u), Some(t)) if t > 0 => Some((u as f64 * 100.0 / t as f64) as f32),
            _ => None,
        }
    }
}

/// The PCI vendor's name.
pub fn vendor_name(id: u32) -> Option<&'static str> {
    Some(match id {
        0x1002 | 0x1022 => "AMD",
        0x8086 => "Intel",
        0x10de => "NVIDIA",
        0x5143 | 0x17cb => "Qualcomm",
        0x106b => "Apple",
        0x13b5 => "Arm",
        0x1414 => "Microsoft",
        0x15ad => "VMware",
        0x1af4 | 0x1b36 => "Red Hat",
        0x1ab8 => "Parallels",
        0x80ee => "VirtualBox",
        _ => return None,
    })
}

/// A PCI address as sysfs writes it: NVML's eight-digit domain
/// (`00000000:01:00.0`) cut to four, in lower case.
#[cfg(any(windows, target_os = "linux", test))]
pub fn bus_addr(s: &str) -> String {
    let s = s.trim().to_ascii_lowercase();
    match s.split_once(':') {
        Some((dom, rest)) if dom.len() > 4 => format!("{}:{rest}", &dom[dom.len() - 4..]),
        _ => s,
    }
}

/// What is said where no GPU is listed: why, on this OS.
pub fn gpu_note() -> Why {
    if cfg!(target_os = "linux") {
        Why::LinuxGpu
    } else if cfg!(windows) {
        Why::WinGpu
    } else if cfg!(target_os = "macos") {
        Why::MacGpu
    } else {
        Why::OtherOs
    }
}

/// What is said where no NPU is listed.
pub fn npu_note() -> Why {
    if cfg!(target_os = "linux") {
        Why::LinuxNpu
    } else if cfg!(windows) {
        Why::WinNpu
    } else if cfg!(target_os = "macos") {
        Why::MacNpu
    } else {
        Why::OtherOs
    }
}

/// GPUs first, then NPUs, each in the order found.
fn order(mut devs: Vec<Device>) -> Vec<Device> {
    devs.sort_by_key(|d| d.kind == Kind::Npu);
    devs
}

/// Windows: the sensors NVML gives laid over the counters' NVIDIA adapters,
/// matched by name, else in order; what NVML lacks stays as D3DKMT gave it. NVML devices with no adapter are left out
/// (the counters list every adapter the OS drives).
#[cfg(any(windows, test))]
pub fn merge_nvml(devs: &mut [Device], nv: Vec<Device>) {
    let mut taken = vec![false; devs.len()];
    for n in nv {
        let nvidia = |d: &Device| d.vendor == Some("NVIDIA") || d.name.starts_with("NVIDIA");
        let by_name = devs.iter().enumerate().position(|(i, d)| !taken[i] && nvidia(d) && d.name == n.name);
        let first = || devs.iter().enumerate().position(|(i, d)| !taken[i] && nvidia(d));
        let Some(i) = by_name.or_else(first) else { continue };
        taken[i] = true;
        let d = &mut devs[i];
        d.temp = n.temp.or(d.temp);
        d.fan = n.fan.or(d.fan);
        d.power = n.power.or(d.power);
        d.clock = n.clock.or(d.clock);
        d.source = "pdh+nvml";
        d.why.retain(|(f, _)| !matches!(f, Field::Temp | Field::Power | Field::Clock));
    }
}

/// Reads every device each tick, keeping what the sources need between
/// readings (counter handles, the last busy times).
pub struct Sampler {
    #[cfg(any(windows, target_os = "linux"))]
    nv: Option<nvidia::Reader>,
    #[cfg(target_os = "linux")]
    busy: linux::Busy,
    #[cfg(target_os = "linux")]
    fd: fdinfo::Reader,
    #[cfg(windows)]
    pdh: Option<pdh::Reader>,
    #[cfg(windows)]
    kmt: kmt::Reader,
}

impl Sampler {
    pub fn new() -> Self {
        Self {
            #[cfg(any(windows, target_os = "linux"))]
            nv: nvidia::Reader::open(),
            #[cfg(target_os = "linux")]
            busy: linux::Busy::default(),
            #[cfg(target_os = "linux")]
            fd: fdinfo::Reader::default(),
            #[cfg(windows)]
            pdh: pdh::Reader::open(),
            #[cfg(windows)]
            kmt: kmt::Reader::default(),
        }
    }

    #[cfg(target_os = "linux")]
    pub fn sample(&mut self) -> Vec<Device> {
        let root = std::path::Path::new("/sys");
        let nv = self.nv.as_mut().map(|n| n.read(true)).unwrap_or_default();
        let now = std::time::Instant::now();
        let mut devs = linux::drm(root, !nv.is_empty());
        self.fd.fill(std::path::Path::new("/proc"), &mut devs, now);
        devs.extend(nv);
        devs.extend(linux::accel(root, &mut self.busy, now));
        order(devs)
    }

    #[cfg(windows)]
    pub fn sample(&mut self) -> Vec<Device> {
        let mut devs = self.pdh.as_mut().map(|p| p.read()).unwrap_or_default();
        self.kmt.fill(&mut devs);
        if let Some(nv) = &mut self.nv {
            merge_nvml(&mut devs, nv.read(false));
        }
        order(devs)
    }

    #[cfg(target_os = "macos")]
    pub fn sample(&mut self) -> Vec<Device> {
        order(mac::devices(&ioaccel::read()))
    }

    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    pub fn sample(&mut self) -> Vec<Device> {
        order(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::tr;

    #[test]
    fn every_reason_and_field_has_english() {
        for w in Why::ALL {
            assert_ne!(tr(w.key()), w.key(), "{w:?}");
        }
        for f in [Field::Usage, Field::Vram, Field::Temp, Field::Power, Field::Clock] {
            assert_ne!(tr(f.key()), f.key(), "{f:?}");
        }
    }

    #[test]
    fn bus_addresses_are_written_as_sysfs_does() {
        assert_eq!(bus_addr("00000000:01:00.0"), "0000:01:00.0");
        assert_eq!(bus_addr("0000:0C:00.0"), "0000:0c:00.0");
        assert_eq!(bus_addr("weird"), "weird");
    }

    fn dev(name: &str, vendor: Option<&'static str>) -> Device {
        Device {
            name: name.into(),
            vendor,
            source: "pdh",
            why: vec![(Field::Temp, Why::WinSensors), (Field::Usage, Why::NoBusy)],
            ..Device::default()
        }
    }

    #[test]
    fn nvml_sensors_land_on_the_nvidia_adapter_of_the_same_name() {
        let mut devs = [dev("AMD Radeon(TM) Graphics", Some("AMD")), dev("NVIDIA GeForce RTX 4060", Some("NVIDIA"))];
        let nv = Device { name: "NVIDIA GeForce RTX 4060".into(), temp: Some(51.0), power: Some(12.5), ..Device::default() };
        merge_nvml(&mut devs, vec![nv]);
        assert_eq!(devs[0].temp, None);
        assert_eq!(devs[1].temp, Some(51.0));
        assert_eq!(devs[1].source, "pdh+nvml");
        assert_eq!(devs[1].why, vec![(Field::Usage, Why::NoBusy)]);
    }

    #[test]
    fn nvml_falls_back_to_order_and_drops_the_rest() {
        let mut devs = [dev("NVIDIA A", Some("NVIDIA")), dev("NVIDIA B", Some("NVIDIA"))];
        let nv = |t| Device { name: "different".into(), temp: Some(t), ..Device::default() };
        merge_nvml(&mut devs, vec![nv(40.0), nv(50.0), nv(60.0)]);
        assert_eq!((devs[0].temp, devs[1].temp), (Some(40.0), Some(50.0)));
    }

    #[test]
    fn gpus_come_before_npus() {
        let npu = Device { id: "n".into(), kind: Kind::Npu, ..Device::default() };
        let gpu = Device { id: "g".into(), ..Device::default() };
        let ids: Vec<String> = order(vec![npu, gpu]).into_iter().map(|d| d.id).collect();
        assert_eq!(ids, ["g", "n"]);
    }
}
