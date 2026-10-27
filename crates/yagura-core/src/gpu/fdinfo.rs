//! Linux: what each process does on a GPU, from the DRM fdinfo
//! (`/proc/<pid>/fdinfo/<fd>` of a `/dev/dri` file). i915, xe, amdgpu,
//! panfrost, msm and others write one block per client: the device
//! (`drm-pdev`), the client's id, each engine's busy time (`drm-engine-*`, in
//! ns; xe gives `drm-cycles-*` over `drm-total-cycles-*` instead) and the
//! memory it holds. Two readings make a percent. A client opened by several
//! fds (or handed to another process) is counted once. Only the processes
//! that may be read are seen: one's own, or all of them as root.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::{Duration, Instant};

use super::{Device, Field, GpuProc, Why};

/// An engine's busy count.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Busy {
    /// Nanoseconds.
    Ns(u64),
    /// xe: cycles busy, and the GPU's cycles in all.
    Cycles(u64, u64),
}

/// One DRM client as an fdinfo file describes it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Client {
    pub driver: String,
    /// The PCI address, `0000:00:02.0`; none for devices not on PCI.
    pub pdev: Option<String>,
    pub id: u64,
    pub engines: BTreeMap<String, Busy>,
    /// Engines of a class that run side by side (`drm-engine-capacity-*`).
    pub capacity: HashMap<String, u32>,
    /// Device memory it holds, in bytes.
    pub vram: Option<u64>,
}

impl Client {
    /// What the client's device is known by: its PCI address, else its driver.
    pub fn device(&self) -> String {
        match &self.pdev {
            Some(p) => p.to_ascii_lowercase(),
            None => format!("driver:{}", self.driver),
        }
    }
}

/// `1234 KiB`, `5 MiB` or a plain number of bytes.
fn size(v: &str) -> Option<u64> {
    let mut it = v.split_whitespace();
    let n: u64 = it.next()?.parse().ok()?;
    Some(match it.next() {
        Some("KiB") => n << 10,
        Some("MiB") => n << 20,
        Some("GiB") => n << 30,
        _ => n,
    })
}

/// The number at the start of `v` (`9288864723 ns`).
fn lead(v: &str) -> Option<u64> {
    v.split_whitespace().next()?.parse().ok()
}

/// One fdinfo file; `None` when it is not a DRM client's.
pub fn parse(text: &str) -> Option<Client> {
    let mut c = Client::default();
    let mut id = None;
    let mut cycles: HashMap<String, u64> = HashMap::new();
    let mut total: HashMap<String, u64> = HashMap::new();
    let (mut resident, mut memory) = (None::<u64>, None::<u64>);
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        if k == "drm-driver" {
            c.driver = v.to_string();
        } else if k == "drm-pdev" {
            c.pdev = Some(v.to_string());
        } else if k == "drm-client-id" {
            id = v.parse().ok();
        } else if let Some(e) = k.strip_prefix("drm-engine-capacity-") {
            if let Ok(n) = v.parse() {
                c.capacity.insert(e.to_string(), n);
            }
        } else if let Some(e) = k.strip_prefix("drm-engine-") {
            if let Some(ns) = lead(v) {
                c.engines.insert(e.to_string(), Busy::Ns(ns));
            }
        } else if let Some(e) = k.strip_prefix("drm-total-cycles-") {
            total.extend(lead(v).map(|n| (e.to_string(), n)));
        } else if let Some(e) = k.strip_prefix("drm-cycles-") {
            cycles.extend(lead(v).map(|n| (e.to_string(), n)));
        } else if k.starts_with("drm-resident-vram") || k.starts_with("drm-resident-local") {
            resident = Some(resident.unwrap_or(0) + size(v)?);
        } else if k == "drm-memory-vram" {
            memory = size(v);
        }
    }
    for (e, n) in cycles {
        if let Some(&t) = total.get(&e) {
            c.engines.insert(e, Busy::Cycles(n, t));
        }
    }
    c.id = id?;
    c.vram = resident.or(memory);
    (!c.driver.is_empty()).then_some(c)
}

/// The engine's name as the screen shows it.
pub fn engine_name(e: &str) -> String {
    match e {
        "render" | "rcs" | "gfx" => "Render",
        "copy" | "bcs" => "Copy",
        "video" | "vcs" | "dec" => "Video",
        "video-enhance" | "vecs" => "VideoEnhance",
        "compute" | "ccs" => "Compute",
        "enc" => "Encode",
        _ => return e.to_string(),
    }
    .to_string()
}

