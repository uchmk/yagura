//! macOS: each `IOAccelerator` in the IOKit registry is a GPU. Its
//! `PerformanceStatistics` table says how busy it is and how much memory it
//! holds; what the table carries depends on the driver. Apple silicon's AGX
//! gives the device, renderer and tiler utilisation and the unified memory in
//! use, Intel's the device utilisation, AMD's also the VRAM and, on some
//! cards, the temperature, fan, power and clock. The name is the
//! accelerator's `model` (Apple silicon) or its PCI device's (Intel and AMD
//! in Intel Macs), as are the PCI ids. Reading the registry is in
//! `ioaccel.rs`; this file turns what it found into devices.

use std::collections::HashMap;

use super::{Device, Fan, Field, Kind, Why, vendor_name};

/// The `PerformanceStatistics` numbers that are read.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const STATS: [&str; 14] = [
    "Device Utilization %",
    "GPU Activity(%)",
    "Renderer Utilization %",
    "Tiler Utilization %",
    "In use system memory",
    "vramUsedBytes",
    "inUseVidMemoryBytes",
    "vramFreeBytes",
    "Temperature(C)",
    "Fan Speed(RPM)",
    "Fan Speed(%)",
    "Total Power(W)",
    "Core Clock(MHz)",
    "gpuCoreClock(MHz)",
];

/// One accelerator as the registry describes it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Raw {
    /// The registry entry's id, stable while the machine is up.
    pub entry: u64,
    /// The driver's class: `AGXAcceleratorG13X`, `IntelAccelerator`…
    pub class: String,
    /// `Apple M2 Pro`, `AMD Radeon Pro 5500M`; empty when nothing says.
    pub model: String,
    /// The PCI device's ids, when it is on PCI.
    pub vendor: Option<u32>,
    pub device: Option<u32>,
    /// `VRAM,totalMB` of the PCI device (discrete GPUs).
    pub vram_mb: Option<u64>,
    /// The `STATS` the table had.
    pub stats: HashMap<String, i64>,
}

/// A model name as IOKit keeps it in data: bytes ending in a NUL.
pub fn c_text(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).trim().to_string()
}

/// Apple's own GPUs have no PCI device; their class says AGX.
fn apple(r: &Raw) -> bool {
    r.vendor.is_none() && r.class.starts_with("AGX")
}

fn pct(v: i64) -> f32 {
    v.clamp(0, 100) as f32
}

pub fn device(r: &Raw) -> Device {
    let stat = |k: &str| r.stats.get(k).copied();
    let bytes = |k: &str| stat(k).map(|v| v.max(0) as u64);
    let vendor = match r.vendor {
        Some(id) => vendor_name(id),
        None if apple(r) => Some("Apple"),
        None => None,
    };
    let mut d = Device {
        id: format!("iokit:{:x}", r.entry),
        kind: Kind::Gpu,
        name: r.model.clone(),
        vendor,
        pci: r.vendor.zip(r.device).map(|(v, d)| format!("{v:04x}:{d:04x}")),
        driver: (!r.class.is_empty()).then(|| r.class.clone()),
        source: "iokit",
        usage: stat("Device Utilization %").or_else(|| stat("GPU Activity(%)")).map(pct),
        temp: stat("Temperature(C)").filter(|&t| t > 0).map(|t| t as f32),
        fan: stat("Fan Speed(RPM)")
            .map(|v| Fan::Rpm(v.max(0) as u32))
            .or_else(|| stat("Fan Speed(%)").map(|v| Fan::Percent(v.clamp(0, 100) as u32))),
        power: stat("Total Power(W)").filter(|&w| w > 0).map(|w| w as f32),
        clock: stat("Core Clock(MHz)").or_else(|| stat("gpuCoreClock(MHz)")).filter(|&c| c > 0).map(|c| c as u32),
        ..Device::default()
    };
    for (key, name) in [("Renderer Utilization %", "Renderer"), ("Tiler Utilization %", "Tiler")] {
        if let Some(v) = stat(key) {
            d.engines.push((name.to_string(), pct(v)));
        }
    }
    if apple(r) {
        // Unified memory: what the GPU holds is system memory.
        d.shared_used = bytes("In use system memory");
        d.why.push((Field::Vram, Why::NoVram));
    } else {
        d.vram_used = bytes("vramUsedBytes").or_else(|| bytes("inUseVidMemoryBytes"));
        d.vram_total = r.vram_mb.filter(|&mb| mb > 0).map(|mb| mb << 20);
        if d.vram_total.is_none()
            && let (Some(used), Some(free)) = (d.vram_used, bytes("vramFreeBytes"))
        {
            d.vram_total = Some(used + free);
        }
        if d.vram_used.is_none() && d.vram_total.is_none() {
            d.why.push((Field::Vram, Why::NoVram));
        }
    }
    if d.usage.is_none() {
        d.why.push((Field::Usage, Why::MacStats));
    }
    for (f, missing) in [(Field::Temp, d.temp.is_none()), (Field::Power, d.power.is_none()), (Field::Clock, d.clock.is_none())] {
        if missing {
            d.why.push((f, Why::MacSensors));
        }
    }
    d
}

