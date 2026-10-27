//! The sampler: one thread that reads the machine through sysinfo on its own
//! schedule (CPU, memory, GPUs, network interfaces, disks, sensors and the battery every interval, processes
//! every other or as [`Ctl::Procs`] says, services every minute for the warning count) and sends
//! each reading to the UI, waking it. The UI steers it with [`Ctl`]: the
//! interval, pause, whether services, sockets or devices are wanted. Dropping the
//! receiver ends the thread. The hardware (`hw`) is read once on a thread of
//! its own.

// The samplers are built with `new` on the thread that owns them; a `Default`
// would invite building them anywhere.
#![allow(clippy::new_without_default)]

/// What the sampler calls after every message so a window can redraw (the
/// agent has no window and passes `None`).
pub type Wake = Option<std::sync::Arc<dyn Fn() + Send + Sync>>;

/// The words a reading shows: a hook for the UI's language table. Without
/// one, the key itself.
#[cfg(not(test))]
static TR: std::sync::OnceLock<fn(&str) -> String> = std::sync::OnceLock::new();

/// Let the UI's translation table word the labels the collectors make.
pub fn set_tr(f: fn(&str) -> String) {
    #[cfg(not(test))]
    let _ = TR.set(f);
    #[cfg(test)]
    let _ = f;
}

#[cfg(test)]
pub(crate) fn tr(key: &str) -> String {
    i18n::tr(key).to_string()
}

#[cfg(not(test))]
pub(crate) fn tr(key: &str) -> String {
    TR.get().map_or_else(|| key.to_string(), |f| f(key))
}

/// The English table, so the tests can check that every label has a word.
#[cfg(test)]
pub(crate) mod i18n {
    use std::sync::OnceLock;

    static EN: OnceLock<ito_i18n::Table> = OnceLock::new();

    pub fn tr(key: &str) -> &str {
        EN.get_or_init(|| ito_i18n::Table::new(&[("en", include_str!("../../../src/lang/en.toml"))], "en").0).tr(key)
    }
}

pub mod battery;
pub mod containers;
pub mod devices;
pub mod disk;
pub mod gpu;
pub mod hw;
pub mod net;
#[cfg(windows)]
mod pdh;
pub mod proxmox;
pub mod sensors;
pub mod session;
pub mod svc;
pub mod topo;
pub mod util;
pub mod wire;
#[cfg(windows)]
mod wmi;
#[cfg(windows)]
mod wts;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};
use serde::{Deserialize, Serialize};
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind, Users};

use topo::Topology;

/// A name the sampler knows at compile time (a source, a vendor, a bus).
/// An alias, not `&'static str` written out: serde would then make every
/// type holding one readable only from `'static` bytes. A viewer reads them
/// back through `remote::wire::name`.
pub type Name = &'static str;

