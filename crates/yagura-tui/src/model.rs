//! What the screen shows, and the arithmetic behind it. No terminal here, so
//! the tests need none.

use std::collections::{HashMap, VecDeque};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use yagura_core::proxmox::rrd::Point;
use yagura_core::proxmox::{Cluster, Guest, History, Inside, Status, Task, apps};
use yagura_core::{Mem, Proc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    /// The Proxmox host's graphs; where the screen starts.
    Host,
    Guests,
    Procs,
    /// One guest's graphs.
    Graph(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    Id,
    Cpu,
    Mem,
    Swap,
    /// Guests with something to do first (swap full, disk full, reboot wanted, updates waiting).
    Problems,
}

impl Sort {
    pub fn next(self) -> Self {
        match self {
            Sort::Id => Sort::Cpu,
            Sort::Cpu => Sort::Mem,
            Sort::Mem => Sort::Swap,
            Sort::Swap => Sort::Problems,
            Sort::Problems => Sort::Id,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Sort::Id => "id",
            Sort::Cpu => "cpu",
            Sort::Mem => "mem",
            Sort::Swap => "swap",
            Sort::Problems => "problems",
        }
    }
}

/// The root disk is called full from here (percent; `disk_full` in the settings).
pub fn disk_full() -> f64 {
    crate::config::get().disk_full
}

/// A failed task counts as a problem for this long (seconds).
pub const FAILED_WINDOW: u64 = 24 * 3600;

/// A stopped guest that was not started for this long is called stale (`stale_days` in the settings), in seconds.
pub fn stale_limit() -> u64 {
    crate::config::get().stale_days * 86400
}

/// Now, in Unix seconds.
pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// How long a guest's task log is trusted before it is read again.
pub const HIST_EVERY: std::time::Duration = std::time::Duration::from_secs(300);

/// Bytes per second of the two directions of a guest's network.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rate {
    pub rx: f64,
    pub tx: f64,
}

/// Seconds of GPU history kept.
const GPU_KEEP: usize = 300;

#[derive(Default)]
pub struct Model {
    pub host: String,
    pub cpu: f32,
    pub load: Option<(f64, f64, f64)>,
    pub mem: Mem,
    pub procs: Vec<Proc>,
    pub cluster: Cluster,
    pub rates: HashMap<u32, Rate>,
    pub inside: HashMap<u32, Inside>,
    /// Guests being looked into right now.
    pub looking: Vec<u32>,
    /// Latest release per app (normalised name); `None` once asked and not answered.
    pub latest: HashMap<String, Option<String>>,
    /// The busiest GPU's usage, one sample a second (Proxmox keeps none).
    pub gpu: VecDeque<f32>,
    /// The last hour of the node and of the guests whose graphs were opened.
    pub host_rrd: Vec<Point>,
    pub guest_rrd: HashMap<u32, Vec<Point>>,
    /// What runs inside the guest being looked at; `None` when it can not be asked.
    pub guest_procs: HashMap<u32, Option<Vec<yagura_core::proxmox::GuestProc>>>,
    /// Each guest's task log and snapshots, and when they were read.
    pub hist: HashMap<u32, (History, Instant)>,
    /// Guests whose task log is being read now.
    pub hist_looking: Vec<u32>,
    /// The last lines of the journal of the entered guest; `None` when it can not be asked.
    pub guest_logs: HashMap<u32, Option<Vec<String>>>,
    /// Only the guests whose name or id contains this (case-blind); empty shows all.
    pub filter: String,
    last: HashMap<u32, (u64, u64, Instant)>,
}

