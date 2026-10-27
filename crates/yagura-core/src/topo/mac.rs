//! macOS. On Apple silicon the device tree's `cpus` node has a child per
//! core with its `cluster-type` (`P` or `E`), `cluster-id` and
//! `logical-cpu-id`; a cluster shares an L2 and there is no L3, so the groups
//! are the clusters. The caches' sizes come from `sysctl hw.perflevelN`.
//! Intel Macs have no such nodes; `hw.physicalcpu` and `hw.logicalcpu` give
//! the cores, whose threads macOS numbers side by side (it sorts the CPUs by
//! their APIC id), and `hw.l3cachesize` the shared L3.

use super::{CoreKind, CpuInfo};

/// One core as the device tree describes it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Node {
    pub cpu: usize,
    pub kind: CoreKind,
    pub cluster: u32,
}

/// `cluster-type`: `P\0` or `E\0`.
pub fn cluster_kind(data: &[u8]) -> CoreKind {
    match data.first() {
        Some(b'P') => CoreKind::Performance,
        Some(b'E') => CoreKind::Efficiency,
        _ => CoreKind::Unknown,
    }
}

/// `hw.perflevelN.name`.
pub fn level_kind(name: &str) -> CoreKind {
    match name.trim_end_matches('\0').trim() {
        "Performance" => CoreKind::Performance,
        "Efficiency" => CoreKind::Efficiency,
        _ => CoreKind::Unknown,
    }
}

/// A device-tree number: a little-endian word.
pub fn word(data: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(..4)?.try_into().ok()?))
}

/// Each core a logical CPU of its own; a cluster (of one kind) shares the L2
/// that `l2_kb` gives for its kind.
pub fn from_tree(nodes: &[Node], l2_kb: &[(CoreKind, u64)]) -> Vec<CpuInfo> {
    nodes
        .iter()
        .map(|n| {
            let shared = nodes.iter().filter(|m| (m.kind, m.cluster) == (n.kind, n.cluster)).map(|m| m.cpu).collect();
            let size = l2_kb.iter().find(|(k, _)| *k == n.kind).map(|&(_, kb)| kb);
            CpuInfo { cpu: n.cpu, siblings: vec![n.cpu], l3: Some((shared, size)), kind: n.kind }
        })
        .collect()
}

/// `physical` cores of `logical / physical` threads side by side, all on one
/// L3. Nothing when the counts do not divide.
pub fn from_counts(physical: usize, logical: usize, l3_kb: Option<u64>) -> Vec<CpuInfo> {
    if physical == 0 || logical < physical || !logical.is_multiple_of(physical) {
        return Vec::new();
    }
    let per = logical / physical;
    let all: Vec<usize> = (0..logical).collect();
    (0..logical)
        .map(|cpu| {
            let first = cpu / per * per;
            CpuInfo { cpu, siblings: (first..first + per).collect(), l3: Some((all.clone(), l3_kb)), kind: CoreKind::Unknown }
        })
        .collect()
}

#[cfg(target_os = "macos")]
mod sys {
    use std::ffi::{CStr, CString};

    use objc2_core_foundation::{CFData, CFRetained, CFString, CFType};
    use objc2_io_kit::{
        IOIteratorNext, IOObjectRelease, IORegistryEntryCreateCFProperty, IORegistryEntryFromPath, IORegistryEntryGetChildIterator,
        io_object_t,
    };

    use super::{CoreKind, Node, cluster_kind, level_kind, word};

    /// `kIOMainPortDefault`.
    const MAIN_PORT: u32 = 0;

