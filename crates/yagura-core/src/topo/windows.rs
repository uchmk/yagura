//! The topology from `GetLogicalProcessorInformationEx(RelationAll)`: the
//! core entries give the threads and the `EfficiencyClass`, the level-3 cache
//! entries the L3 sets. The buffer is parsed by hand (a pure function, tested
//! on Linux too) so no struct of the bindings has to line up with it.

use super::{CoreKind, CpuInfo};

const RELATION_PROCESSOR_CORE: u32 = 0;
const RELATION_CACHE: u32 = 2;
/// One `GROUP_AFFINITY`: a 64-bit mask, the group, three reserved words.
const AFFINITY: usize = 16;

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// The (group, bit) pairs of `count` affinities starting at `at`.
fn masks(e: &[u8], at: usize, count: usize) -> Vec<(u16, u32)> {
    let mut out = Vec::new();
    for i in 0..count {
        let a = at + i * AFFINITY;
        let (Some(mask), Some(group)) = (u64_at(e, a), u16_at(e, a + 8)) else { break };
        out.extend((0..64).filter(|b| mask & (1 << b) != 0).map(|b| (group, b)));
    }
    out
}

/// The entries of the buffer as one [`CpuInfo`] per logical CPU, numbered
/// by the rank of their (group, bit): the order sysinfo lists them in.
pub fn parse(buf: &[u8]) -> Vec<CpuInfo> {
    let mut cores: Vec<(Vec<(u16, u32)>, u8)> = Vec::new();
    let mut l3s: Vec<(Vec<(u16, u32)>, u64)> = Vec::new();
    let mut at = 0;
    while let (Some(rel), Some(size)) = (u32_at(buf, at), u32_at(buf, at + 4)) {
        let size = size as usize;
        if size < 8 || at + size > buf.len() {
            break;
        }
        let e = &buf[at..at + size];
        match rel {
            RELATION_PROCESSOR_CORE => {
                let class = e.get(9).copied().unwrap_or(0);
                let count = u16_at(e, 30).unwrap_or(0) as usize;
                cores.push((masks(e, 32, count), class));
            }
            RELATION_CACHE => {
                let level = e.get(8).copied().unwrap_or(0);
                let bytes = u32_at(e, 12).unwrap_or(0) as u64;
                let kind = u32_at(e, 16).unwrap_or(1);
                // Unified caches only; GroupCount 0 is the old one-mask form.
                if level == 3 && kind == 0 {
                    let count = (u16_at(e, 38).unwrap_or(0) as usize).max(1);
                    l3s.push((masks(e, 40, count), bytes));
                }
            }
            _ => {}
        }
        at += size;
    }

    let mut all: Vec<(u16, u32)> = cores.iter().flat_map(|c| c.0.iter().copied()).collect();
    all.sort_unstable();
    all.dedup();
    let rank = |gb: &(u16, u32)| all.binary_search(gb).ok();
    let max_class = cores.iter().map(|c| c.1).max().unwrap_or(0);
    let classes_differ = cores.iter().any(|c| c.1 != max_class);

    let mut infos = Vec::new();
    for (threads, class) in &cores {
        let siblings: Vec<usize> = threads.iter().filter_map(rank).collect();
        let kind = match (classes_differ, *class == max_class) {
            (false, _) => CoreKind::Unknown,
            (true, true) => CoreKind::Performance,
            (true, false) => CoreKind::Efficiency,
        };
        for gb in threads {
            let Some(cpu) = rank(gb) else { continue };
            let l3 = l3s.iter().find(|(m, _)| m.contains(gb)).map(|(m, bytes)| (m.iter().filter_map(rank).collect(), Some(bytes / 1024)));
            infos.push(CpuInfo { cpu, siblings: siblings.clone(), l3, kind });
        }
    }
    infos.sort_by_key(|i| i.cpu);
    infos
}

#[cfg(windows)]
pub fn read(logical: usize) -> super::Topology {
    use windows::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationAll, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };

    let mut len = 0u32;
    // SAFETY: a null buffer asks for the size; the call fails with
    // ERROR_INSUFFICIENT_BUFFER and sets `len`.
    let _ = unsafe { GetLogicalProcessorInformationEx(RelationAll, None, &mut len) };
    if len == 0 {
        return super::fallback(logical);
    }
    // u64s keep the buffer aligned for the struct the API writes.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: the buffer holds `len` bytes, as the call asked for.
    let ok = unsafe {
        GetLogicalProcessorInformationEx(RelationAll, Some(buf.as_mut_ptr().cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>()), &mut len)
    };
    if ok.is_err() {
        return super::fallback(logical);
    }
    // SAFETY: plain bytes of the buffer just filled, `len` of them.
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), len as usize) };
    super::build(&parse(bytes), logical, "win32")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn affinity(out: &mut Vec<u8>, mask: u64, group: u16) {
        out.extend(mask.to_le_bytes());
        out.extend(group.to_le_bytes());
        out.extend([0u8; 6]);
    }

    fn core(class: u8, mask: u64) -> Vec<u8> {
        let mut e = vec![0u8; 32];
        e[0..4].copy_from_slice(&RELATION_PROCESSOR_CORE.to_le_bytes());
        e[9] = class;
        e[30..32].copy_from_slice(&1u16.to_le_bytes());
        affinity(&mut e, mask, 0);
        let size = e.len() as u32;
        e[4..8].copy_from_slice(&size.to_le_bytes());
        e
    }

    fn l3(bytes: u32, mask: u64) -> Vec<u8> {
        let mut e = vec![0u8; 40];
        e[0..4].copy_from_slice(&RELATION_CACHE.to_le_bytes());
        e[8] = 3;
        e[12..16].copy_from_slice(&bytes.to_le_bytes());
        e[38..40].copy_from_slice(&1u16.to_le_bytes());
        affinity(&mut e, mask, 0);
        let size = e.len() as u32;
        e[4..8].copy_from_slice(&size.to_le_bytes());
        e
    }

    /// Two P-cores with SMT (class 1) and two E-cores (class 0) under one
    /// 24 MiB L3.
    #[test]
    fn parses_a_hybrid_buffer() {
        let mut buf = Vec::new();
        buf.extend(core(1, 0b0000_0011));
        buf.extend(core(1, 0b0000_1100));
        buf.extend(core(0, 0b0001_0000));
        buf.extend(core(0, 0b0010_0000));
        buf.extend(l3(24 * 1024 * 1024, 0b0011_1111));
        let infos = parse(&buf);
        assert_eq!(infos.len(), 6);
        assert_eq!(infos[3].siblings, vec![2, 3]);
        assert_eq!(infos[3].kind, CoreKind::Performance);
        assert_eq!(infos[4].kind, CoreKind::Efficiency);
        assert_eq!(infos[5].l3, Some(((0..6).collect(), Some(24 * 1024))));
        let t = super::super::build(&infos, 6, "win32");
        let labels: Vec<&str> = t.groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, ["P-cores", "E-cores"]);
    }

    /// One class everywhere is no kind at all; a cut-off entry stops the walk.
    #[test]
    fn one_class_is_unknown_and_garbage_stops() {
        let mut buf = core(0, 1);
        buf.extend(core(0, 2));
        buf.extend([0u8, 0, 0, 0, 200, 0]);
        let infos = parse(&buf);
        assert_eq!(infos.len(), 2);
        assert!(infos.iter().all(|i| i.kind == CoreKind::Unknown && i.l3.is_none()));
    }
}
