//! The topology from sysfs: `/sys/devices/system/cpu/cpuN/topology` for the
//! cores, `cache/index3` for the L3, and `cpu_core` / `cpu_atom` (Intel's
//! hybrid chips) or `cpu_capacity` (Arm's big.LITTLE) for the kinds.

use std::path::Path;

use super::{CoreKind, CpuInfo, Topology, build, fallback, parse_cpu_list};

fn read_list(path: &Path) -> Option<Vec<usize>> {
    parse_cpu_list(&std::fs::read_to_string(path).ok()?)
}

/// `33792K` → 33792; `32M` → 32768.
fn parse_size_kb(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mul) = match s.as_bytes().last()? {
        b'K' => (&s[..s.len() - 1], 1),
        b'M' => (&s[..s.len() - 1], 1024),
        b'G' => (&s[..s.len() - 1], 1024 * 1024),
        _ => (s, 1),
    };
    num.parse::<u64>().ok().map(|n| n * mul)
}

/// Read under `sys_devices` (`/sys/devices`, or a fake tree in a test).
pub fn read(sys_devices: &Path, logical: usize) -> Topology {
    let cpu_dir = sys_devices.join("system/cpu");
    let Some(online) = read_list(&cpu_dir.join("online")) else { return fallback(logical) };
    if online.len() != logical {
        // sysinfo and the kernel disagree (a CPU went on- or offline in
        // between); positions would not line up.
        return fallback(logical);
    }
    // The OS's number → its position among the online CPUs.
    let pos = |n: usize| online.iter().position(|&o| o == n);
    let to_pos = |list: Vec<usize>| -> Vec<usize> { list.into_iter().filter_map(pos).collect() };

    let p_cores = read_list(&sys_devices.join("cpu_core/cpus"));
    let e_cores = read_list(&sys_devices.join("cpu_atom/cpus"));
    let capacity =
        |n: usize| -> Option<u64> { std::fs::read_to_string(cpu_dir.join(format!("cpu{n}/cpu_capacity"))).ok()?.trim().parse().ok() };
    let caps: Vec<Option<u64>> = online.iter().map(|&n| capacity(n)).collect();
    let max_cap = caps.iter().flatten().max().copied();
    let caps_differ = caps.iter().flatten().any(|c| Some(*c) != max_cap);

    let mut infos = Vec::new();
    for (i, &n) in online.iter().enumerate() {
        let base = cpu_dir.join(format!("cpu{n}"));
        let siblings = read_list(&base.join("topology/thread_siblings_list"))
            .or_else(|| read_list(&base.join("topology/core_cpus_list")))
            .map(to_pos)
            .unwrap_or_else(|| vec![i]);
        let l3 = read_list(&base.join("cache/index3/shared_cpu_list")).map(|list| {
            let size = std::fs::read_to_string(base.join("cache/index3/size")).ok().and_then(|s| parse_size_kb(&s));
            (to_pos(list), size)
        });
        let kind = match (&p_cores, &e_cores) {
            (Some(p), _) if p.contains(&n) => CoreKind::Performance,
            (_, Some(e)) if e.contains(&n) => CoreKind::Efficiency,
            _ if caps_differ => match caps[i] {
                Some(c) if Some(c) == max_cap => CoreKind::Performance,
                Some(_) => CoreKind::Efficiency,
                None => CoreKind::Unknown,
            },
            _ => CoreKind::Unknown,
        };
        infos.push(CpuInfo { cpu: i, siblings, l3, kind });
    }
    build(&infos, logical, "sysfs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::test_dir;

    fn put(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn sizes_read_in_kib() {
        assert_eq!(parse_size_kb("33792K\n"), Some(33792));
        assert_eq!(parse_size_kb("32M"), Some(32768));
        assert_eq!(parse_size_kb("512"), Some(512));
        assert_eq!(parse_size_kb("x"), None);
    }

    /// A hybrid chip with CPU 1 offline: two P threads on one core (0, 2),
    /// one E-core (3). Positions skip the offline CPU.
    #[test]
    fn reads_a_fake_hybrid_sysfs() {
        let d = test_dir("topo-sysfs");
        put(&d, "system/cpu/online", "0,2-3\n");
        put(&d, "cpu_core/cpus", "0-2\n");
        put(&d, "cpu_atom/cpus", "3\n");
        for (n, sib) in [(0, "0,2"), (2, "0,2"), (3, "3")] {
            put(&d, &format!("system/cpu/cpu{n}/topology/thread_siblings_list"), sib);
            put(&d, &format!("system/cpu/cpu{n}/cache/index3/shared_cpu_list"), "0-3");
            put(&d, &format!("system/cpu/cpu{n}/cache/index3/size"), "12288K");
        }
        let t = read(&d, 3);
        assert_eq!(t.source, "sysfs");
        assert_eq!(t.groups.len(), 2);
        assert_eq!(t.groups[0].label, "P-cores");
        assert_eq!(t.groups[0].cores[0].threads, vec![0, 1]);
        assert_eq!(t.groups[0].cache_kb, Some(12288));
        assert_eq!(t.groups[1].cores[0].threads, vec![2]);
        // A count that does not match the online list is not trusted.
        assert_eq!(read(&d, 4).source, "fallback");
    }

    /// big.LITTLE: no cpu_core / cpu_atom, the capacities tell.
    #[test]
    fn capacities_tell_big_from_little() {
        let d = test_dir("topo-capacity");
        put(&d, "system/cpu/online", "0-3\n");
        for n in 0..4 {
            put(&d, &format!("system/cpu/cpu{n}/topology/thread_siblings_list"), &n.to_string());
            put(&d, &format!("system/cpu/cpu{n}/cpu_capacity"), if n < 2 { "446" } else { "1024" });
        }
        let t = read(&d, 4);
        let labels: Vec<&str> = t.groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, ["E-cores", "P-cores"]);
    }
}