impl Model {
    /// Take a new `pvesh` listing and work out each guest's network rate from
    /// the previous one (the counters restart with the guest, so a smaller
    /// number is a restart, not a negative rate).
    pub fn set_cluster(&mut self, c: Cluster, now: Instant) {
        for g in &c.guests {
            if let Some((rx, tx, at)) = self.last.get(&g.id) {
                let dt = now.duration_since(*at).as_secs_f64();
                if dt > 0.0 {
                    self.rates
                        .insert(g.id, Rate { rx: g.netin.saturating_sub(*rx) as f64 / dt, tx: g.netout.saturating_sub(*tx) as f64 / dt });
                }
            }
            self.last.insert(g.id, (g.netin, g.netout, now));
        }
        let ids: Vec<u32> = c.guests.iter().map(|g| g.id).collect();
        self.last.retain(|id, _| ids.contains(id));
        self.rates.retain(|id, _| ids.contains(id));
        self.cluster = c;
    }

    /// The guests in the order `sort` asks (templates last), biggest first
    /// except by id.
    pub fn guests(&self, sort: Sort) -> Vec<&Guest> {
        let needle = self.filter.to_lowercase();
        let mut v: Vec<&Guest> = self
            .cluster
            .guests
            .iter()
            .filter(|g| needle.is_empty() || g.name.to_lowercase().contains(&needle) || g.id.to_string().contains(&needle))
            .collect();
        let swap = |g: &Guest| g.swap.and_then(|s| s.fraction()).unwrap_or(-1.0);
        v.sort_by(|a, b| {
            a.template.cmp(&b.template).then_with(|| match sort {
                Sort::Id => a.id.cmp(&b.id),
                Sort::Cpu => b.cpu.total_cmp(&a.cpu),
                Sort::Mem => b.mem.cmp(&a.mem),
                Sort::Swap => swap(b).total_cmp(&swap(a)),
                Sort::Problems => self.problems(b).cmp(&self.problems(a)).then(a.id.cmp(&b.id)),
            })
        });
        v
    }

    /// How much a guest wants attention: a full swap or disk counts double, a wanted reboot or waiting updates once
    /// (running guests); a stale stopped guest counts once, a task that failed in the last day twice. Templates are 0.
    pub fn problems(&self, g: &Guest) -> u32 {
        self.problems_at(g, unix_now())
    }

    pub fn problems_at(&self, g: &Guest, now: u64) -> u32 {
        if g.template {
            return 0;
        }
        let mut n = 0;
        if g.status == Status::Running {
            if g.swap.is_some_and(|s| s.near_limit()) {
                n += 2;
            }
            if disk_pct(g).is_some_and(|p| p >= disk_full()) {
                n += 2;
            }
            if let Some(i) = self.inside.get(&g.id) {
                n += u32::from(i.reboot_required) + u32::from(i.upgradable.unwrap_or(0) > 0);
            }
        }
        n + u32::from(self.stale_at(g, now).is_some()) + 2 * u32::from(self.failed_at(g, now).is_some())
    }

    /// How long a stopped guest has gone without a start, when that is over the limit and the task log knows a start.
    pub fn stale_at(&self, g: &Guest, now: u64) -> Option<u64> {
        if g.template || g.status != Status::Stopped {
            return None;
        }
        let idle = now.saturating_sub(self.hist.get(&g.id)?.0.last_start()?);
        (idle >= stale_limit()).then_some(idle)
    }

    /// The task that failed in the last day, if any.
    pub fn failed_at(&self, g: &Guest, now: u64) -> Option<&Task> {
        if g.template {
            return None;
        }
        self.hist.get(&g.id)?.0.failure_since(now.saturating_sub(FAILED_WINDOW))
    }

    /// What needs a look now, as `(key, text)`; a key stays the same while the cause does, so a new key is a new alert.
    pub fn alerts(&self, now: u64) -> Vec<(String, String)> {
        let cfg = crate::config::get();
        let mut out = vec![];
        for s in &self.cluster.storage {
            if let Some(p) = s.pct().filter(|p| *p >= cfg.storage_full) {
                out.push((format!("storage {}", s.name), format!("storage {} {p:.0}% full", s.name)));
            }
        }
        for (name, health) in self.cluster.bad_pools() {
            out.push((format!("pool {name}"), format!("pool {name} is {health}")));
        }
        if let Some(t) = self.cluster.temp_c.filter(|t| *t >= cfg.temp_hot) {
            out.push(("temp".into(), format!("host at {t:.0}C")));
        }
        for g in self.cluster.guests.iter().filter(|g| !g.template) {
            if let Some(t) = self.failed_at(g, now) {
                out.push((format!("failed {} {}", g.id, t.start), format!("{} {}: {} failed", g.id, g.name, t.kind)));
            }
            if self.stale_at(g, now).is_some() {
                out.push((format!("stale {}", g.id), format!("{} {} is stale", g.id, g.name)));
            }
        }
        out
    }

