//! How the logical CPUs sit on the chip: which share a core (SMT), which
//! share a cache (the L3; the L2 of a cluster on Apple silicon), which are
//! performance and which efficiency cores.
//! Read once at startup; the CPU screen draws its tiles from it.
//!
//! Logical CPUs are numbered by their position in sysinfo's list (the online
//! CPUs in order), not by the OS's own numbers, so a tile's thread is the
//! usage at the same index.

// Other systems draw `fallback`.
#![cfg_attr(not(any(target_os = "linux", target_os = "macos", windows)), allow(dead_code))]

use serde::{Deserialize, Serialize};

use crate::tr;

#[cfg(any(target_os = "linux", test))]
pub mod linux;
#[cfg(any(target_os = "macos", test))]
pub mod mac;
#[cfg(any(windows, test))]
pub mod windows;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum CoreKind {
    Unknown,
    Performance,
    Efficiency,
}

/// What the OS says about one logical CPU, in positions.
#[derive(Clone, Debug, PartialEq)]
pub struct CpuInfo {
    pub cpu: usize,
    /// The logical CPUs on the same core, itself included.
    pub siblings: Vec<usize>,
    /// The CPUs sharing its last cache (the L3, or the cluster's L2 where
    /// there is no L3) and the cache's size in KiB, when known.
    pub l3: Option<(Vec<usize>, Option<u64>)>,
    pub kind: CoreKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Core {
    /// Counted from 0 across the chip: the `C3` on its tile.
    pub id: usize,
    pub threads: Vec<usize>,
    pub kind: CoreKind,
}

/// Cores sharing a cache (and of one kind): a CCD on Ryzen, the P- or
/// E-cores on a hybrid Intel chip, a cluster on Apple silicon, the whole chip
/// on most others.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub label: String,
    /// The shared cache's level (3, or 2 on Apple silicon) and size.
    pub cache: u8,
    pub cache_kb: Option<u64>,
    pub kind: CoreKind,
    pub cores: Vec<Core>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Topology {
    pub groups: Vec<Group>,
    pub logical: usize,
    /// Where it came from: `sysfs`, `win32`, `iokit` or `sysctl` (macOS), or
    /// `fallback` when the OS said
    /// nothing usable and each logical CPU is drawn as a core of its own.
    #[serde(deserialize_with = "crate::wire::name")]
    pub source: crate::Name,
}

impl Topology {
    pub fn cores(&self) -> impl Iterator<Item = &Core> {
        self.groups.iter().flat_map(|g| &g.cores)
    }

    pub fn core_count(&self) -> usize {
        self.groups.iter().map(|g| g.cores.len()).sum()
    }

    /// `8C/16T · 2 L3` — the line over the compact tiles.
    pub fn summary(&self) -> String {
        let mut s = format!("{}C/{}T", self.core_count(), self.logical);
        if self.groups.len() > 1 {
            s += &format!(" · {} {}", self.groups.len(), tr("topo.groups"));
        }
        s
    }
}

/// A core as `build` finds it: its threads, its kind, the CPUs that share
/// its L3 and that cache's size.
type Placed = (Vec<usize>, CoreKind, Vec<usize>, Option<u64>);

/// One core per logical CPU in one group: what is drawn when the OS says
/// nothing or says something that does not add up.
pub fn fallback(logical: usize) -> Topology {
    let cores = (0..logical).map(|i| Core { id: i, threads: vec![i], kind: CoreKind::Unknown }).collect();
    Topology {
        groups: vec![Group { label: tr("topo.cpu"), cache: 3, cache_kb: None, kind: CoreKind::Unknown, cores }],
        logical,
        source: "fallback",
    }
}

/// Cores from the sibling sets, groups from the L3 sets and kinds. A CPU the
/// OS skipped becomes a core of its own, so every usage has a tile.
#[cfg(any(target_os = "linux", windows, test))]
pub fn build(infos: &[CpuInfo], logical: usize, source: &'static str) -> Topology {
    build_at(infos, logical, source, 3)
}

/// [`build`] with the sets shared at cache level `cache`.
pub fn build_at(infos: &[CpuInfo], logical: usize, source: &'static str, cache: u8) -> Topology {
    if infos.is_empty() || logical == 0 {
        return fallback(logical);
    }
    let mut infos: Vec<&CpuInfo> = infos.iter().filter(|i| i.cpu < logical).collect();
    infos.sort_by_key(|i| i.cpu);
    let info_of = |cpu: usize| infos.iter().find(|i| i.cpu == cpu).copied();

    // Cores in the order of their first thread.
    let mut cores: Vec<Placed> = Vec::new();
    let mut placed = vec![false; logical];
    for cpu in 0..logical {
        if placed[cpu] {
            continue;
        }
        let (mut threads, kind, l3, size) = match info_of(cpu) {
            Some(i) => {
                let (l3, size) = i.l3.clone().unwrap_or_default();
                (i.siblings.clone(), i.kind, l3, size)
            }
            None => (vec![cpu], CoreKind::Unknown, Vec::new(), None),
        };
        threads.retain(|&t| t < logical && !placed[t]);
        if !threads.contains(&cpu) {
            threads.push(cpu);
        }
        threads.sort_unstable();
        threads.dedup();
        for &t in &threads {
            placed[t] = true;
        }
        let mut l3 = l3;
        l3.sort_unstable();
        cores.push((threads, kind, l3, size));
    }

    // Groups by (L3 set, kind), in the order they first appear.
    let mut keys: Vec<(Vec<usize>, CoreKind)> = Vec::new();
    let mut groups: Vec<Group> = Vec::new();
    for (id, (threads, kind, l3, size)) in cores.into_iter().enumerate() {
        let key = (l3, kind);
        let gi = match keys.iter().position(|k| *k == key) {
            Some(gi) => gi,
            None => {
                keys.push(key);
                groups.push(Group { label: String::new(), cache, cache_kb: size, kind, cores: Vec::new() });
                groups.len() - 1
            }
        };
        groups[gi].cores.push(Core { id, threads, kind });
    }

    // Labels: "CPU" alone; else by kind where kinds differ, and by L3 where
    // there are several caches.
    let mut l3_sets: Vec<&Vec<usize>> = keys.iter().map(|k| &k.0).collect();
    l3_sets.sort();
    l3_sets.dedup();
    let kinds_differ = keys.iter().any(|k| k.1 != keys[0].1);
    let several = groups.len() > 1;
    for (g, key) in groups.iter_mut().zip(&keys) {
        let mut parts = Vec::new();
        if kinds_differ {
            parts.push(match g.kind {
                CoreKind::Performance => tr("topo.p_cores").to_string(),
                CoreKind::Efficiency => tr("topo.e_cores").to_string(),
                CoreKind::Unknown => tr("topo.cores").to_string(),
            });
        }
        if l3_sets.len() > 1 {
            let n = l3_sets.iter().position(|s| **s == key.0).unwrap_or(0);
            parts.push(format!("L{cache} #{n}"));
        }
        g.label = if !several || parts.is_empty() { tr("topo.cpu").to_string() } else { parts.join(" · ") };
    }
    Topology { groups, logical, source }
}

/// A kernel CPU list: `0-3,8-11` → 0, 1, 2, 3, 8, 9, 10, 11. `None` when it
/// does not read as one.
#[cfg(any(target_os = "linux", test))]
pub fn parse_cpu_list(s: &str) -> Option<Vec<usize>> {
    let s = s.trim();
    let mut out = Vec::new();
    if s.is_empty() {
        return Some(out);
    }
    for part in s.split(',') {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (usize, usize) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
                if b < a || b - a > 65536 {
                    return None;
                }
                out.extend(a..=b);
            }
            None => out.push(part.trim().parse().ok()?),
        }
    }
    Some(out)
}

