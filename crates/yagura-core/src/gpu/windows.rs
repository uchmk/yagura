//! Windows: the `GPU Engine`, `GPU Adapter Memory` and `GPU Process Memory`
//! counters, folded per adapter. The counters name their instances after the
//! adapter's LUID (`pid_1234_luid_0x00000000_0x0000D1A5_phys_0_eng_0_engtype_3D`);
//! DXGI gives each LUID its name and memory sizes. An NPU is an adapter DXGI
//! does not list (compute only) with no 3D engine. The parsing and folding
//! are plain functions so they are tested on every OS; `pdh.rs` reads.

#![cfg_attr(not(windows), allow(dead_code))]

use std::collections::{BTreeMap, HashMap};

use super::{Device, Fan, Field, GpuProc, Kind, Why};

/// An adapter as DXGI lists it.
#[derive(Clone, Debug, Default)]
pub struct Adapter {
    /// `luid_0x00000000_0x0000d1a5`, lowercase.
    pub luid: String,
    pub name: String,
    pub vendor: Option<&'static str>,
    pub pci: Option<String>,
    /// Bytes.
    pub dedicated: u64,
    pub shared: u64,
    /// The user-mode driver's version, as dxdiag shows it.
    pub driver: Option<String>,
    /// The Basic Render Driver: left out.
    pub software: bool,
}

/// The LUID a DXGI adapter is called by in the counters.
pub fn luid_name(high: i32, low: u32) -> String {
    format!("luid_0x{:08x}_0x{low:08x}", high as u32)
}

/// The user-mode driver's version from DXGI's `CheckInterfaceSupport`:
/// four 16-bit parts, highest first (`32.0.31041.1004`).
pub fn umd_version(v: i64) -> String {
    let v = v as u64;
    format!("{}.{}.{}.{}", v >> 48, v >> 32 & 0xffff, v >> 16 & 0xffff, v & 0xffff)
}

/// The LUID back out of a device id (`luid:0x00000000_0x0000d1a5`).
pub fn luid_parts(id: &str) -> Option<(i32, u32)> {
    let (high, low) = id.strip_prefix("luid:0x")?.split_once("_0x")?;
    Some((u32::from_str_radix(high, 16).ok()? as i32, u32::from_str_radix(low, 16).ok()?))
}

/// What D3DKMT says of an adapter's sensors: the same source as Task
/// Manager's GPU temperature. Zero where the driver reports nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Perf {
    /// Tenths of a degree.
    pub temp: u32,
    /// From the caps: zero when the driver keeps no temperature.
    pub temp_max: u32,
    pub fan_rpm: u32,
    /// From the caps: zero when the driver keeps no fan speed.
    pub fan_max: u32,
    /// The first engine's clock, Hz.
    pub freq: u64,
}

/// Lay what D3DKMT gave over a device from the counters.
pub fn apply_perf(d: &mut Device, p: &Perf) {
    let mut filled = Vec::new();
    if p.temp_max > 0 && p.temp > 0 {
        d.temp = Some(p.temp as f32 / 10.0);
        filled.push(Field::Temp);
    }
    if p.fan_max > 0 {
        d.fan = Some(Fan::Rpm(p.fan_rpm));
    }
    if p.freq >= 1_000_000 {
        d.clock = Some((p.freq / 1_000_000) as u32);
        filled.push(Field::Clock);
    }
    if !filled.is_empty() || d.fan.is_some() {
        d.source = "pdh+d3dkmt";
    }
    d.why.retain(|(f, _)| !filled.contains(f));
}

/// `luid_0x…_0x…` out of an instance name, lowercase.
pub fn luid_of(inst: &str) -> Option<String> {
    let lower = inst.to_ascii_lowercase();
    let at = lower.find("luid_0x")?;
    let rest = &lower[at..];
    // luid_0x + 8 hex + _0x + 8 hex
    let len = "luid_0x".len() + 8 + "_0x".len() + 8;
    let luid = rest.get(..len)?;
    luid.as_bytes()[len - 11..len - 8].eq(b"_0x").then(|| luid.to_string())
}

pub fn pid_of(inst: &str) -> Option<u32> {
    inst.strip_prefix("pid_")?.split('_').next()?.parse().ok()
}

/// `(phys, eng, engine type)`; the type loses a trailing `_N`
/// (`Compute_1` → `Compute`).
pub fn engine_of(inst: &str) -> Option<(u32, u32, String)> {
    let num = |key: &str| -> Option<u32> { inst.split(key).nth(1)?.split('_').next()?.parse().ok() };
    let phys = num("_phys_")?;
    let eng = num("_eng_")?;
    let ty = inst.split("_engtype_").nth(1)?;
    let ty = match ty.rsplit_once('_') {
        Some((head, tail)) if !head.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => ty,
    };
    Some((phys, eng, ty.to_string()))
}