    /// `(guests, running, with updates waiting, wanting a reboot)` over the guests on show, templates left out.
    pub fn summary(&self) -> (usize, usize, usize, usize) {
        let list: Vec<&Guest> = self.guests(Sort::Id).into_iter().filter(|g| !g.template).collect();
        let running = list.iter().filter(|g| g.status == yagura_core::proxmox::Status::Running).count();
        let inside = |g: &&&Guest| self.inside.get(&g.id);
        let updates = list.iter().filter(|g| inside(g).is_some_and(|i| i.upgradable.unwrap_or(0) > 0)).count();
        let reboot = list.iter().filter(|g| inside(g).is_some_and(|i| i.reboot_required)).count();
        (list.len(), running, updates, reboot)
    }

    /// The app of a guest to show: the first one with a known repository, else the first one. `(name, installed, latest)`.
    pub fn app(&self, id: u32) -> Option<(&str, &str, Option<&str>)> {
        let apps = &self.inside.get(&id)?.apps;
        let (n, v) = apps.iter().find(|(n, _)| apps::repo(n).is_some()).or(apps.first())?;
        Some((n, v, self.latest.get(&apps::key(n)).and_then(|l| l.as_deref())))
    }

    /// The node whose graphs the host screen shows: the one named like this machine, else the first.
    pub fn host_node(&self) -> Option<&str> {
        let nodes = &self.cluster.nodes;
        nodes.iter().find(|n| n.name.eq_ignore_ascii_case(&self.host)).or(nodes.first()).map(|n| n.name.as_str())
    }

    /// Keep a GPU sample; the ring holds `GPU_KEEP` of them.
    pub fn push_gpu(&mut self, usage: Option<f32>) {
        let Some(u) = usage else { return };
        if self.gpu.len() == GPU_KEEP {
            self.gpu.pop_front();
        }
        self.gpu.push_back(u);
    }

    /// The processes, busiest first.
    pub fn procs_sorted(&self) -> Vec<&Proc> {
        let mut v: Vec<&Proc> = self.procs.iter().collect();
        v.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.mem.cmp(&a.mem)));
        v
    }
}

/// How full the root disk of a running guest is, in percent; `None` when Proxmox gives no size.
pub fn disk_pct(g: &Guest) -> Option<f64> {
    (g.status == yagura_core::proxmox::Status::Running && g.maxdisk > 0).then(|| g.disk as f64 * 100.0 / g.maxdisk as f64)
}

/// `1.5G`, `12M`, `300K`, `42B`: binary units, one decimal under 10.
pub fn bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 || v >= 10.0 { format!("{:.0}{}", v, U[i]) } else { format!("{:.1}{}", v, U[i]) }
}