/// What does not change while running, read once at startup.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Static {
    pub host: String,
    pub os: String,
    pub kernel: String,
    pub arch: String,
    pub brand: String,
    /// Seconds since the epoch.
    pub boot_time: u64,
    pub topo: Topology,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Mem {
    pub total: u64,
    pub used: u64,
    pub available: u64,
    pub swap_total: u64,
    pub swap_used: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Proc {
    pub pid: u32,
    pub ppid: Option<u32>,
    pub name: String,
    /// Percent of one logical CPU, top's way: a process busy on two
    /// threads reads 200.
    pub cpu: f32,
    /// Resident bytes.
    pub mem: u64,
    /// `None` where the OS does not say cheaply (macOS for now).
    pub threads: Option<u32>,
    pub user: Option<String>,
    pub cmd: String,
    pub exe: Option<String>,
    /// Seconds since the epoch.
    pub start: u64,
    pub status: String,
}

/// Also what the fleet agent streams to a viewer that watches its machine:
/// variants are only ever added at the end.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Msg {
    Static(Box<Static>),
    /// Per logical CPU in sysinfo's order: usage in percent, clock in MHz.
    /// `load` is the 1, 5 and 15 minute load average where the OS keeps one
    /// (not Windows).
    Cpu {
        usage: Vec<f32>,
        freq_mhz: Vec<u64>,
        total: f32,
        load: Option<(f64, f64, f64)>,
    },
    Mem(Mem),
    Procs {
        procs: Vec<Proc>,
        total_threads: Option<u64>,
    },
    /// GPUs first, then NPUs.
    Gpu(Vec<gpu::Device>),
    /// Every [`SVC_EVERY`] while the svc screen wants them, every
    /// [`SVC_IDLE`] otherwise (the failed ones are counted on the fleet card).
    Svc(svc::Services),
    /// Every interval, so each interface has its history.
    Nics(Vec<net::Nic>),
    /// Every interval, so each drive and volume has its history.
    Disks(disk::Disks),
    /// Every interval, so each sensor has its history.
    Sensors(sensors::Sensors),
    /// Only while the net screen wants them.
    Conns(net::Conns),
    /// Once, from its own thread.
    Hardware(Box<hw::Hardware>),
    /// Every interval, before the sensors; `None` without a battery.
    Battery(Option<battery::Battery>),
    /// Every [`DEV_EVERY`] while the devices screen wants them, every
    /// [`DEV_IDLE`] otherwise (the problem ones are counted on the fleet card).
    Devices(Box<devices::Devices>),
    /// Every [`CT_EVERY`] while the containers screen wants them, every
    /// [`CT_IDLE`] otherwise (the stopped ones are counted on the fleet card).
    Containers(Box<containers::Containers>),
    /// Every [`session::EVERY`]: the logins and the power plan.
    Session(session::Session),
}

#[derive(Debug)]
pub enum Ctl {
    Interval(Duration),
    Pause(bool),
    /// Read services at once and then every [`SVC_EVERY`], or go back to
    /// every [`SVC_IDLE`].
    Services(bool),
    /// Read sockets (at once, then every [`NET_EVERY`]) or stop reading them.
    Net(bool),
    /// Read devices at once and then every [`DEV_EVERY`], or go back to every
    /// [`DEV_IDLE`].
    Devices(bool),
    /// Read containers at once and then every [`CT_EVERY`], or go back to
    /// every [`CT_IDLE`].
    Containers(bool),
    /// Sample processes every this many rounds (2 in the window); 0 stops:
    /// the fleet agent reads them only while a viewer watches.
    Procs(u32),
}

/// How often services are read while wanted: listing them spawns `systemctl`
/// or opens every service, so not at the sampling interval.
pub const SVC_EVERY: Duration = Duration::from_secs(5);

/// How often services are read while no screen shows them, for the count of
/// failed ones on the fleet card.
pub const SVC_IDLE: Duration = Duration::from_secs(60);

/// The first idle read waits this long, so startup reads only what is drawn.
const SVC_FIRST: Duration = Duration::from_secs(10);

/// The gap until the next services read.
fn svc_gap(wanted: bool) -> Duration {
    if wanted { SVC_EVERY } else { SVC_IDLE }
}

/// How often sockets are read while wanted: on Linux that walks every
/// process's open files.
pub const NET_EVERY: Duration = Duration::from_secs(2);

/// How often devices are read while wanted: a plugged device shows within
/// this, and walking every device each interval is more than it needs.
pub const DEV_EVERY: Duration = Duration::from_secs(5);

/// How often containers are read while wanted: each running one's stats
/// are a request of its own to the engine.
pub const CT_EVERY: Duration = Duration::from_secs(3);

/// How often devices and containers are read while no screen shows them, for
/// the counts on the fleet card.
pub const DEV_IDLE: Duration = Duration::from_secs(60);
pub const CT_IDLE: Duration = Duration::from_secs(60);

/// The first idle reads wait this long (staggered after [`SVC_FIRST`]).
const DEV_FIRST: Duration = Duration::from_secs(12);
const CT_FIRST: Duration = Duration::from_secs(14);

/// The gap until the next devices read.
fn dev_gap(wanted: bool) -> Duration {
    if wanted { DEV_EVERY } else { DEV_IDLE }
}

/// The gap until the next containers read.
fn ct_gap(wanted: bool) -> Duration {
    if wanted { CT_EVERY } else { CT_IDLE }
}