/// `VideoDecode` → `Video Decode`.
fn spaced(ty: &str) -> String {
    let mut out = String::with_capacity(ty.len() + 2);
    let mut prev_lower = false;
    for c in ty.chars() {
        if c.is_ascii_uppercase() && prev_lower {
            out.push(' ');
        }
        prev_lower = c.is_ascii_lowercase();
        out.push(c);
    }
    out
}

/// Where an engine type goes in the list: the familiar ones first.
fn rank(ty: &str) -> usize {
    const ORDER: [&str; 6] = ["3D", "Compute", "Copy", "VideoDecode", "VideoEncode", "VideoProcessing"];
    ORDER.iter().position(|o| *o == ty).unwrap_or(ORDER.len())
}

const MAX_PROCS: usize = 16;

#[derive(Default)]
struct Acc {
    /// (phys, eng) → (type, summed over processes).
    engines: HashMap<(u32, u32), (String, f64)>,
    /// pid → the busiest engine it uses.
    procs: HashMap<u32, f64>,
    proc_mem: HashMap<u32, f64>,
    dedicated: f64,
    shared: f64,
}

/// One device per adapter: the DXGI adapters in their order, then LUIDs the
/// counters alone know (NPUs). Each counter is `(instance, value)`.
pub fn fold(
    adapters: &[Adapter],
    engines: &[(String, f64)],
    dedicated: &[(String, f64)],
    shared: &[(String, f64)],
    proc_mem: &[(String, f64)],
) -> Vec<Device> {
    // BTreeMap so the counter-only adapters come in a stable order.
    let mut acc: BTreeMap<String, Acc> = BTreeMap::new();
    for (inst, v) in engines {
        let (Some(luid), Some((phys, eng, ty))) = (luid_of(inst), engine_of(inst)) else { continue };
        let a = acc.entry(luid).or_default();
        a.engines.entry((phys, eng)).or_insert_with(|| (ty, 0.0)).1 += v;
        if let Some(pid) = pid_of(inst) {
            let p = a.procs.entry(pid).or_default();
            *p = p.max(*v);
        }
    }
    for (inst, v) in dedicated {
        if let Some(luid) = luid_of(inst) {
            acc.entry(luid).or_default().dedicated += v;
        }
    }
    for (inst, v) in shared {
        if let Some(luid) = luid_of(inst) {
            acc.entry(luid).or_default().shared += v;
        }
    }
    for (inst, v) in proc_mem {
        let (Some(luid), Some(pid)) = (luid_of(inst), pid_of(inst)) else { continue };
        *acc.entry(luid).or_default().proc_mem.entry(pid).or_default() += v;
    }

    let mut out = Vec::new();
    for a in adapters.iter().filter(|a| !a.software) {
        let c = acc.remove(&a.luid).unwrap_or_default();
        let mut d = device(&a.luid, c, Kind::Gpu);
        d.name = a.name.clone();
        d.vendor = a.vendor;
        d.pci = a.pci.clone();
        d.vram_total = (a.dedicated > 0).then_some(a.dedicated);
        d.shared_total = (a.shared > 0).then_some(a.shared);
        d.driver = a.driver.clone();
        // The process counter can read more than the card holds (dwm at 38 GiB
        // on a 16 GiB card): such a reading is no reading.
        if let Some(total) = d.vram_total {
            for p in &mut d.procs {
                p.mem = p.mem.filter(|&m| m <= total);
            }
        }
        if d.vram_total.is_none() {
            d.why.push((Field::Vram, Why::NoVram));
        }
        out.push(d);
    }
    let software: Vec<&str> = adapters.iter().filter(|a| a.software).map(|a| a.luid.as_str()).collect();
    for (luid, c) in acc {
        if software.contains(&luid.as_str()) || c.engines.is_empty() {
            continue;
        }
        let kind = if c.engines.values().any(|(t, _)| t == "3D") { Kind::Gpu } else { Kind::Npu };
        let mut d = device(&luid, c, kind);
        d.vram_used = d.vram_used.filter(|&b| b > 0);
        d.shared_used = d.shared_used.filter(|&b| b > 0);
        if d.vram_used.is_none() {
            d.why.push((Field::Vram, Why::NoVram));
        }
        out.push(d);
    }
    out
}