/// Every accelerator, in the registry's order. A driver can publish more than
/// one accelerator for a device (one per display, on some Intel Macs); those
/// sharing a name and PCI ids are kept once.
pub fn devices(raws: &[Raw]) -> Vec<Device> {
    let mut out: Vec<Device> = Vec::new();
    for r in raws {
        let d = device(r);
        if !d.name.is_empty() && out.iter().any(|o| (&o.name, &o.pci) == (&d.name, &d.pci)) {
            continue;
        }
        out.push(d);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(pairs: &[(&str, i64)]) -> HashMap<String, i64> {
        pairs.iter().map(|&(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn apple_silicon_uses_unified_memory() {
        let r = Raw {
            entry: 0x1000_02a3,
            class: "AGXAcceleratorG14X".into(),
            model: "Apple M2 Pro".into(),
            stats: stats(&[
                ("Device Utilization %", 37),
                ("Renderer Utilization %", 35),
                ("Tiler Utilization %", 12),
                ("In use system memory", 1 << 30),
            ]),
            ..Raw::default()
        };
        let d = device(&r);
        assert_eq!(d.id, "iokit:100002a3");
        assert_eq!((d.name.as_str(), d.vendor, d.source), ("Apple M2 Pro", Some("Apple"), "iokit"));
        assert_eq!(d.usage, Some(37.0));
        assert_eq!(d.engines, vec![("Renderer".to_string(), 35.0), ("Tiler".to_string(), 12.0)]);
        assert_eq!((d.shared_used, d.vram_used), (Some(1 << 30), None));
        assert_eq!(d.why(Field::Vram), Some(Why::NoVram));
        assert_eq!(d.why(Field::Temp), Some(Why::MacSensors));
        assert_eq!(d.why(Field::Usage), None);
        assert_eq!(d.pci, None);
    }

    #[test]
    fn an_amd_card_in_an_intel_mac_has_its_own_memory_and_sensors() {
        let r = Raw {
            entry: 7,
            class: "AMDRadeonX6000_AMDNavi14GraphicsAccelerator".into(),
            model: "AMD Radeon Pro 5500M".into(),
            vendor: Some(0x1002),
            device: Some(0x7340),
            vram_mb: Some(4096),
            stats: stats(&[
                ("GPU Activity(%)", 140),
                ("vramUsedBytes", 512 << 20),
                ("Temperature(C)", 58),
                ("Fan Speed(%)", 30),
                ("Total Power(W)", 21),
                ("Core Clock(MHz)", 1300),
            ]),
        };
        let d = device(&r);
        assert_eq!((d.vendor, d.pci.as_deref()), (Some("AMD"), Some("1002:7340")));
        assert_eq!(d.usage, Some(100.0));
        assert_eq!((d.vram_used, d.vram_total), (Some(512 << 20), Some(4 << 30)));
        assert_eq!((d.temp, d.fan, d.power, d.clock), (Some(58.0), Some(Fan::Percent(30)), Some(21.0), Some(1300)));
        assert!(d.why.is_empty(), "{:?}", d.why);
    }

    #[test]
    fn an_intel_igpu_says_what_is_missing() {
        let r = Raw {
            entry: 9,
            class: "IntelAccelerator".into(),
            model: "Intel Iris Plus Graphics".into(),
            vendor: Some(0x8086),
            device: Some(0x8a52),
            stats: stats(&[("vramUsedBytes", 0), ("vramFreeBytes", 1536 << 20)]),
            ..Raw::default()
        };
        let d = device(&r);
        assert_eq!(d.vendor, Some("Intel"));
        assert_eq!(d.vram_total, Some(1536 << 20));
        assert_eq!(d.why(Field::Usage), Some(Why::MacStats));
        assert_eq!(d.why(Field::Clock), Some(Why::MacSensors));
    }

    #[test]
    fn twin_accelerators_are_kept_once_and_names_read_from_data() {
        let r = Raw {
            entry: 1,
            class: "IntelAccelerator".into(),
            model: c_text(b"Intel UHD Graphics 630\0"),
            vendor: Some(0x8086),
            device: Some(0x3e9b),
            ..Raw::default()
        };
        let twin = Raw { entry: 2, ..r.clone() };
        let other = Raw {
            entry: 3,
            class: "AMDRadeonX5000".into(),
            model: "AMD Radeon Pro 560X".into(),
            vendor: Some(0x1002),
            device: Some(0x67ef),
            ..Raw::default()
        };
        let names: Vec<String> = devices(&[r, twin, other]).into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["Intel UHD Graphics 630", "AMD Radeon Pro 560X"]);
        assert_eq!(c_text(b"abc"), "abc");
    }
}
