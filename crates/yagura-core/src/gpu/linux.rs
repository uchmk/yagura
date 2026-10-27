//! GPUs and NPUs from sysfs: `/sys/class/drm/cardN` for the GPUs (amdgpu
//! keeps a busy percent and its memory there; every driver can have a hwmon
//! with the sensors) and `/sys/class/accel/accelN` for the NPUs (Intel's
//! `intel_vpu` keeps a busy time and its clock).

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{Device, Fan, Field, Kind, Why, vendor_name};

/// The last busy time of each NPU, to turn the next one into a percent.
#[derive(Debug, Default)]
pub struct Busy(HashMap<String, (u64, Instant)>);

fn read(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn read_u64(p: &Path) -> Option<u64> {
    let s = read(p)?;
    match s.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

/// The `KEY=value` lines of a `uevent` file.
fn uevent(dev: &Path) -> HashMap<String, String> {
    read(&dev.join("uevent"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect()
}

/// `name` followed by digits only: `card0`, not `card0-HDMI-A-1`.
fn numbered(dir: &Path, name: &str) -> Vec<(u32, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<(u32, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_str()?.strip_prefix(name)?.parse().ok()?;
            Some((n, e.path()))
        })
        .collect();
    out.sort_by_key(|&(n, _)| n);
    out
}

/// Vendors of devices that are not on PCI, by their driver.
fn vendor_of_driver(driver: &str) -> Option<&'static str> {
    Some(match driver {
        "msm" | "msm_drm" | "msm_dpu" => "Qualcomm",
        "panfrost" | "panthor" | "lima" => "Arm",
        "asahi" => "Apple",
        "v3d" | "vc4" => "Broadcom",
        "intel_vpu" => "Intel",
        "amdxdna" => "AMD",
        _ => return None,
    })
}

/// Identity common to GPUs and NPUs: vendor, PCI ids, driver.
fn identify(dev: &Path, d: &mut Device) {
    let ev = uevent(dev);
    d.driver = ev.get("DRIVER").cloned();
    d.bus = ev.get("PCI_SLOT_NAME").map(|s| s.to_ascii_lowercase());
    let pci = ev.get("PCI_ID").map(|s| s.to_lowercase());
    let vendor_id =
        read_u64(&dev.join("vendor")).or_else(|| pci.as_deref().and_then(|p| u64::from_str_radix(p.split(':').next()?, 16).ok()));
    d.pci = pci.or_else(|| {
        let device = read_u64(&dev.join("device"))?;
        Some(format!("{:04x}:{device:04x}", vendor_id?))
    });
    d.vendor = vendor_id.and_then(|v| vendor_name(v as u32)).or_else(|| d.driver.as_deref().and_then(vendor_of_driver));
    d.name = read(&dev.join("product_name")).unwrap_or_default();
}

fn first_hwmon(dev: &Path) -> Option<PathBuf> {
    numbered(&dev.join("hwmon"), "hwmon").into_iter().next().map(|(_, p)| p)
}

/// The GPUs under `root` (`/sys`, or a fake tree in a test). NVIDIA cards
/// are left out when `skip_nvidia` (NVML lists them, better).
pub fn drm(root: &Path, skip_nvidia: bool) -> Vec<Device> {
    let mut out = Vec::new();
    for (n, card) in numbered(&root.join("class/drm"), "card") {
        let dev = card.join("device");
        if !dev.is_dir() {
            continue;
        }
        let mut d = Device { id: format!("drm:card{n}"), source: "sysfs", ..Device::default() };
        identify(&dev, &mut d);
        let nvidia = d.vendor == Some("NVIDIA");
        if nvidia && skip_nvidia {
            continue;
        }
        let missing = if nvidia { Why::NoNvml } else { Why::NoBusy };
        d.usage = read_u64(&dev.join("gpu_busy_percent")).map(|v| v.min(100) as f32);
        if d.usage.is_none() {
            d.why.push((Field::Usage, missing));
        }
        if let Some(pct) = read_u64(&dev.join("mem_busy_percent")) {
            d.engines.push(("Memory".into(), pct.min(100) as f32));
        }
        d.vram_used = read_u64(&dev.join("mem_info_vram_used"));
        d.vram_total = read_u64(&dev.join("mem_info_vram_total"));
        if d.vram_total.is_none() {
            d.why.push((Field::Vram, if nvidia { Why::NoNvml } else { Why::NoVram }));
        }
        d.shared_used = read_u64(&dev.join("mem_info_gtt_used"));
        d.shared_total = read_u64(&dev.join("mem_info_gtt_total"));
        let hw = first_hwmon(&dev);
        let hw_u64 = |f: &str| hw.as_ref().and_then(|h| read_u64(&h.join(f)));
        d.temp = hw_u64("temp1_input").map(|m| m as f32 / 1000.0);
        d.fan = hw_u64("fan1_input").map(|r| Fan::Rpm(r as u32));
        d.power = hw_u64("power1_average").or_else(|| hw_u64("power1_input")).map(|uw| uw as f32 / 1e6);
        d.clock = hw_u64("freq1_input")
            .map(|hz| hz / 1_000_000)
            .or_else(|| read_u64(&card.join("gt_cur_freq_mhz")))
            .or_else(|| read_u64(&dev.join("tile0/gt0/freq0/cur_freq")))
            .map(|mhz| mhz as u32);
        let sensor = if nvidia { Why::NoNvml } else { Why::NoSensor };
        for (f, have) in [(Field::Temp, d.temp.is_some()), (Field::Power, d.power.is_some()), (Field::Clock, d.clock.is_some())] {
            if !have {
                d.why.push((f, sensor));
            }
        }
        out.push(d);
    }
    out
}

/// Percent busy between two readings of a busy time in microseconds.
pub fn busy_pct(prev_us: u64, now_us: u64, elapsed: std::time::Duration) -> f32 {
    let wall = elapsed.as_micros() as f64;
    if wall <= 0.0 {
        return 0.0;
    }
    (now_us.saturating_sub(prev_us) as f64 * 100.0 / wall).min(100.0) as f32
}

/// The NPUs under `root`. Usage needs two readings, so it is `None` the
/// first time.
pub fn accel(root: &Path, busy: &mut Busy, now: Instant) -> Vec<Device> {
    let mut out = Vec::new();
    let mut seen = Vec::new();
    for (n, node) in numbered(&root.join("class/accel"), "accel") {
        let dev = node.join("device");
        if !dev.is_dir() {
            continue;
        }
        let id = format!("accel:accel{n}");
        let mut d = Device { id: id.clone(), kind: Kind::Npu, source: "sysfs", ..Device::default() };
        identify(&dev, &mut d);
        match read_u64(&dev.join("npu_busy_time_us")) {
            Some(us) => {
                if let Some(&(prev, at)) = busy.0.get(&id) {
                    d.usage = Some(busy_pct(prev, us, now.saturating_duration_since(at)));
                }
                busy.0.insert(id.clone(), (us, now));
                seen.push(id);
            }
            None => d.why.push((Field::Usage, Why::NpuLittle)),
        }
        d.vram_used = read_u64(&dev.join("npu_memory_utilization"));
        d.clock = read_u64(&dev.join("npu_current_frequency_mhz")).map(|m| m as u32);
        for (f, have) in [(Field::Vram, d.vram_used.is_some()), (Field::Clock, d.clock.is_some())] {
            if !have {
                d.why.push((f, Why::NpuLittle));
            }
        }
        // No accel driver has a hwmon for these.
        d.why.extend([(Field::Temp, Why::NoSensor), (Field::Power, Why::NoSensor)]);
        out.push(d);
    }
    busy.0.retain(|k, _| seen.contains(k));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::test_dir;
    use std::time::Duration;

    fn put(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn amdgpu_reads_busy_memory_and_sensors() {
        let root = test_dir("gpu-amd");
        let dev = "class/drm/card1/device";
        put(&root, &format!("{dev}/uevent"), "DRIVER=amdgpu\nPCI_CLASS=30000\nPCI_ID=1002:744C\nPCI_SLOT_NAME=0000:03:00.0\n");
        put(&root, &format!("{dev}/vendor"), "0x1002\n");
        put(&root, &format!("{dev}/device"), "0x744c\n");
        put(&root, &format!("{dev}/gpu_busy_percent"), "34\n");
        put(&root, &format!("{dev}/mem_info_vram_used"), "1073741824\n");
        put(&root, &format!("{dev}/mem_info_vram_total"), "17179869184\n");
        put(&root, &format!("{dev}/hwmon/hwmon3/temp1_input"), "51000\n");
        put(&root, &format!("{dev}/hwmon/hwmon3/power1_average"), "23000000\n");
        put(&root, &format!("{dev}/hwmon/hwmon3/fan1_input"), "1200\n");
        put(&root, &format!("{dev}/hwmon/hwmon3/freq1_input"), "2100000000\n");
        // A connector and a render node are not cards.
        put(&root, "class/drm/card1-DP-1/status", "connected\n");
        put(&root, "class/drm/renderD128/device/vendor", "0x1002\n");
        let devs = drm(&root, false);
        assert_eq!(devs.len(), 1);
        let d = &devs[0];
        assert_eq!(d.id, "drm:card1");
        assert_eq!((d.vendor, d.pci.as_deref(), d.driver.as_deref()), (Some("AMD"), Some("1002:744c"), Some("amdgpu")));
        assert_eq!(d.bus.as_deref(), Some("0000:03:00.0"));
        assert_eq!(d.usage, Some(34.0));
        assert_eq!(d.vram_pct(), Some(6.25));
        assert_eq!((d.temp, d.power, d.clock, d.fan), (Some(51.0), Some(23.0), Some(2100), Some(Fan::Rpm(1200))));
        assert!(d.why.is_empty(), "{:?}", d.why);
    }

    #[test]
    fn i915_says_what_it_lacks() {
        let root = test_dir("gpu-intel");
        put(&root, "class/drm/card0/device/uevent", "DRIVER=i915\nPCI_ID=8086:A7A0\n");
        put(&root, "class/drm/card0/gt_cur_freq_mhz", "1300\n");
        let d = &drm(&root, false)[0];
        assert_eq!(d.vendor, Some("Intel"));
        assert_eq!(d.clock, Some(1300));
        assert_eq!(d.why(Field::Usage), Some(Why::NoBusy));
        assert_eq!(d.why(Field::Vram), Some(Why::NoVram));
        assert_eq!(d.why(Field::Temp), Some(Why::NoSensor));
        assert_eq!(d.why(Field::Clock), None);
    }

    #[test]
    fn nvidia_cards_blame_nvml_or_step_aside_for_it() {
        let root = test_dir("gpu-nv");
        put(&root, "class/drm/card0/device/uevent", "DRIVER=nvidia\nPCI_ID=10DE:2882\n");
        let d = &drm(&root, false)[0];
        assert_eq!(d.why(Field::Usage), Some(Why::NoNvml));
        assert_eq!(d.why(Field::Temp), Some(Why::NoNvml));
        assert!(drm(&root, true).is_empty());
    }

    #[test]
    fn non_pci_gpus_get_their_vendor_from_the_driver() {
        let root = test_dir("gpu-arm");
        put(&root, "class/drm/card0/device/uevent", "DRIVER=panthor\nOF_NAME=gpu\n");
        let d = &drm(&root, false)[0];
        assert_eq!((d.vendor, d.pci.as_deref()), (Some("Arm"), None));
    }

    #[test]
    fn busy_time_turns_into_a_percent() {
        assert_eq!(busy_pct(1_000, 501_000, Duration::from_secs(1)), 50.0);
        assert_eq!(busy_pct(0, 3_000_000, Duration::from_secs(1)), 100.0);
        assert_eq!(busy_pct(5, 1, Duration::from_secs(1)), 0.0);
        assert_eq!(busy_pct(0, 5, Duration::ZERO), 0.0);
    }

    #[test]
    fn intel_npu_needs_two_readings() {
        let root = test_dir("npu-intel");
        let dev = "class/accel/accel0/device";
        put(&root, &format!("{dev}/uevent"), "DRIVER=intel_vpu\nPCI_ID=8086:7D1D\n");
        put(&root, &format!("{dev}/npu_busy_time_us"), "1000000\n");
        put(&root, &format!("{dev}/npu_current_frequency_mhz"), "1400\n");
        let mut busy = Busy::default();
        let t0 = Instant::now();
        let first = &accel(&root, &mut busy, t0)[0];
        assert_eq!((first.kind, first.vendor, first.usage, first.clock), (Kind::Npu, Some("Intel"), None, Some(1400)));
        put(&root, &format!("{dev}/npu_busy_time_us"), "1250000\n");
        let second = &accel(&root, &mut busy, t0 + Duration::from_secs(1))[0];
        assert_eq!(second.usage, Some(25.0));
        // Gone devices are forgotten.
        std::fs::remove_dir_all(root.join("class/accel/accel0")).unwrap();
        assert!(accel(&root, &mut busy, t0 + Duration::from_secs(2)).is_empty());
        assert!(busy.0.is_empty());
    }

    #[test]
    fn npus_without_counters_say_so() {
        let root = test_dir("npu-amd");
        put(&root, "class/accel/accel0/device/uevent", "DRIVER=amdxdna\nPCI_ID=1022:17F0\n");
        let d = &accel(&root, &mut Busy::default(), Instant::now())[0];
        assert_eq!(d.vendor, Some("AMD"));
        assert_eq!(d.why(Field::Usage), Some(Why::NpuLittle));
        assert_eq!(d.why(Field::Vram), Some(Why::NpuLittle));
        assert_eq!(d.why(Field::Temp), Some(Why::NoSensor));
    }
}