/// Read this machine's topology for `logical` CPUs.
pub fn read(logical: usize) -> Topology {
    #[cfg(target_os = "linux")]
    let t = linux::read(std::path::Path::new("/sys/devices"), logical);
    #[cfg(windows)]
    let t = windows::read(logical);
    #[cfg(target_os = "macos")]
    let t = mac::read(logical);
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    let t = fallback(logical);
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cpu_lists() {
        assert_eq!(parse_cpu_list("0-3,8-11"), Some(vec![0, 1, 2, 3, 8, 9, 10, 11]));
        assert_eq!(parse_cpu_list("5\n"), Some(vec![5]));
        assert_eq!(parse_cpu_list(""), Some(vec![]));
        assert_eq!(parse_cpu_list("3-1"), None);
        assert_eq!(parse_cpu_list("a"), None);
    }

    fn info(cpu: usize, siblings: &[usize], l3: &[usize], kind: CoreKind) -> CpuInfo {
        CpuInfo { cpu, siblings: siblings.to_vec(), l3: Some((l3.to_vec(), Some(32768))), kind }
    }

    /// Two CCDs of four SMT cores each, numbered the Linux way (the second
    /// thread of core n is n + 16).
    #[test]
    fn two_ccds_with_smt() {
        let mut infos = Vec::new();
        for c in 0..16 {
            let l3: Vec<usize> = if c < 8 { (0..8).chain(16..24).collect() } else { (8..16).chain(24..32).collect() };
            infos.push(info(c, &[c, c + 16], &l3, CoreKind::Unknown));
            infos.push(info(c + 16, &[c, c + 16], &l3, CoreKind::Unknown));
        }
        let t = build(&infos, 32, "test");
        assert_eq!(t.groups.len(), 2);
        assert_eq!(t.groups[0].label, "L3 #0");
        assert_eq!(t.groups[1].cores.len(), 8);
        assert_eq!(t.groups[1].cores[0].threads, vec![8, 24]);
        assert!(t.cores().all(|c| c.threads.len() == 2));
        assert_eq!(t.summary(), "16C/32T · 2 groups");
    }

    /// A hybrid chip: P-cores with SMT, E-cores without, one L3.
    #[test]
    fn hybrid_cores_group_by_kind() {
        let l3: Vec<usize> = (0..8).collect();
        let mut infos = Vec::new();
        for p in 0..2 {
            infos.push(info(2 * p, &[2 * p, 2 * p + 1], &l3, CoreKind::Performance));
            infos.push(info(2 * p + 1, &[2 * p, 2 * p + 1], &l3, CoreKind::Performance));
        }
        for e in 4..8 {
            infos.push(info(e, &[e], &l3, CoreKind::Efficiency));
        }
        let t = build(&infos, 8, "test");
        let labels: Vec<&str> = t.groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, ["P-cores", "E-cores"]);
        assert_eq!(t.core_count(), 6);
        assert_eq!(t.groups[1].cores[0].id, 2);
    }

    /// A CPU the OS left out still gets a tile; nothing is one group "CPU".
    #[test]
    fn gaps_and_nothing_fall_back() {
        let l3 = [0, 1];
        let t = build(&[info(0, &[0], &l3, CoreKind::Unknown)], 2, "test");
        assert_eq!(t.groups.len(), 2);
        assert_eq!(t.core_count(), 2);
        let f = build(&[], 4, "test");
        assert_eq!(f.source, "fallback");
        assert_eq!(f.groups[0].label, "CPU");
        assert_eq!(f.core_count(), 4);
    }
}