/// Percent busy between two counts, for `cap` engines side by side.
pub fn busy_pct(prev: Busy, now: Busy, wall: Duration, cap: u32) -> Option<f32> {
    let share = match (prev, now) {
        (Busy::Ns(a), Busy::Ns(b)) => {
            let w = wall.as_nanos() as f64;
            if w <= 0.0 {
                return None;
            }
            b.saturating_sub(a) as f64 / w
        }
        (Busy::Cycles(a, ta), Busy::Cycles(b, tb)) => {
            let t = tb.saturating_sub(ta);
            if t == 0 {
                return None;
            }
            b.saturating_sub(a) as f64 / t as f64
        }
        _ => return None,
    };
    Some((share * 100.0 / f64::from(cap.max(1))).clamp(0.0, 100.0) as f32)
}

/// What one device's clients did since the last reading.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tally {
    /// Per engine (as the screen names it), percent, all clients together.
    pub engines: BTreeMap<String, f32>,
    /// False on the first reading, when there is nothing to compare with.
    pub rated: bool,
    pub procs: Vec<GpuProc>,
}

/// The last counts of each client, by device and client id.
#[derive(Debug, Default)]
pub struct Prev(HashMap<(String, u64), (Instant, BTreeMap<String, Busy>)>);

/// Turn this reading's clients (with the process holding each) into a tally
/// per device, keeping the counts for next time.
pub fn tally(prev: &mut Prev, now: Instant, clients: &[(u32, Client)]) -> HashMap<String, Tally> {
    let mut out: HashMap<String, Tally> = HashMap::new();
    let mut seen = HashMap::new();
    for (pid, c) in clients {
        let key = (c.device(), c.id);
        if seen.contains_key(&key) {
            continue;
        }
        let t = out.entry(key.0.clone()).or_default();
        let mut usage: Option<f32> = None;
        if let Some((at, before)) = prev.0.get(&key) {
            t.rated = true;
            let wall = now.saturating_duration_since(*at);
            for (e, &b) in &c.engines {
                let Some(&a) = before.get(e) else { continue };
                let Some(p) = busy_pct(a, b, wall, c.capacity.get(e).copied().unwrap_or(1)) else { continue };
                let sum = t.engines.entry(engine_name(e)).or_default();
                *sum = (*sum + p).min(100.0);
                usage = Some(usage.map_or(p, |u| u.max(p)));
            }
        }
        match t.procs.iter_mut().find(|g| g.pid == *pid) {
            Some(g) => {
                g.usage = match (g.usage, usage) {
                    (Some(a), Some(b)) => Some((a + b).min(100.0)),
                    (a, b) => a.or(b),
                };
                g.mem = match (g.mem, c.vram) {
                    (Some(a), Some(b)) => Some(a + b),
                    (a, b) => a.or(b),
                };
            }
            None => t.procs.push(GpuProc { pid: *pid, usage, mem: c.vram }),
        }
        seen.insert(key, (now, c.engines.clone()));
    }
    prev.0 = seen;
    for t in out.values_mut() {
        t.procs.sort_by(|a, b| b.usage.unwrap_or(0.0).total_cmp(&a.usage.unwrap_or(0.0)).then(b.mem.cmp(&a.mem)).then(a.pid.cmp(&b.pid)));
    }
    out
}

/// Lay a device's tally over what sysfs gave: its processes always, and its
/// usage and engines where sysfs had no busy percent (i915, xe, Arm GPUs).
pub fn apply(d: &mut Device, t: &Tally) {
    d.procs = t.procs.clone();
    if d.usage.is_none() && t.rated {
        d.usage = Some(t.engines.values().copied().fold(0.0, f32::max));
        d.engines.extend(t.engines.iter().map(|(e, &p)| (e.clone(), p)));
        d.why.retain(|(f, w)| !(*f == Field::Usage && *w == Why::NoBusy));
    }
    d.source = "sysfs+fdinfo";
}

/// What a device is known by in the tallies.
pub fn device_key(d: &Device) -> Option<String> {
    match (&d.bus, &d.driver) {
        (Some(b), _) => Some(b.clone()),
        (None, Some(drv)) => Some(format!("driver:{drv}")),
        _ => None,
    }
}

/// Which fds of which processes are DRM files. Finding them means reading
/// every link in `/proc/*/fd`, so that is done every few seconds; between
/// times only the fds found are read again.
#[derive(Debug, Default)]
pub struct Scan {
    fds: Vec<(u32, u32)>,
    last: Option<Instant>,
}

const RESCAN: Duration = Duration::from_secs(5);