    fn sysctl_bytes(name: &str) -> Option<Vec<u8>> {
        let name = CString::new(name).ok()?;
        let mut buf = vec![0u8; 256];
        let mut len = buf.len();
        // SAFETY: a C string name and a buffer of `len` bytes; the length
        // written is the value's.
        let r = unsafe { libc::sysctlbyname(name.as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
        (r == 0).then(|| {
            buf.truncate(len);
            buf
        })
    }

    pub fn int(name: &str) -> Option<u64> {
        let b = sysctl_bytes(name)?;
        match b.len() {
            4 => Some(u64::from(u32::from_ne_bytes(b.try_into().ok()?))),
            8 => Some(u64::from_ne_bytes(b.try_into().ok()?)),
            _ => None,
        }
    }

    fn text(name: &str) -> Option<String> {
        let b = sysctl_bytes(name)?;
        Some(String::from_utf8_lossy(&b).trim_end_matches('\0').to_string())
    }

    /// The L2 of a cluster of each kind, in KiB.
    pub fn l2_by_kind() -> Vec<(CoreKind, u64)> {
        let levels = int("hw.nperflevels").unwrap_or(0).min(8);
        (0..levels)
            .filter_map(|i| {
                let kind = level_kind(&text(&format!("hw.perflevel{i}.name"))?);
                let bytes = int(&format!("hw.perflevel{i}.l2cachesize"))?;
                (bytes > 0).then_some((kind, bytes / 1024))
            })
            .collect()
    }

    /// An IOKit object, released when dropped.
    struct Obj(io_object_t);

    impl Drop for Obj {
        fn drop(&mut self) {
            IOObjectRelease(self.0);
        }
    }

    fn data(e: io_object_t, key: &str) -> Option<Vec<u8>> {
        let key = CFString::from_str(key);
        // SAFETY: a live entry and a string key; the default allocator; no options.
        let v: CFRetained<CFType> = unsafe { IORegistryEntryCreateCFProperty(e, Some(&key), None, 0) }?;
        Some(v.downcast_ref::<CFData>()?.to_vec())
    }

    /// The device tree's cores; none on an Intel Mac.
    pub fn nodes() -> Vec<Node> {
        const PATH: &CStr = c"IODeviceTree:/cpus";
        const PLANE: &CStr = c"IODeviceTree";
        // SAFETY: a C string path; the entry is released by `Obj`.
        let cpus = Obj(unsafe { IORegistryEntryFromPath(MAIN_PORT, PATH.as_ptr().cast_mut().cast()) });
        if cpus.0 == 0 {
            return Vec::new();
        }
        let mut iter = 0;
        // SAFETY: a live entry and a plane name that is a C string; the
        // iterator is written on success and released by `Obj`.
        if unsafe { IORegistryEntryGetChildIterator(cpus.0, PLANE.as_ptr().cast_mut().cast(), &mut iter) } != 0 {
            return Vec::new();
        }
        let iter = Obj(iter);
        let mut out = Vec::new();
        loop {
            // Each child is released by `Obj`.
            let child = Obj(IOIteratorNext(iter.0));
            if child.0 == 0 {
                break;
            }
            let (Some(kind), Some(cpu)) = (data(child.0, "cluster-type"), data(child.0, "logical-cpu-id").and_then(|d| word(&d))) else {
                continue;
            };
            let cluster = data(child.0, "cluster-id").and_then(|d| word(&d)).unwrap_or(0);
            out.push(Node { cpu: cpu as usize, kind: cluster_kind(&kind), cluster });
        }
        out
    }
}

/// Read this Mac's topology for `logical` CPUs.
#[cfg(target_os = "macos")]
pub fn read(logical: usize) -> super::Topology {
    let nodes = sys::nodes();
    if !nodes.is_empty() {
        return super::build_at(&from_tree(&nodes, &sys::l2_by_kind()), logical, "iokit", 2);
    }
    let n = |k: &str| sys::int(k).unwrap_or(0) as usize;
    let l3 = sys::int("hw.l3cachesize").filter(|&b| b > 0).map(|b| b / 1024);
    let infos = from_counts(n("hw.physicalcpu"), n("hw.logicalcpu"), l3);
    if infos.len() != logical {
        return super::fallback(logical);
    }
    super::build_at(&infos, logical, "sysctl", 3)
}

#[cfg(test)]
mod tests {
    use super::super::build_at;
    use super::*;

    /// An M1 Pro: two E-cores (cluster 0), then eight P-cores in two clusters
    /// of four, as the device tree lists them.
    #[test]
    fn apple_silicon_groups_by_cluster() {
        let mut nodes =
            vec![Node { cpu: 0, kind: CoreKind::Efficiency, cluster: 0 }, Node { cpu: 1, kind: CoreKind::Efficiency, cluster: 0 }];
        nodes.extend((2..10).map(|cpu| Node { cpu, kind: CoreKind::Performance, cluster: if cpu < 6 { 1 } else { 2 } }));
        let l2 = [(CoreKind::Performance, 12288), (CoreKind::Efficiency, 4096)];
        let t = build_at(&from_tree(&nodes, &l2), 10, "iokit", 2);
        let labels: Vec<&str> = t.groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, ["E-cores · L2 #0", "P-cores · L2 #1", "P-cores · L2 #2"]);
        assert_eq!(
            t.groups.iter().map(|g| (g.cache, g.cache_kb)).collect::<Vec<_>>(),
            [(2, Some(4096)), (2, Some(12288)), (2, Some(12288))]
        );
        assert_eq!(t.groups[2].cores[0].threads, vec![6]);
        assert_eq!(t.summary(), "10C/10T · 3 groups");
    }

    /// A plain M1: one cluster of each kind, so no cache number.
    #[test]
    fn one_cluster_of_each_kind_is_named_by_kind() {
        let nodes: Vec<Node> = (0..8)
            .map(|cpu| Node { cpu, kind: if cpu < 4 { CoreKind::Efficiency } else { CoreKind::Performance }, cluster: u32::from(cpu >= 4) })
            .collect();
        let t = build_at(&from_tree(&nodes, &[]), 8, "iokit", 2);
        let labels: Vec<&str> = t.groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, ["E-cores · L2 #0", "P-cores · L2 #1"]);
        assert_eq!(t.groups[0].cache_kb, None);
    }

    #[test]
    fn device_tree_values() {
        assert_eq!(cluster_kind(b"P\0"), CoreKind::Performance);
        assert_eq!(cluster_kind(b"E\0"), CoreKind::Efficiency);
        assert_eq!(cluster_kind(b""), CoreKind::Unknown);
        assert_eq!(level_kind("Efficiency\0"), CoreKind::Efficiency);
        assert_eq!(word(&[5, 0, 0, 0]), Some(5));
        assert_eq!(word(&[5, 0]), None);
    }

    /// A quad-core i7 with Hyper-Threading: threads side by side.
    #[test]
    fn intel_macs_pair_their_threads() {
        let t = build_at(&from_counts(4, 8, Some(8192)), 8, "sysctl", 3);
        assert_eq!(t.groups.len(), 1);
        assert_eq!(t.groups[0].label, "CPU");
        assert_eq!(t.groups[0].cores[1].threads, vec![2, 3]);
        assert_eq!(t.summary(), "4C/8T");
        assert!(from_counts(3, 8, None).is_empty());
        assert!(from_counts(0, 0, None).is_empty());
    }
}