/// `3d4h`, `5h12m`, `7m`, `9s`.
pub fn age(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{secs}s"),
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h{m}m"),
        (d, h, _) => format!("{d}d{h}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use yagura_core::proxmox::Swap;

    fn guest(id: u32, netin: u64) -> Guest {
        Guest { id, netin, ..Guest::default() }
    }

    #[test]
    fn units_and_ages_read_short() {
        assert_eq!(bytes(0), "0B");
        assert_eq!(bytes(1536), "1.5K");
        assert_eq!(bytes(300 * 1024 * 1024), "300M");
        assert_eq!(age(9), "9s");
        assert_eq!(age(7 * 60), "7m");
        assert_eq!(age(5 * 3600 + 12 * 60), "5h12m");
        assert_eq!(age(3 * 86400 + 4 * 3600), "3d4h");
    }

    #[test]
    fn rates_come_from_two_readings_and_a_restart_is_not_negative() {
        let mut m = Model::default();
        let t0 = Instant::now();
        m.set_cluster(Cluster { guests: vec![guest(1, 1000)], ..Cluster::default() }, t0);
        assert!(m.rates.is_empty());
        m.set_cluster(Cluster { guests: vec![guest(1, 3000)], ..Cluster::default() }, t0 + Duration::from_secs(2));
        assert_eq!(m.rates[&1].rx, 1000.0);
        m.set_cluster(Cluster { guests: vec![guest(1, 10)], ..Cluster::default() }, t0 + Duration::from_secs(4));
        assert_eq!(m.rates[&1].rx, 0.0);
        m.set_cluster(Cluster::default(), t0 + Duration::from_secs(6));
        assert!(m.rates.is_empty());
    }

    #[test]
    fn problems_sort_first_and_the_filter_and_summary_follow_what_is_shown() {
        use yagura_core::proxmox::Status;
        let mut m = Model::default();
        let mk =
            |id: u32, name: &str, disk| Guest { id, name: name.into(), status: Status::Running, disk, maxdisk: 100, ..Guest::default() };
        m.cluster.guests = vec![mk(1, "alpha", 10), mk(2, "beta", 95), mk(3, "gamma", 10)];
        m.inside.insert(3, Inside { upgradable: Some(4), reboot_required: true, ..Default::default() });
        let ids = |m: &Model| m.guests(Sort::Problems).iter().map(|g| g.id).collect::<Vec<_>>();
        // beta: full disk (2); gamma: updates + reboot (2); ties go by id.
        assert_eq!(ids(&m), [2, 3, 1]);
        assert_eq!(m.summary(), (3, 3, 1, 1));
        m.filter = "AM".into();
        assert_eq!(ids(&m), [3]);
        assert_eq!(m.summary(), (1, 1, 1, 1));
        m.filter = "2".into();
        assert_eq!(ids(&m), [2]);
    }

    #[test]
    fn a_stale_stopped_guest_and_a_failed_task_count_as_problems() {
        use yagura_core::proxmox::Task;
        let mut m = Model::default();
        let g = Guest { id: 5, status: Status::Stopped, ..Guest::default() };
        let t = |kind: &str, start, status: &str| Task { kind: kind.into(), start, end: Some(start + 1), status: status.into() };
        let now = 100 * 86400;
        assert_eq!(m.problems_at(&g, now), 0);
        let h = History { tasks: vec![t("vzstart", now - 30 * 86400, "OK")], snapshots: vec![], backups: vec![] };
        m.hist.insert(5, (h, Instant::now()));
        assert_eq!((m.problems_at(&g, now), m.stale_at(&g, now)), (1, Some(30 * 86400)));
        let h =
            History { tasks: vec![t("vzstart", now - 86400, "OK"), t("vzstart", now - 100, "boom")], snapshots: vec![], backups: vec![] };
        m.hist.insert(5, (h, Instant::now()));
        assert_eq!((m.problems_at(&g, now), m.stale_at(&g, now).is_some()), (2, false));
        let tpl = Guest { template: true, ..g.clone() };
        assert_eq!(m.problems_at(&tpl, now), 0);
    }

    #[test]
    fn guests_sort_by_what_is_asked_and_templates_go_last() {
        let mut m = Model::default();
        let mk = |id, cpu, used, template| Guest { id, cpu, template, swap: Some(Swap { used, max: Some(100) }), ..Guest::default() };
        m.cluster.guests = vec![mk(1, 0.1, 10, false), mk(2, 0.9, 95, false), mk(3, 0.5, 50, true), mk(4, 0.3, 20, false)];
        let ids = |s| m.guests(s).iter().map(|g| g.id).collect::<Vec<_>>();
        assert_eq!(ids(Sort::Id), [1, 2, 4, 3]);
        assert_eq!(ids(Sort::Cpu), [2, 4, 1, 3]);
        assert_eq!(ids(Sort::Swap), [2, 4, 1, 3]);
    }
}