impl Scan {
    fn find(&mut self, proc: &Path) {
        self.fds.clear();
        let Ok(rd) = std::fs::read_dir(proc) else { return };
        for e in rd.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            let Ok(fds) = std::fs::read_dir(e.path().join("fd")) else { continue };
            for f in fds.flatten() {
                let Some(fd) = f.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
                if std::fs::read_link(f.path()).is_ok_and(|t| t.starts_with("/dev/dri/")) {
                    self.fds.push((pid, fd));
                }
            }
        }
        self.fds.sort_unstable();
    }

    /// Every DRM client under `proc` (`/proc`, or a fake tree in a test).
    pub fn read(&mut self, proc: &Path, now: Instant) -> Vec<(u32, Client)> {
        if self.last.is_none_or(|at| now.saturating_duration_since(at) >= RESCAN) {
            self.find(proc);
            self.last = Some(now);
        }
        let mut out = Vec::new();
        self.fds.retain(|&(pid, fd)| {
            let Ok(text) = std::fs::read_to_string(proc.join(format!("{pid}/fdinfo/{fd}"))) else { return false };
            out.extend(parse(&text).map(|c| (pid, c)));
            true
        });
        out
    }
}

/// The scan and the last counts, kept by the sampler.
#[derive(Debug, Default)]
pub struct Reader {
    scan: Scan,
    prev: Prev,
}