/// Start the sampler; `wake` is repainted after every message (the agent has
/// no window and passes `None`).
pub fn spawn(wake: crate::Wake, interval: Duration) -> (Receiver<Msg>, Sender<Ctl>) {
    let (tx, rx) = unbounded();
    let (ctl_tx, ctl_rx) = unbounded();
    hw::spawn(tx.clone(), wake.clone());
    std::thread::Builder::new()
        .name("yagura-sampler".into())
        .spawn(move || run(&tx, &ctl_rx, &wake, interval))
        .expect("spawn the sampler thread");
    (rx, ctl_tx)
}

fn run(tx: &Sender<Msg>, ctl: &Receiver<Ctl>, wake: &crate::Wake, mut interval: Duration) {
    let send = |m: Msg| {
        let ok = tx.send(m).is_ok();
        if let Some(w) = wake {
            w();
        }
        ok
    };
    let cpu_kind = CpuRefreshKind::nothing().with_cpu_usage().with_frequency();
    let mem_kind = MemoryRefreshKind::nothing().with_ram().with_swap();
    let mut sys = System::new();
    sys.refresh_cpu_specifics(cpu_kind);
    let logical = sys.cpus().len();
    let st = Static {
        host: System::host_name().unwrap_or_default(),
        os: System::long_os_version().unwrap_or_default(),
        kernel: System::kernel_version().unwrap_or_default(),
        arch: System::cpu_arch(),
        brand: sys.cpus().first().map(|c| c.brand().trim().to_string()).unwrap_or_default(),
        boot_time: System::boot_time(),
        topo: topo::read(logical),
    };
    if !send(Msg::Static(Box::new(st))) {
        return;
    }
    let mut procs = ProcSampler::new();
    // Opened here: the counter handles stay on this thread.
    let mut gpus = gpu::Sampler::new();
    let mut services = svc::Reader::default();
    // Services are always due at some point: wanted, or idle for the count.
    let mut svc_on = false;
    let mut svc_next = Instant::now() + SVC_FIRST;
    let mut nics = net::NicReader::new();
    let mut disks = disk::Reader::new();
    let mut sensors = sensors::Reader::new();
    let mut net_next: Option<Instant> = None;
    let mut devs = devices::Reader::default();
    let mut dev_on = false;
    let mut dev_next = Instant::now() + DEV_FIRST;
    let mut cts = containers::Reader::default();
    let mut ct_on = false;
    let mut ct_next = Instant::now() + CT_FIRST;
    let mut session_next = Instant::now();
    let mut paused = false;
    let mut procs_every: u32 = 2;
    // The first CPU reading needs a gap after the first refresh.
    let mut next = Instant::now() + interval.max(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    let mut tick: u64 = 0;
    loop {
        let due = if paused {
            None
        } else {
            [Some(next), Some(svc_next), Some(session_next), net_next, Some(dev_next), Some(ct_next)].into_iter().flatten().min()
        };
        let got = match due {
            Some(t) => ctl.recv_timeout(t.saturating_duration_since(Instant::now())),
            None => ctl.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match got {
            Ok(Ctl::Interval(d)) => {
                interval = d;
                next = Instant::now() + interval;
                continue;
            }
            Ok(Ctl::Pause(p)) => {
                paused = p;
                next = Instant::now() + interval;
                continue;
            }
            Ok(Ctl::Services(on)) => {
                // Read at once, paused or not: the screen has nothing yet.
                if on && !send(Msg::Svc(services.read())) {
                    return;
                }
                svc_on = on;
                svc_next = Instant::now() + svc_gap(on);
                continue;
            }
            Ok(Ctl::Net(on)) => {
                if on && !send(Msg::Conns(net::read_conns())) {
                    return;
                }
                net_next = on.then(|| Instant::now() + NET_EVERY);
                continue;
            }
            Ok(Ctl::Devices(on)) => {
                if on && !send(Msg::Devices(Box::new(devs.read()))) {
                    return;
                }
                dev_on = on;
                dev_next = Instant::now() + dev_gap(on);
                continue;
            }
            Ok(Ctl::Containers(on)) => {
                if on && !send(Msg::Containers(Box::new(cts.read()))) {
                    return;
                }
                ct_on = on;
                ct_next = Instant::now() + ct_gap(on);
                continue;
            }
            Ok(Ctl::Procs(n)) => {
                procs_every = n;
                // A new watcher gets them on the next round.
                tick = 0;
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() >= svc_next {
            if !send(Msg::Svc(services.read())) {
                return;
            }
            svc_next = Instant::now() + svc_gap(svc_on);
        }
        if let Some(t) = net_next
            && Instant::now() >= t
        {
            if !send(Msg::Conns(net::read_conns())) {
                return;
            }
            net_next = Some(Instant::now() + NET_EVERY);
        }
        if Instant::now() >= dev_next {
            if !send(Msg::Devices(Box::new(devs.read()))) {
                return;
            }
            dev_next = Instant::now() + dev_gap(dev_on);
        }
        if Instant::now() >= ct_next {
            if !send(Msg::Containers(Box::new(cts.read()))) {
                return;
            }
            ct_next = Instant::now() + ct_gap(ct_on);
        }
        if Instant::now() >= session_next {
            if !send(Msg::Session(session::read())) {
                return;
            }
            session_next = Instant::now() + session::EVERY;
        }
        if Instant::now() < next {
            continue;
        }
        sys.refresh_cpu_specifics(cpu_kind);
        sys.refresh_memory_specifics(mem_kind);
        let usage: Vec<f32> = sys.cpus().iter().map(|c| c.cpu_usage()).collect();
        let freq_mhz = sys.cpus().iter().map(|c| c.frequency()).collect();
        let total = if usage.is_empty() { 0.0 } else { usage.iter().sum::<f32>() / usage.len() as f32 };
        let mem = Mem {
            total: sys.total_memory(),
            used: sys.used_memory(),
            available: sys.available_memory(),
            swap_total: sys.total_swap(),
            swap_used: sys.used_swap(),
        };
        let load = load_average();
        if !send(Msg::Cpu { usage, freq_mhz, total, load }) || !send(Msg::Mem(mem)) {
            return;
        }
        let devs = gpus.sample();
        let disk = disks.read();
        let temps = sensors.read(&devs, &disk.drives);
        // Before the sensors: the agent's summary follows them.
        if !send(Msg::Gpu(devs))
            || !send(Msg::Nics(nics.read()))
            || !send(Msg::Disks(disk))
            || !send(Msg::Battery(battery::read()))
            || !send(Msg::Sensors(temps))
        {
            return;
        }
        if procs_every > 0 && tick.is_multiple_of(u64::from(procs_every)) {
            let (procs, total_threads) = procs.sample(&mut sys);
            if !send(Msg::Procs { procs, total_threads }) {
                return;
            }
        }
        tick += 1;
        next += interval;
        if next < Instant::now() {
            // Fell behind (the machine slept): start over from now.
            next = Instant::now() + interval;
        }
    }
}

struct ProcSampler {
    users: Users,
    users_at: Instant,
    names: HashMap<String, String>,
    #[cfg(windows)]
    wts: wts::Wts,
}

impl ProcSampler {
    fn new() -> Self {
        Self {
            users: Users::new_with_refreshed_list(),
            users_at: Instant::now(),
            names: HashMap::new(),
            #[cfg(windows)]
            wts: wts::Wts::default(),
        }
    }

    fn sample(&mut self, sys: &mut System) -> (Vec<Proc>, Option<u64>) {
        let kind = ProcessRefreshKind::nothing()
            .without_tasks()
            .with_cpu()
            .with_memory()
            .with_user(UpdateKind::OnlyIfNotSet)
            .with_cmd(UpdateKind::OnlyIfNotSet)
            .with_exe(UpdateKind::OnlyIfNotSet);
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        // New accounts are rare; the list is read again at most once a minute.
        if self.users_at.elapsed() > Duration::from_secs(60) {
            self.users.refresh();
            self.names.clear();
            #[cfg(windows)]
            self.wts.forget();
            self.users_at = Instant::now();
        }
        let threads = thread_counts();
        let mut out = Vec::with_capacity(sys.processes().len());
        for (pid, p) in sys.processes() {
            if p.thread_kind().is_some() {
                continue;
            }
            let pid = pid.as_u32();
            let user = p.user_id().and_then(|uid| {
                let key = uid.to_string();
                if let Some(n) = self.names.get(&key) {
                    return Some(n.clone());
                }
                let name = self.users.get_user_by_id(uid)?.name().to_string();
                self.names.insert(key, name.clone());
                Some(name)
            });
            let cmd = p.cmd().iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" ");
            out.push(Proc {
                pid,
                ppid: p.parent().map(|x| x.as_u32()),
                name: p.name().to_string_lossy().into_owned(),
                cpu: p.cpu_usage(),
                mem: p.memory(),
                threads: threads.as_ref().and_then(|t| t.get(&pid).copied()),
                user,
                cmd,
                exe: p.exe().map(|e| e.display().to_string()),
                start: p.start_time(),
                status: p.status().to_string(),
            });
        }
        // Not elevated, sysinfo cannot open other users' processes: no owner,
        // no memory. WTS has both without opening them.
        #[cfg(windows)]
        if out.iter().any(|p| p.user.is_none() || p.mem == 0) {
            fill(&mut out, &self.wts.read());
        }
        let total = threads.map(|t| t.values().map(|&n| n as u64).sum());
        (out, total)
    }
}

/// What WTS lists of a process (`wts.rs`).
#[cfg(any(windows, test))]
struct Seen {
    name: String,
    user: Option<String>,
    /// The working set, in WTS's unit (see [`wts_unit`]).
    mem: u64,
}

/// The owner and memory sysinfo could not read, from WTS. A pid only counts
/// where the name matches too: the two lists are read moments apart.
#[cfg(any(windows, test))]
fn fill(out: &mut [Proc], seen: &HashMap<u32, Seen>) {
    let same = |p: &Proc| seen.get(&p.pid).filter(|s| s.name.eq_ignore_ascii_case(&p.name));
    let unit = wts_unit(out.iter().filter_map(|p| Some((same(p)?.mem, p.mem))));
    for p in out.iter_mut() {
        let Some(s) = same(p) else { continue };
        if p.user.is_none() {
            p.user = s.user.clone();
        }
        if p.mem == 0 {
            p.mem = s.mem * unit;
        }
    }
}

/// The working set is documented in bytes but is said to come in KiB on
/// some versions: learnt from the processes both lists read, by the middle
/// ratio. Bytes when none can tell.
#[cfg(any(windows, test))]
fn wts_unit(pairs: impl Iterator<Item = (u64, u64)>) -> u64 {
    let mut ratios: Vec<f64> = pairs.filter(|&(w, s)| w > 0 && s > 0).map(|(w, s)| s as f64 / w as f64).collect();
    if ratios.is_empty() {
        return 1;
    }
    ratios.sort_by(f64::total_cmp);
    if ratios[ratios.len() / 2] > 32.0 { 1024 } else { 1 }
}

#[cfg(not(windows))]
fn load_average() -> Option<(f64, f64, f64)> {
    let l = System::load_average();
    Some((l.one, l.five, l.fifteen))
}

/// Windows keeps no load average.
#[cfg(windows)]
fn load_average() -> Option<(f64, f64, f64)> {
    None
}

/// Threads per process, from `/proc/<pid>/stat`: sysinfo only counts them
/// by listing every task, which costs far more.
#[cfg(target_os = "linux")]
fn thread_counts() -> Option<HashMap<u32, u32>> {
    let mut out = HashMap::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        if let Some(n) = std::fs::read_to_string(entry.path().join("stat")).ok().and_then(|s| stat_threads(&s)) {
            out.insert(pid, n);
        }
    }
    Some(out)
}

/// Field 20 of a stat line. The name in parentheses may hold spaces and
/// parentheses itself, so fields are counted from its last `)`.
#[cfg(any(target_os = "linux", test))]
fn stat_threads(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(17)?.parse().ok()
}

/// Threads per process from one Toolhelp snapshot.
#[cfg(windows)]
fn thread_counts() -> Option<HashMap<u32, u32>> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };

    // SAFETY: a snapshot of every process, closed below.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?;
    let mut out = HashMap::new();
    let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    // SAFETY: `e` is a PROCESSENTRY32W with its size set, as the calls ask.
    let mut ok = unsafe { Process32FirstW(snap, &mut e) }.is_ok();
    while ok {
        out.insert(e.th32ProcessID, e.cntThreads);
        // SAFETY: as above.
        ok = unsafe { Process32NextW(snap, &mut e) }.is_ok();
    }
    // SAFETY: the handle from CreateToolhelp32Snapshot, closed once.
    let _ = unsafe { CloseHandle(snap) };
    Some(out)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn thread_counts() -> Option<HashMap<u32, u32>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wts_fills_the_owner_and_memory_sysinfo_could_not_read() {
        let p =
            |pid, name: &str, user: Option<&str>, mem| Proc { pid, name: name.into(), user: user.map(Into::into), mem, ..Proc::default() };
        let seen = |name: &str, user: Option<&str>, mem| Seen { name: name.into(), user: user.map(Into::into), mem };
        let mut out = vec![
            p(10, "yagura.exe", Some("me"), 40 << 20),
            p(20, "svchost.exe", None, 0),
            p(30, "lsass.exe", None, 0),
            p(40, "dwm.exe", None, 0),
            p(50, "Idle", None, 0),
        ];
        let wts: HashMap<u32, Seen> = [
            (10, seen("yagura.exe", Some("me"), 41 << 20)),
            (20, seen("svchost.exe", Some("LOCAL SERVICE"), 8 << 20)),
            (30, seen("LSASS.EXE", Some("SYSTEM"), 20 << 20)),
            (40, seen("notepad.exe", Some("me"), 5 << 20)), // the pid came round again
            (50, seen("Idle", None, 0)),
        ]
        .into();
        fill(&mut out, &wts);
        let got: Vec<(Option<&str>, u64)> = out.iter().map(|p| (p.user.as_deref(), p.mem)).collect();
        assert_eq!(got, [(Some("me"), 40 << 20), (Some("LOCAL SERVICE"), 8 << 20), (Some("SYSTEM"), 20 << 20), (None, 0), (None, 0)]);
    }

    #[test]
    fn the_wts_unit_is_learnt_from_processes_both_read() {
        assert_eq!(wts_unit([(41 << 20, 40 << 20), (10 << 20, 11 << 20)].into_iter()), 1);
        assert_eq!(wts_unit([(41 << 10, 40 << 20), (10 << 10, 11 << 20), (0, 5)].into_iter()), 1024);
        assert_eq!(wts_unit(std::iter::empty()), 1);
        let mut out =
            vec![Proc { pid: 1, name: "a".into(), mem: 4 << 20, ..Proc::default() }, Proc { pid: 2, name: "b".into(), ..Proc::default() }];
        let seen: HashMap<u32, Seen> =
            [(1, Seen { name: "a".into(), user: None, mem: 4 << 10 }), (2, Seen { name: "b".into(), user: None, mem: 3 << 10 })].into();
        fill(&mut out, &seen);
        assert_eq!(out[1].mem, 3 << 20);
    }

    #[test]
    fn threads_come_from_field_20_past_the_name() {
        let stat = "1234 (a (weird) name) S 1 1234 1234 0 -1 4194560 100 0 0 0 5 3 0 0 20 0 7 0 12345 1000 50";
        assert_eq!(stat_threads(stat), Some(7));
        assert_eq!(stat_threads("garbage"), None);
    }

    #[test]
    fn services_are_read_often_when_shown_and_each_minute_otherwise() {
        assert_eq!(svc_gap(true), SVC_EVERY);
        assert_eq!(svc_gap(false), SVC_IDLE);
        assert!(SVC_FIRST < SVC_IDLE && SVC_EVERY < SVC_IDLE);
        assert_eq!((dev_gap(true), dev_gap(false)), (DEV_EVERY, DEV_IDLE));
        assert_eq!((ct_gap(true), ct_gap(false)), (CT_EVERY, CT_IDLE));
        assert!(DEV_FIRST < DEV_IDLE && CT_FIRST < CT_IDLE);
    }
}
