//! What the screen shows, and the arithmetic behind it. No terminal here, so
//! the tests need none.

use std::collections::HashMap;
use std::time::Instant;

use yagura_core::proxmox::{Cluster, Guest, Inside};
use yagura_core::{Mem, Proc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Guests,
    Procs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    Id,
    Cpu,
    Mem,
    Swap,
}

impl Sort {
    pub fn next(self) -> Self {
        match self {
            Sort::Id => Sort::Cpu,
            Sort::Cpu => Sort::Mem,
            Sort::Mem => Sort::Swap,
            Sort::Swap => Sort::Id,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Sort::Id => "id",
            Sort::Cpu => "cpu",
            Sort::Mem => "mem",
            Sort::Swap => "swap",
        }
    }
}

/// Bytes per second of the two directions of a guest's network.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rate {
    pub rx: f64,
    pub tx: f64,
}

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
        let mut v: Vec<&Guest> = self.cluster.guests.iter().collect();
        let swap = |g: &Guest| g.swap.and_then(|s| s.fraction()).unwrap_or(-1.0);
        v.sort_by(|a, b| {
            a.template.cmp(&b.template).then_with(|| match sort {
                Sort::Id => a.id.cmp(&b.id),
                Sort::Cpu => b.cpu.total_cmp(&a.cpu),
                Sort::Mem => b.mem.cmp(&a.mem),
                Sort::Swap => swap(b).total_cmp(&swap(a)),
            })
        });
        v
    }

    /// The processes, busiest first.
    pub fn procs_sorted(&self) -> Vec<&Proc> {
        let mut v: Vec<&Proc> = self.procs.iter().collect();
        v.sort_by(|a, b| b.cpu.total_cmp(&a.cpu).then(b.mem.cmp(&a.mem)));
        v
    }
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