fn device(luid: &str, c: Acc, kind: Kind) -> Device {
    let mut by_type: HashMap<String, f64> = HashMap::new();
    for (ty, v) in c.engines.into_values() {
        let m = by_type.entry(ty).or_default();
        *m = m.max(v);
    }
    let mut engines: Vec<(String, f64)> = by_type.into_iter().collect();
    engines.sort_by(|a, b| rank(&a.0).cmp(&rank(&b.0)).then_with(|| a.0.cmp(&b.0)));
    let usage = engines.iter().map(|e| e.1).fold(0.0, f64::max).min(100.0) as f32;

    let mut pids: Vec<u32> = c.procs.keys().chain(c.proc_mem.keys()).copied().collect();
    pids.sort_unstable();
    pids.dedup();
    let mut procs: Vec<GpuProc> = pids
        .into_iter()
        .map(|pid| GpuProc { pid, usage: c.procs.get(&pid).map(|&u| u.min(100.0) as f32), mem: c.proc_mem.get(&pid).map(|&m| m as u64) })
        .filter(|p| p.usage.unwrap_or(0.0) > 0.0 || p.mem.unwrap_or(0) > 0)
        .collect();
    procs.sort_by(|a, b| {
        let key = |p: &GpuProc| (p.usage.unwrap_or(0.0), p.mem.unwrap_or(0));
        let (ka, kb) = (key(a), key(b));
        kb.0.total_cmp(&ka.0).then(kb.1.cmp(&ka.1)).then(a.pid.cmp(&b.pid))
    });
    procs.truncate(MAX_PROCS);

    Device {
        id: format!("luid:{}", luid.trim_start_matches("luid_")),
        kind,
        source: "pdh",
        usage: Some(usage),
        engines: engines.into_iter().map(|(t, v)| (spaced(&t), v.min(100.0) as f32)).collect(),
        vram_used: Some(c.dedicated as u64),
        shared_used: Some(c.shared as u64),
        procs,
        why: vec![(Field::Temp, Why::WinSensors), (Field::Power, Why::WinSensors), (Field::Clock, Why::WinSensors)],
        ..Device::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMD: &str = "luid_0x00000000_0x0000D1A5";
    const NPU: &str = "luid_0x00000000_0x0000E2B6";

    fn eng(pid: u32, luid: &str, eng: u32, ty: &str, v: f64) -> (String, f64) {
        (format!("pid_{pid}_{luid}_phys_0_eng_{eng}_engtype_{ty}"), v)
    }

    #[test]
    fn parses_instance_names() {
        let inst = "pid_1234_luid_0x00000000_0x0000D1A5_phys_0_eng_12_engtype_Compute_1";
        assert_eq!(luid_of(inst).as_deref(), Some("luid_0x00000000_0x0000d1a5"));
        assert_eq!(pid_of(inst), Some(1234));
        assert_eq!(engine_of(inst), Some((0, 12, "Compute".to_string())));
        assert_eq!(engine_of("pid_1_luid_0x00000000_0x00000001_phys_0_eng_3_engtype_3D"), Some((0, 3, "3D".to_string())));
        assert_eq!(luid_of("luid_0x00000000_0x0000D1A5_phys_0").as_deref(), Some("luid_0x00000000_0x0000d1a5"));
        assert_eq!(pid_of("luid_0x00000000_0x0000D1A5_phys_0"), None);
        assert_eq!(luid_of("_Total"), None);
        assert_eq!(luid_name(0, 0xD1A5), "luid_0x00000000_0x0000d1a5");
        assert_eq!(luid_name(-1, 1), "luid_0xffffffff_0x00000001");
    }

    #[test]
    fn d3dkmt_fills_what_the_driver_keeps() {
        assert_eq!(luid_parts("luid:0x00000000_0x0000d1a5"), Some((0, 0xd1a5)));
        assert_eq!(luid_parts("luid:0xffffffff_0x00000001"), Some((-1, 1)));
        assert_eq!(luid_parts("nvml:0"), None);
        let devs = fold(&[amd()], &[eng(1, AMD, 0, "3D", 1.0)], &[], &[], &[]);
        let mut d = devs[0].clone();
        apply_perf(&mut d, &Perf { temp: 543, temp_max: 1100, fan_rpm: 0, fan_max: 3300, freq: 2_430_000_000 });
        assert_eq!((d.temp, d.fan, d.clock, d.source), (Some(54.3), Some(Fan::Rpm(0)), Some(2430), "pdh+d3dkmt"));
        assert_eq!(d.why, vec![(Field::Power, Why::WinSensors)]);
        // A driver that keeps nothing: the counters' device as it was.
        let mut bare = devs[0].clone();
        apply_perf(&mut bare, &Perf { temp: 400, ..Perf::default() });
        assert_eq!(bare, devs[0]);
    }

    #[test]
    fn reads_the_umd_version() {
        let v = (32i64 << 48) | (31041 << 16) | 1004;
        assert_eq!(umd_version(v), "32.0.31041.1004");
        assert_eq!(umd_version((32i64 << 48) | (15 << 32) | (6614 << 16) | 3), "32.15.6614.3");
    }

    #[test]
    fn spaces_engine_types() {
        assert_eq!(spaced("VideoDecode"), "Video Decode");
        assert_eq!(spaced("3D"), "3D");
        assert_eq!(spaced("LegacyOverlay"), "Legacy Overlay");
    }

    fn amd() -> Adapter {
        Adapter {
            luid: AMD.to_ascii_lowercase(),
            name: "AMD Radeon RX 7800 XT".into(),
            vendor: Some("AMD"),
            pci: Some("1002:747e".into()),
            dedicated: 16 << 30,
            shared: 32 << 30,
            driver: Some("32.0.31041.1004".into()),
            software: false,
        }
    }

    #[test]
    fn folds_engines_processes_and_memory_per_adapter() {
        let engines = [
            eng(100, AMD, 0, "3D", 30.0),
            eng(200, AMD, 0, "3D", 25.0), // same engine: 55 in all
            eng(200, AMD, 4, "VideoDecode", 10.0),
            eng(300, AMD, 1, "Compute_0", 5.0),
            eng(300, AMD, 2, "Compute_1", 20.0), // the busier Compute engine
            eng(400, AMD, 0, "3D", 0.0),
        ];
        let dedicated = [(format!("{AMD}_phys_0"), 2.0 * (1u64 << 30) as f64)];
        let shared = [(format!("{AMD}_phys_0"), 1.0 * (1u64 << 30) as f64)];
        let proc_mem = [
            (format!("pid_100_{AMD}_phys_0"), 512.0 * 1024.0 * 1024.0),
            (format!("pid_200_{AMD}_phys_0"), 38.1 * (1u64 << 30) as f64), // more than the card holds
        ];
        let devs = fold(&[amd()], &engines, &dedicated, &shared, &proc_mem);
        assert_eq!(devs.len(), 1);
        let d = &devs[0];
        assert_eq!((d.kind, d.id.as_str(), d.source), (Kind::Gpu, "luid:0x00000000_0x0000d1a5", "pdh"));
        assert_eq!(d.usage, Some(55.0));
        let names: Vec<&str> = d.engines.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(names, ["3D", "Compute", "Video Decode"]);
        assert_eq!(d.engines[1].1, 20.0);
        assert_eq!((d.vram_used, d.vram_total), (Some(2 << 30), Some(16 << 30)));
        assert_eq!(d.vram_pct(), Some(12.5));
        // Busiest first; pid 400 used nothing and holds nothing.
        let pids: Vec<u32> = d.procs.iter().map(|p| p.pid).collect();
        assert_eq!(pids, [100, 200, 300]);
        assert_eq!((d.procs[0].mem, d.procs[1].mem), (Some(512 << 20), None));
        assert_eq!(d.driver.as_deref(), Some("32.0.31041.1004"));
        assert_eq!(d.why(Field::Temp), Some(Why::WinSensors));
        assert_eq!(d.why(Field::Vram), None);
    }

    #[test]
    fn counter_only_adapters_are_npus_unless_they_draw() {
        let software = Adapter { luid: "luid_0x00000000_0x00000009".into(), software: true, ..Adapter::default() };
        let engines = [
            eng(500, NPU, 0, "Compute", 40.0),
            eng(600, "luid_0x00000000_0x00000009", 0, "3D", 3.0), // Basic Render Driver
        ];
        let devs = fold(&[amd(), software], &engines, &[], &[], &[]);
        assert_eq!(devs.len(), 2);
        assert_eq!((devs[0].usage, devs[0].procs.len()), (Some(0.0), 0));
        let npu = &devs[1];
        assert_eq!((npu.kind, npu.usage, npu.name.as_str()), (Kind::Npu, Some(40.0), ""));
        assert_eq!(npu.why(Field::Vram), Some(Why::NoVram));
    }

    #[test]
    fn keeps_the_busiest_sixteen_processes() {
        let engines: Vec<(String, f64)> = (1..=20).map(|p| eng(p, AMD, 0, "3D", p as f64)).collect();
        let d = &fold(&[amd()], &engines, &[], &[], &[])[0];
        assert_eq!(d.procs.len(), MAX_PROCS);
        assert_eq!(d.procs[0].pid, 20);
        assert_eq!(d.usage, Some(100.0)); // 210 summed, capped
    }
}