impl Reader {
    /// Fill the devices sysfs listed (not NVML's) from `/proc`.
    pub fn fill(&mut self, proc: &Path, devs: &mut [Device], now: Instant) {
        let clients = self.scan.read(proc, now);
        let tallies = tally(&mut self.prev, now, &clients);
        for d in devs.iter_mut().filter(|d| d.source == "sysfs") {
            if let Some(t) = device_key(d).and_then(|k| tallies.get(&k)) {
                apply(d, t);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const I915: &str = "pos:\t0\nflags:\t02100002\nmnt_id:\t26\nino:\t685\n\
drm-driver:\ti915\ndrm-client-id:\t7\ndrm-pdev:\t0000:00:02.0\n\
drm-total-system0:\t12288 KiB\ndrm-resident-system0:\t12288 KiB\n\
drm-engine-render:\t1000000000 ns\ndrm-engine-copy:\t0 ns\n\
drm-engine-video:\t0 ns\ndrm-engine-capacity-video:\t2\ndrm-engine-video-enhance:\t0 ns\n";

    #[test]
    fn i915_fdinfo_is_read() {
        let c = parse(I915).unwrap();
        assert_eq!((c.driver.as_str(), c.id, c.device()), ("i915", 7, "0000:00:02.0".to_string()));
        assert_eq!(c.engines.get("render"), Some(&Busy::Ns(1_000_000_000)));
        assert_eq!(c.engines.len(), 4);
        assert_eq!(c.capacity.get("video"), Some(&2));
        // System memory is not the device's own.
        assert_eq!(c.vram, None);
        assert_eq!(parse("pos:\t0\nflags:\t02\n"), None);
    }

    #[test]
    fn xe_and_amdgpu_fdinfo_are_read() {
        let xe = "drm-driver:\txe\ndrm-client-id:\t3\ndrm-pdev:\t0000:03:00.0\n\
drm-resident-vram0:\t2048 KiB\ndrm-cycles-rcs:\t500\ndrm-total-cycles-rcs:\t10000\ndrm-cycles-bcs:\t0\n";
        let c = parse(xe).unwrap();
        assert_eq!(c.engines.get("rcs"), Some(&Busy::Cycles(500, 10000)));
        // No total for bcs: left out.
        assert_eq!(c.engines.len(), 1);
        assert_eq!(c.vram, Some(2 << 20));
        let amd = "drm-driver:\tamdgpu\ndrm-pdev:\t0000:0c:00.0\ndrm-client-id:\t41\n\
drm-memory-vram:\t1024 KiB\ndrm-memory-gtt:\t8 KiB\ndrm-engine-gfx:\t300 ns\ndrm-engine-dec:\t0 ns\n";
        let c = parse(amd).unwrap();
        assert_eq!((c.vram, c.engines.get("gfx")), (Some(1 << 20), Some(&Busy::Ns(300))));
        let arm = parse("drm-driver:\tpanfrost\ndrm-client-id:\t1\ndrm-engine-fragment:\t0 ns\n").unwrap();
        assert_eq!(arm.device(), "driver:panfrost");
    }

    #[test]
    fn busy_counts_turn_into_percents() {
        let s = Duration::from_secs(1);
        assert_eq!(busy_pct(Busy::Ns(0), Busy::Ns(250_000_000), s, 1), Some(25.0));
        assert_eq!(busy_pct(Busy::Ns(0), Busy::Ns(1_000_000_000), s, 2), Some(50.0));
        assert_eq!(busy_pct(Busy::Cycles(0, 0), Busy::Cycles(300, 1000), s, 1), Some(30.0));
        assert_eq!(busy_pct(Busy::Ns(5), Busy::Ns(1), s, 1), Some(0.0));
        assert_eq!(busy_pct(Busy::Ns(0), Busy::Ns(1), Duration::ZERO, 1), None);
        assert_eq!(busy_pct(Busy::Ns(0), Busy::Cycles(1, 1), s, 1), None);
    }

    fn client(id: u64, render_ns: u64, vram: Option<u64>) -> Client {
        Client {
            driver: "i915".into(),
            pdev: Some("0000:00:02.0".into()),
            id,
            engines: BTreeMap::from([("render".into(), Busy::Ns(render_ns)), ("copy".into(), Busy::Ns(0))]),
            vram,
            ..Client::default()
        }
    }

    #[test]
    fn clients_add_up_per_process_and_device() {
        let mut prev = Prev::default();
        let t0 = Instant::now();
        // Process 10 has two clients, one of them open twice; 20 has one.
        let first =
            [(10, client(1, 0, Some(1 << 20))), (10, client(1, 0, Some(1 << 20))), (10, client(2, 0, None)), (20, client(3, 0, None))];
        let t = &tally(&mut prev, t0, &first)["0000:00:02.0"];
        assert!(!t.rated);
        assert_eq!(t.procs.iter().map(|p| (p.pid, p.usage, p.mem)).collect::<Vec<_>>(), [(10, None, Some(1 << 20)), (20, None, None)]);
        let second = [(10, client(1, 200_000_000, Some(1 << 20))), (10, client(2, 100_000_000, None)), (20, client(3, 500_000_000, None))];
        let t = &tally(&mut prev, t0 + Duration::from_secs(1), &second)["0000:00:02.0"];
        assert!(t.rated);
        assert_eq!(t.engines.get("Render"), Some(&80.0));
        assert_eq!(t.engines.get("Copy"), Some(&0.0));
        let procs: Vec<_> = t.procs.iter().map(|p| (p.pid, p.usage.map(f32::round))).collect();
        assert_eq!(procs, [(20, Some(50.0)), (10, Some(30.0))]);
        // A client that went away is forgotten.
        tally(&mut prev, t0 + Duration::from_secs(2), &[]);
        assert!(prev.0.is_empty());
    }

    #[test]
    fn a_tally_fills_what_sysfs_lacked() {
        let mut d = Device {
            source: "sysfs",
            bus: Some("0000:00:02.0".into()),
            driver: Some("i915".into()),
            why: vec![(Field::Usage, Why::NoBusy), (Field::Temp, Why::NoSensor)],
            ..Device::default()
        };
        assert_eq!(device_key(&d).as_deref(), Some("0000:00:02.0"));
        let t = Tally {
            engines: BTreeMap::from([("Render".into(), 40.0), ("Copy".into(), 5.0)]),
            rated: true,
            procs: vec![GpuProc { pid: 1, usage: Some(40.0), mem: None }],
        };
        apply(&mut d, &t);
        assert_eq!((d.usage, d.source, d.procs.len()), (Some(40.0), "sysfs+fdinfo", 1));
        assert_eq!(d.engines, vec![("Copy".to_string(), 5.0), ("Render".to_string(), 40.0)]);
        assert_eq!(d.why, vec![(Field::Temp, Why::NoSensor)]);
        // amdgpu's own busy percent is kept.
        let mut amd = Device { usage: Some(12.0), source: "sysfs", ..Device::default() };
        apply(&mut amd, &t);
        assert_eq!((amd.usage, amd.engines.len()), (Some(12.0), 0));
        let arm = Device { driver: Some("panfrost".into()), ..Device::default() };
        assert_eq!(device_key(&arm).as_deref(), Some("driver:panfrost"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn drm_fds_are_found_and_read() {
        let root = crate::util::test_dir("gpu-fdinfo");
        std::fs::create_dir_all(root.join("42/fd")).unwrap();
        std::fs::create_dir_all(root.join("42/fdinfo")).unwrap();
        std::os::unix::fs::symlink("/dev/dri/renderD128", root.join("42/fd/5")).unwrap();
        std::os::unix::fs::symlink("/dev/null", root.join("42/fd/0")).unwrap();
        std::fs::write(root.join("42/fdinfo/5"), I915).unwrap();
        std::fs::write(root.join("42/fdinfo/0"), "pos:\t0\n").unwrap();
        std::fs::create_dir_all(root.join("self")).unwrap();
        let mut scan = Scan::default();
        let t0 = Instant::now();
        let found = scan.read(&root, t0);
        assert_eq!(found.iter().map(|(pid, c)| (*pid, c.id)).collect::<Vec<_>>(), [(42, 7)]);
        // Between rescans a closed fd is dropped.
        std::fs::remove_file(root.join("42/fdinfo/5")).unwrap();
        assert!(scan.read(&root, t0 + Duration::from_secs(1)).is_empty());
        assert!(scan.fds.is_empty());
    }
}
