//! Proxmox VE: the guests of a node as `pvesh` lists them, their swap from
//! the host's cgroups, and what is inside a container (`pct exec`). Linux
//! only, and only on a Proxmox host as root; everywhere else the reader says
//! why there is nothing ([`Note`]). The parsing is pure, so every OS tests it.
//!
//! Nothing here runs on a UI thread: `pvesh` and `pct exec` take seconds.

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod apps;
pub mod rrd;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    #[default]
    Lxc,
    Qemu,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Running,
    #[default]
    Stopped,
    Other,
}

/// A guest's swap: what it has paged out and the limit it may reach.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Swap {
    pub used: u64,
    /// `None` is unlimited.
    pub max: Option<u64>,
}

impl Swap {
    /// Used over the limit, 0.0 to 1.0; `None` when unlimited.
    pub fn fraction(&self) -> Option<f32> {
        self.max.filter(|m| *m > 0).map(|m| (self.used as f32 / m as f32).min(1.0))
    }

    /// At or past 90% of its limit: shown in red.
    pub fn near_limit(&self) -> bool {
        self.fraction().is_some_and(|f| f >= 0.9)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Guest {
    pub id: u32,
    pub kind: Kind,
    pub name: String,
    pub node: String,
    pub status: Status,
    /// 0.0 to 1.0 of the guest's CPUs (`pvesh` gives a fraction, not percent).
    pub cpu: f32,
    pub cpus: u32,
    pub mem: u64,
    pub maxmem: u64,
    pub disk: u64,
    pub maxdisk: u64,
    /// Cumulative since the guest started: a rate needs two readings.
    pub netin: u64,
    pub netout: u64,
    pub diskread: u64,
    pub diskwrite: u64,
    pub uptime: u64,
    pub tags: Vec<String>,
    pub template: bool,
    pub swap: Option<Swap>,
}

/// The node itself, from the same listing.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub name: String,
    pub online: bool,
    pub cpu: f32,
    pub cpus: u32,
    pub mem: u64,
    pub maxmem: u64,
    pub uptime: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Cluster {
    pub nodes: Vec<Node>,
    pub guests: Vec<Guest>,
    pub note: Option<Note>,
    /// The storages the cluster lists (`local`, `local-lvm`, a mount…), with how full each is.
    pub storage: Vec<Storage>,
    /// This host runs an older kernel than the newest one installed, or says it wants a reboot.
    pub reboot_required: bool,
    /// ZFS pools of this host as `(name, health)`; empty when there is no `zpool`.
    #[serde(default)]
    pub pools: Vec<(String, String)>,
    /// The hottest thermal zone of this host, in degrees Celsius.
    #[serde(default)]
    pub temp_c: Option<f32>,
}

impl Cluster {
    /// Pools whose health is not `ONLINE`.
    pub fn bad_pools(&self) -> Vec<&(String, String)> {
        self.pools.iter().filter(|p| p.1 != "ONLINE").collect()
    }
}

/// `zpool list -H -o name,health` into `(name, health)`.
pub fn parse_zpool(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((f.next()?.to_string(), f.next()?.to_string()))
        })
        .collect()
}

/// The hottest of the thermal zones' millidegree readings, in degrees; implausible values are dropped.
pub fn max_temp(milli: &[i64]) -> Option<f32> {
    milli.iter().copied().filter(|m| (1..150_000).contains(m)).max().map(|m| m as f32 / 1000.0)
}

fn host_pools() -> Vec<(String, String)> {
    Command::new("zpool")
        .args(["list", "-H", "-o", "name,health"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_zpool(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

fn host_temp() -> Option<f32> {
    let zones = std::fs::read_dir("/sys/class/thermal").ok()?;
    let v: Vec<i64> = zones.filter_map(|e| std::fs::read_to_string(e.ok()?.path().join("temp")).ok()?.trim().parse().ok()).collect();
    max_temp(&v)
}

/// One storage of a node, from the same listing as the guests.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Storage {
    pub name: String,
    pub node: String,
    pub used: u64,
    pub total: u64,
    pub active: bool,
    /// The storage type (`dir`, `zfspool`, `lvmthin`, `nfs`…).
    pub kind: String,
    /// What it may hold (`images,rootdir,backup`…).
    pub content: String,
}

impl Storage {
    /// Percent used; `None` when it has no size (not mounted).
    pub fn pct(&self) -> Option<f64> {
        (self.active && self.total > 0).then(|| self.used as f64 * 100.0 / self.total as f64)
    }
}

/// The storages in a `/cluster/resources` listing.
pub fn parse_storage(json: &str) -> Vec<Storage> {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return vec![] };
    let u = |r: &Value, k: &str| r.get(k).and_then(Value::as_u64).unwrap_or(0);
    let s = |r: &Value, k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    v.as_array()
        .map(|l| {
            l.iter()
                .filter(|r| r.get("type").and_then(Value::as_str) == Some("storage"))
                .map(|r| Storage {
                    name: s(r, "storage"),
                    node: s(r, "node"),
                    used: u(r, "disk"),
                    total: u(r, "maxdisk"),
                    active: r.get("status").and_then(Value::as_str) == Some("available"),
                    kind: s(r, "plugintype"),
                    content: s(r, "content"),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Kernel versions as numbers, so `6.8.12-10-pve` is newer than `6.8.12-4-pve`.
fn kernel_key(v: &str) -> Vec<u64> {
    v.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).filter_map(|p| p.parse().ok()).collect()
}

/// Whether one of the installed kernels is newer than the running one (same flavour, such as `-pve`).
pub fn kernel_stale(running: &str, installed: &[String]) -> bool {
    let flavour = |v: &str| v.rsplit('-').next().filter(|f| f.chars().all(|c| c.is_ascii_alphabetic())).unwrap_or("").to_string();
    let (run, f) = (kernel_key(running), flavour(running));
    installed.iter().any(|i| flavour(i) == f && kernel_key(i) > run)
}

/// This host wants a reboot: `/run/reboot-required`, or a newer kernel in `/lib/modules` than the running one.
fn host_reboot_required() -> bool {
    if Path::new("/run/reboot-required").exists() {
        return true;
    }
    let Ok(running) = std::fs::read_to_string("/proc/sys/kernel/osrelease") else { return false };
    let installed: Vec<String> =
        std::fs::read_dir("/lib/modules").map(|d| d.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect()).unwrap_or_default();
    kernel_stale(running.trim(), &installed)
}

/// Why there is nothing to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Note {
    /// Not Linux.
    OtherOs,
    /// No `pvesh`: not a Proxmox host.
    NoPvesh,
    /// `pvesh` ran and failed (not root, or the cluster is down).
    Failed,
    /// Its output was not what the API documents.
    Unreadable,
}

/// `pvesh get /cluster/resources --output-format json` into nodes and guests.
/// Storage, pools and the SDN are skipped.
pub fn parse_resources(json: &str) -> Result<(Vec<Node>, Vec<Guest>), String> {
    let v: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let list = v.as_array().ok_or("not a list")?;
    let (mut nodes, mut guests) = (Vec::new(), Vec::new());
    let u = |r: &Value, k: &str| r.get(k).and_then(Value::as_u64).unwrap_or(0);
    let f = |r: &Value, k: &str| r.get(k).and_then(Value::as_f64).unwrap_or(0.0) as f32;
    let s = |r: &Value, k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    for r in list {
        match r.get("type").and_then(Value::as_str) {
            Some("node") => nodes.push(Node {
                name: s(r, "node"),
                online: r.get("status").and_then(Value::as_str) == Some("online"),
                cpu: f(r, "cpu"),
                cpus: u(r, "maxcpu") as u32,
                mem: u(r, "mem"),
                maxmem: u(r, "maxmem"),
                uptime: u(r, "uptime"),
            }),
            Some(t @ ("lxc" | "qemu")) => guests.push(Guest {
                id: u(r, "vmid") as u32,
                kind: if t == "lxc" { Kind::Lxc } else { Kind::Qemu },
                name: s(r, "name"),
                node: s(r, "node"),
                status: match r.get("status").and_then(Value::as_str) {
                    Some("running") => Status::Running,
                    Some("stopped") => Status::Stopped,
                    _ => Status::Other,
                },
                cpu: f(r, "cpu"),
                cpus: u(r, "maxcpu") as u32,
                mem: u(r, "mem"),
                maxmem: u(r, "maxmem"),
                disk: u(r, "disk"),
                maxdisk: u(r, "maxdisk"),
                netin: u(r, "netin"),
                netout: u(r, "netout"),
                diskread: u(r, "diskread"),
                diskwrite: u(r, "diskwrite"),
                uptime: u(r, "uptime"),
                tags: s(r, "tags").split(';').filter(|t| !t.is_empty()).map(str::to_string).collect(),
                template: u(r, "template") == 1,
                swap: None,
            }),
            _ => {}
        }
    }
    guests.sort_by_key(|g| g.id);
    Ok((nodes, guests))
}

/// `memory.swap.current` and `memory.swap.max` of a cgroup (`max` is
/// unlimited).
pub fn parse_swap(current: &str, max: &str) -> Option<Swap> {
    let used = current.trim().parse().ok()?;
    let max = match max.trim() {
        "max" => None,
        n => Some(n.parse().ok()?),
    };
    Some(Swap { used, max })
}

/// A container's swap from the host's cgroup tree under `root`
/// (`/sys/fs/cgroup`): `lxc/<id>`, or `lxc.payload.<id>` where LXC 5+ puts
/// the payload.
pub fn lxc_swap(root: &Path, id: u32) -> Option<Swap> {
    [format!("lxc/{id}"), format!("lxc.payload.{id}")].iter().find_map(|p| {
        let dir = root.join(p);
        let cur = std::fs::read_to_string(dir.join("memory.swap.current")).ok()?;
        let max = std::fs::read_to_string(dir.join("memory.swap.max")).unwrap_or_else(|_| "max".into());
        parse_swap(&cur, &max)
    })
}

/// `VmSwap` of a `/proc/<pid>/status`, in bytes (the file says kB).
pub fn parse_vmswap(status: &str) -> Option<u64> {
    let kb = status.lines().find_map(|l| l.strip_prefix("VmSwap:"))?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()?;
    Some(kb * 1024)
}

/// A VM's swap: the swapped-out part of its QEMU process, found through the
/// pid file `<run>/<id>.pid` (`/run/qemu-server`). QEMU has no swap limit of
/// its own, so `max` is unset.
pub fn vm_swap(run: &Path, proc_root: &Path, id: u32) -> Option<Swap> {
    let pid: u32 = std::fs::read_to_string(run.join(format!("{id}.pid"))).ok()?.trim().parse().ok()?;
    let status = std::fs::read_to_string(proc_root.join(pid.to_string()).join("status")).ok()?;
    Some(Swap { used: parse_vmswap(&status)?, max: None })
}

/// The whole cluster listing, with the guests' swap added. Runs `pvesh`,
/// so call it from a thread of its own.
pub fn read() -> Cluster {
    let none = |note| Cluster { note: Some(note), ..Cluster::default() };
    if !cfg!(target_os = "linux") {
        return none(Note::OtherOs);
    }
    let out = match Command::new("pvesh").args(["get", "/cluster/resources", "--output-format", "json"]).output() {
        Ok(o) if o.status.success() => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return none(Note::NoPvesh),
        _ => return none(Note::Failed),
    };
    let Ok((nodes, mut guests)) = parse_resources(&String::from_utf8_lossy(&out.stdout)) else {
        return none(Note::Unreadable);
    };
    for g in guests.iter_mut().filter(|g| g.status == Status::Running) {
        g.swap = match g.kind {
            Kind::Lxc => lxc_swap(Path::new("/sys/fs/cgroup"), g.id),
            Kind::Qemu => vm_swap(Path::new("/run/qemu-server"), Path::new("/proc"), g.id),
        };
    }
    let storage = parse_storage(&String::from_utf8_lossy(&out.stdout));
    Cluster { nodes, guests, note: None, storage, reboot_required: host_reboot_required(), pools: host_pools(), temp_c: host_temp() }
}

/// What is inside a guest: a container read with `pct exec`, a VM through its
/// QEMU guest agent (`qm guest exec`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Inside {
    /// `PRETTY_NAME` of `/etc/os-release`.
    pub os: Option<String>,
    pub packages: Option<u32>,
    /// Packages `apt` says can be upgraded, from the lists as they are.
    pub upgradable: Option<u32>,
    /// Packages `apt-mark hold` keeps back on purpose: not counted in `upgradable`.
    #[serde(default)]
    pub held: u32,
    /// Seconds since the apt lists were fetched: how far to trust `upgradable`.
    pub lists_age: Option<u64>,
    pub reboot_required: bool,
    /// Apps an installer left a version for: `(name, version)`.
    pub apps: Vec<(String, String)>,
    /// Names of the packages an upgrade would install (held ones left out).
    #[serde(default)]
    pub upgrades: Vec<String>,
    /// A VM whose guest agent does not answer: nothing could be read.
    pub no_agent: bool,
}

/// `PRETTY_NAME` from `/etc/os-release`.
pub fn parse_os_release(text: &str) -> Option<String> {
    text.lines().find_map(|l| l.strip_prefix("PRETTY_NAME=")).map(|v| v.trim().trim_matches('"').to_string())
}

/// How many packages `dpkg-query -W` lists (one per line).
pub fn count_packages(text: &str) -> u32 {
    text.lines().filter(|l| !l.trim().is_empty()).count() as u32
}

/// How many packages `apt list --upgradable` lists: its header line
/// (`Listing...`) and warnings are not packages.
pub fn count_upgradable(text: &str) -> u32 {
    text.lines().filter(|l| l.contains("[upgradable from:")).count() as u32
}

const HOLD: &str = "---hold---\n";

/// `apt list --upgradable` followed by `apt-mark showhold` after a [`HOLD`] line: `(upgradable, held)`. A held
/// package is counted only as held.
pub fn parse_upgradable(text: &str) -> (u32, u32) {
    let (list, holds) = text.split_once(HOLD).unwrap_or((text, ""));
    let holds: Vec<&str> = holds.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let (mut up, mut held) = (0, 0);
    for l in list.lines().filter(|l| l.contains("[upgradable from:")) {
        if holds.contains(&l.split('/').next().unwrap_or("")) {
            held += 1;
        } else {
            up += 1;
        }
    }
    (up, held)
}

/// Names of the packages `apt list --upgradable` lists, held ones left out.
pub fn parse_upgrade_names(text: &str) -> Vec<String> {
    let (list, holds) = text.split_once(HOLD).unwrap_or((text, ""));
    let holds: Vec<&str> = holds.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    list.lines()
        .filter(|l| l.contains("[upgradable from:"))
        .filter_map(|l| l.split('/').next())
        .filter(|n| !holds.contains(n))
        .map(str::to_string)
        .collect()
}

/// Whether upgrading this package needs care: a kernel, Proxmox itself, the init system, libc, a bootloader.
pub fn high_impact(name: &str) -> bool {
    const PREFIX: [&str; 8] = ["linux-image", "pve-", "proxmox-", "systemd", "libc6", "grub", "openssh-server", "zfs"];
    name == "pve-manager" || PREFIX.iter().any(|p| name.starts_with(p))
}

/// The first `E:` line apt printed, else the last line said.
fn apt_error(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.starts_with("E:")).or_else(|| text.lines().rev().find(|l| !l.trim().is_empty()))?;
    Some(line.trim().chars().take(80).collect())
}

/// The result of `qm guest exec`: its `out-data` when the command exited 0.
pub fn parse_guest_exec(json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json).ok()?;
    (v.get("exitcode").and_then(Value::as_i64) == Some(0)).then(|| v.get("out-data").and_then(Value::as_str).unwrap_or("").to_string())
}

/// `qm agent <id> get-osinfo`: the pretty name, and whether it is Windows.
pub fn parse_osinfo(json: &str) -> (Option<String>, bool) {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return (None, false) };
    let r = v.get("result").unwrap_or(&v);
    let name = r.get("pretty-name").or_else(|| r.get("name")).and_then(Value::as_str).map(str::to_string);
    (name, r.get("id").and_then(Value::as_str) == Some("mswindows"))
}

fn exec(kind: Kind, id: u32, args: &[&str]) -> Option<String> {
    match kind {
        Kind::Lxc => {
            let o = Command::new("pct").arg("exec").arg(id.to_string()).arg("--").args(args).output().ok()?;
            o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
        }
        Kind::Qemu => {
            let o =
                Command::new("qm").args(["guest", "exec"]).arg(id.to_string()).args(["--timeout", "20", "--"]).args(args).output().ok()?;
            if !o.status.success() {
                return None;
            }
            parse_guest_exec(&String::from_utf8_lossy(&o.stdout))
        }
    }
}

fn agent(id: u32, args: &[&str]) -> Option<String> {
    let o = Command::new("qm").arg("agent").arg(id.to_string()).args(args).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// What the keys can ask of a guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Power {
    Start,
    Shutdown,
    Reboot,
    Upgrade,
}

impl Power {
    pub fn verb(self) -> &'static str {
        match self {
            Power::Start => "start",
            Power::Shutdown => "shutdown",
            Power::Reboot => "reboot",
            Power::Upgrade => "upgrade",
        }
    }

    /// Whether a guest in `status` can be asked this at all.
    pub fn fits(self, status: Status) -> bool {
        match self {
            Power::Start => status == Status::Stopped,
            Power::Shutdown | Power::Reboot | Power::Upgrade => status == Status::Running,
        }
    }
}

/// The command for a power change: `pct <verb> <id>` or `qm <verb> <id>`.
pub fn power_command(kind: Kind, id: u32, p: Power) -> (&'static str, [String; 2]) {
    (if kind == Kind::Lxc { "pct" } else { "qm" }, [p.verb().to_string(), id.to_string()])
}

/// Run a power change and wait for it (shutdown can take a while: call it from
/// a thread of its own). The error is the first line of what the tool said.
pub fn power(kind: Kind, id: u32, p: Power) -> Result<(), String> {
    if matches!(p, Power::Upgrade) {
        return upgrade(kind, id);
    }
    let (prog, args) = power_command(kind, id, p);
    let o = Command::new(prog).args(args).output().map_err(|e| format!("{prog}: {e}"))?;
    if o.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&o.stderr);
    Err(err.lines().find(|l| !l.trim().is_empty()).unwrap_or("failed").trim().to_string())
}

/// One task from a node's task log.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Task {
    /// `vzstart`, `qmstart`, `vzshutdown`, `vzsnapshot`…
    pub kind: String,
    pub start: u64,
    /// `None` while it runs.
    pub end: Option<u64>,
    /// `OK`, or what went wrong; empty while it runs.
    pub status: String,
}

impl Task {
    pub fn failed(&self) -> bool {
        self.end.is_some() && !self.status.is_empty() && self.status != "OK" && !self.status.starts_with("WARNINGS")
    }
}

/// One start of a guest and how long it ran before it was shut down.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Run {
    pub start: u64,
    pub ran: Option<u64>,
}

/// The guest's task log (oldest first) and its snapshots' times: what shows when it last ran, what failed, what piles up.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct History {
    pub tasks: Vec<Task>,
    pub snapshots: Vec<u64>,
    /// Creation times of the guest's backups on the storages Proxmox lists.
    pub backups: Vec<u64>,
}

impl History {
    /// The newest successful starts, newest first, each with how long the guest stayed up (to the next shutdown or stop).
    pub fn runs(&self, n: usize) -> Vec<Run> {
        let stops = |t: &Task| matches!(t.kind.as_str(), "vzshutdown" | "vzstop" | "qmshutdown" | "qmstop") && t.status == "OK";
        self.tasks
            .iter()
            .rev()
            .filter(|t| matches!(t.kind.as_str(), "vzstart" | "qmstart") && t.status == "OK")
            .take(n)
            .map(|t| Run {
                start: t.start,
                ran: self.tasks.iter().find(|s| stops(s) && s.start >= t.start).map(|s| s.end.unwrap_or(s.start).saturating_sub(t.start)),
            })
            .collect()
    }

    pub fn last_start(&self) -> Option<u64> {
        self.runs(1).first().map(|r| r.start)
    }

    /// The newest task that failed at or after `since` (Unix seconds).
    pub fn failure_since(&self, since: u64) -> Option<&Task> {
        self.tasks.iter().rev().find(|t| t.failed() && t.end.unwrap_or(t.start) >= since)
    }

    /// When the newest backup was made: a backup file on a storage, or a vzdump task that finished well.
    pub fn last_backup(&self) -> Option<u64> {
        let task = self.tasks.iter().filter(|t| t.kind == "vzdump" && t.status == "OK").map(|t| t.end.unwrap_or(t.start)).max();
        self.backups.iter().copied().chain(task).max()
    }

    /// The newest vzdump task that failed at or after `since`.
    pub fn backup_failure_since(&self, since: u64) -> Option<&Task> {
        self.tasks.iter().rev().find(|t| t.kind == "vzdump" && t.failed() && t.end.unwrap_or(t.start) >= since)
    }

    pub fn oldest_snapshot(&self) -> Option<u64> {
        self.snapshots.iter().copied().min()
    }
}

/// A node's task list into tasks, oldest first.
pub fn parse_tasks(json: &str) -> Vec<Task> {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return vec![] };
    let mut t: Vec<Task> = v
        .as_array()
        .map(|l| {
            l.iter()
                .filter_map(|t| {
                    Some(Task {
                        kind: t["type"].as_str()?.to_string(),
                        start: t["starttime"].as_u64()?,
                        end: t["endtime"].as_u64(),
                        status: t["status"].as_str().unwrap_or("").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    t.sort_by_key(|t| t.start);
    t
}

/// One task of a node's task log, with the fields the task screen shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HostTask {
    pub node: String,
    pub upid: String,
    /// `vzdump`, `qmstart`, `aptupdate`…
    pub kind: String,
    /// The guest ID the task is about; empty for a job over several guests or for the host.
    pub id: String,
    pub user: String,
    pub start: u64,
    /// `None` while it runs.
    pub end: Option<u64>,
    /// `OK`, or what went wrong; empty while it runs.
    pub status: String,
}

impl HostTask {
    pub fn failed(&self) -> bool {
        self.end.is_some() && !self.status.is_empty() && self.status != "OK" && !self.status.starts_with("WARNINGS")
    }
}

/// A node's task list into host tasks, newest first.
pub fn parse_host_tasks(json: &str, node: &str) -> Vec<HostTask> {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return vec![] };
    let s = |t: &Value, k: &str| t[k].as_str().unwrap_or("").to_string();
    let mut out: Vec<HostTask> = v
        .as_array()
        .map(|l| {
            l.iter()
                .filter_map(|t| {
                    Some(HostTask {
                        node: node.to_string(),
                        upid: t["upid"].as_str()?.to_string(),
                        kind: t["type"].as_str()?.to_string(),
                        id: s(t, "id"),
                        user: s(t, "user"),
                        start: t["starttime"].as_u64()?,
                        end: t["endtime"].as_u64(),
                        status: s(t, "status"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by_key(|t| std::cmp::Reverse(t.start));
    out
}

/// The lines in a task log (`[{"n":1,"t":"text"}]`), the last `keep` of them.
pub fn parse_task_log(json: &str, keep: usize) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return vec![] };
    let mut lines: Vec<String> =
        v.as_array().map(|l| l.iter().filter_map(|x| x["t"].as_str().map(str::to_string)).collect()).unwrap_or_default();
    let skip = lines.len().saturating_sub(keep);
    lines.drain(..skip);
    lines
}

/// The snapshots' times in a snapshot list (the `current` entry has none).
pub fn parse_snapshots(json: &str) -> Vec<u64> {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return vec![] };
    v.as_array().map(|l| l.iter().filter_map(|s| s["snaptime"].as_u64()).collect()).unwrap_or_default()
}

/// The creation times of the backups in a storage content listing.
pub fn parse_backups(json: &str) -> Vec<u64> {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return vec![] };
    v.as_array().map(|l| l.iter().filter_map(|b| b["ctime"].as_u64()).collect()).unwrap_or_default()
}

fn pvesh(path: &str, extra: &[&str]) -> Option<String> {
    let o = Command::new("pvesh").args(["get", path]).args(extra).args(["--output-format", "json"]).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// The task log and snapshots of a guest (read-only `pvesh`). This is how a guest that is stopped after its weekly job
/// (a backup container) shows when it last ran, though Proxmox lists no backup for it. Slow: call it from a thread of its own.
pub fn history(node: &str, kind: Kind, id: u32, storages: &[String]) -> History {
    let tasks = pvesh(&format!("/nodes/{node}/tasks"), &["--vmid", &id.to_string(), "--limit", "50"]).map(|j| parse_tasks(&j));
    let what = if kind == Kind::Lxc { "lxc" } else { "qemu" };
    let snapshots = pvesh(&format!("/nodes/{node}/{what}/{id}/snapshot"), &[]).map(|j| parse_snapshots(&j));
    let backups = storages
        .iter()
        .filter_map(|st| pvesh(&format!("/nodes/{node}/storage/{st}/content"), &["--content", "backup", "--vmid", &id.to_string()]))
        .flat_map(|j| parse_backups(&j))
        .collect();
    History { tasks: tasks.unwrap_or_default(), snapshots: snapshots.unwrap_or_default(), backups }
}

/// The latest tasks of every node, newest first (read-only `pvesh`). Slow: call it from a thread of its own.
pub fn host_tasks(nodes: &[String]) -> Vec<HostTask> {
    let mut all: Vec<HostTask> = nodes
        .iter()
        .filter_map(|n| pvesh(&format!("/nodes/{n}/tasks"), &["--limit", "100"]).map(|j| parse_host_tasks(&j, n)))
        .flatten()
        .collect();
    all.sort_by_key(|t| std::cmp::Reverse(t.start));
    all.truncate(150);
    all
}

/// The log of one task; `None` when it can not be read. Slow: call it from a thread of its own.
pub fn task_log(node: &str, upid: &str) -> Option<Vec<String>> {
    pvesh(&format!("/nodes/{node}/tasks/{upid}/log"), &["--limit", "5000"]).map(|j| parse_task_log(&j, 500))
}

/// The last lines of a running guest's journal (or syslog); `None` when it can not be asked. Slow: call it from a thread of its own.
pub fn logs(kind: Kind, id: u32) -> Option<Vec<String>> {
    let script = "journalctl -n 200 --no-pager -o short 2>/dev/null || tail -n 200 /var/log/syslog /var/log/messages 2>/dev/null";
    let out = exec(kind, id, &["sh", "-c", script])?;
    let v: Vec<String> = out.lines().map(str::to_string).collect();
    (!v.is_empty()).then_some(v)
}

/// `apt-get update` and `apt-get upgrade` inside a guest, without questions, and wait for them (call it from a
/// thread of its own). The error is the last line the command said.
pub fn upgrade(kind: Kind, id: u32) -> Result<(), String> {
    // Mend a half-installed state first, and clear the downloads after: a 4 GB root fills up.
    const SCRIPT: &str = "export DEBIAN_FRONTEND=noninteractive; apt-get update -qq && apt-get -y -qq --fix-broken install && \
         apt-get -y -qq -o Dpkg::Options::=--force-confold upgrade; rc=$?; apt-get clean; exit $rc";
    let o = match kind {
        Kind::Lxc => Command::new("pct").arg("exec").arg(id.to_string()).args(["--", "sh", "-c", SCRIPT]).output(),
        Kind::Qemu => {
            Command::new("qm").args(["guest", "exec"]).arg(id.to_string()).args(["--timeout", "1800", "--", "sh", "-c", SCRIPT]).output()
        }
    }
    .map_err(|e| e.to_string())?;
    let out = String::from_utf8_lossy(&o.stdout);
    let ok = match kind {
        Kind::Lxc => o.status.success(),
        Kind::Qemu => o.status.success() && parse_guest_exec(&out).is_some(),
    };
    if ok {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&o.stderr);
    // An `E:` line says more than the last line (a dpkg trailer); a VM puts it in the JSON's `err-data`.
    let text = format!("{err}\n{out}").replace("\\n", "\n");
    Err(apt_error(&text).unwrap_or_else(|| "failed".into()))
}

/// Look inside a running guest. About two seconds per command, so call it
/// from a thread of its own. Never runs `apt update`: the update count is only
/// as fresh as the lists, and `lists_age` says how fresh that is. A VM needs
/// the guest agent; Windows answers with its name only.
pub fn inspect(kind: Kind, id: u32) -> Inside {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let os = match kind {
        Kind::Lxc => exec(kind, id, &["cat", "/etc/os-release"]).and_then(|t| parse_os_release(&t)),
        Kind::Qemu => {
            if agent(id, &["ping"]).is_none() {
                return Inside { no_agent: true, ..Inside::default() };
            }
            let (name, windows) = agent(id, &["get-osinfo"]).map_or((None, false), |t| parse_osinfo(&t));
            if windows {
                return Inside { os: name, ..Inside::default() };
            }
            name.or_else(|| exec(kind, id, &["cat", "/etc/os-release"]).and_then(|t| parse_os_release(&t)))
        }
    };
    let listing = exec(kind, id, &["sh", "-c", "apt list --upgradable 2>/dev/null; echo ---hold---; apt-mark showhold"]);
    let up = listing.as_deref().map(parse_upgradable);
    Inside {
        os,
        packages: exec(kind, id, &["dpkg-query", "-W", "-f", "${Package}\n"]).map(|t| count_packages(&t)),
        upgradable: up.map(|u| u.0),
        held: up.map_or(0, |u| u.1),
        lists_age: exec(kind, id, &["stat", "-c", "%Y", "/var/lib/apt/lists"])
            .and_then(|t| t.trim().parse::<u64>().ok())
            .map(|m| now.saturating_sub(m)),
        reboot_required: exec(kind, id, &["test", "-e", "/var/run/reboot-required"]).is_some(),
        apps: if kind == Kind::Lxc {
            exec(kind, id, &["sh", "-c", apps::FIND]).map(|t| apps::parse_found(&t)).unwrap_or_default()
        } else {
            vec![]
        },
        upgrades: listing.as_deref().map(parse_upgrade_names).unwrap_or_default(),
        no_agent: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_tasks_and_logs() {
        let j = r#"[{"upid":"UPID:a:1","type":"vzdump","id":"","user":"root@pam","starttime":100,"endtime":150,"status":"job errors"},
                    {"upid":"UPID:a:2","type":"qmstart","id":"101","user":"root@pam","starttime":200}]"#;
        let t = parse_host_tasks(j, "a");
        assert_eq!((t[0].kind.as_str(), t[0].end, t[0].failed()), ("qmstart", None, false));
        assert!(t[1].failed() && t[1].id.is_empty());
        assert_eq!(parse_task_log(r#"[{"n":1,"t":"a"},{"n":2,"t":"b"},{"n":3,"t":"c"}]"#, 2), vec!["b", "c"]);
    }

    #[test]
    fn zpool_health_and_temperature() {
        let p = parse_zpool("rpool\tONLINE\ntank\tDEGRADED\n");
        assert_eq!(p, vec![("rpool".into(), "ONLINE".into()), ("tank".into(), "DEGRADED".into())]);
        let c = Cluster { pools: p, ..Cluster::default() };
        assert_eq!(c.bad_pools().len(), 1);
        assert_eq!(max_temp(&[45000, 71500, 0, -5]), Some(71.5));
        assert_eq!(max_temp(&[]), None);
    }

    #[test]
    fn upgrade_names_and_impact() {
        let t = "Listing...\nlibc6/stable 2.36-9 amd64 [upgradable from: 2.36-8]\nvim/stable 9.0 amd64 [upgradable from: 8.9]\n---hold---\nvim\n";
        assert_eq!(parse_upgrade_names(t), vec!["libc6".to_string()]);
        assert!(high_impact("linux-image-6.8") && high_impact("pve-manager") && !high_impact("vim"));
    }

    #[test]
    fn backups_from_content_and_tasks() {
        assert_eq!(parse_backups(r#"[{"ctime":100},{"ctime":300},{"x":1}]"#), vec![100, 300]);
        let ok = Task { kind: "vzdump".into(), start: 500, end: Some(520), status: "OK".into() };
        let bad = Task { kind: "vzdump".into(), start: 900, end: Some(910), status: "job errors".into() };
        let h = History { tasks: vec![ok, bad], snapshots: vec![], backups: vec![300] };
        assert_eq!(h.last_backup(), Some(520));
        assert_eq!(h.backup_failure_since(800).map(|t| t.start), Some(900));
        assert!(h.backup_failure_since(1000).is_none());
        assert_eq!(History::default().last_backup(), None);
    }

    const LIST: &str = r#"[
      {"type":"node","node":"pve","status":"online","cpu":0.07,"maxcpu":16,"mem":8000000000,"maxmem":64000000000,"uptime":1000,"id":"node/pve"},
      {"type":"lxc","vmid":101,"name":"web","node":"pve","status":"running","cpu":0.5,"maxcpu":2,"mem":100,"maxmem":512,"disk":5,"maxdisk":8,
       "netin":10,"netout":20,"diskread":30,"diskwrite":40,"uptime":99,"tags":"prod;web","template":0},
      {"type":"qemu","vmid":100,"name":"vm","node":"pve","status":"stopped","cpu":0,"maxcpu":4,"mem":0,"maxmem":4096,"tags":"","template":1},
      {"type":"storage","storage":"local","node":"pve"}
    ]"#;

    #[test]
    fn the_task_log_gives_runs_failures_and_snapshots() {
        let json = r#"[
            {"type":"vzstart","status":"OK","starttime":300,"endtime":301},
            {"type":"vzshutdown","status":"OK","starttime":4000,"endtime":4010},
            {"type":"vzstart","status":"OK","starttime":200,"endtime":201},
            {"type":"vzshutdown","status":"OK","starttime":1000,"endtime":1010},
            {"type":"vzstart","status":"command failed","starttime":400,"endtime":401},
            {"type":"vzsnapshot","status":"WARNINGS: 1","starttime":500,"endtime":501},
            {"type":"vzstart","starttime":9000}
        ]"#;
        let snaps = r#"[{"name":"a","snaptime":50},{"name":"current"},{"name":"b","snaptime":90}]"#;
        let h = History { tasks: parse_tasks(json), snapshots: parse_snapshots(snaps), backups: vec![] };
        assert_eq!(h.runs(5), [Run { start: 300, ran: Some(710) }, Run { start: 200, ran: Some(810) }]);
        assert_eq!(h.last_start(), Some(300));
        assert_eq!(h.failure_since(0).map(|t| t.start), Some(400));
        assert!(h.failure_since(402).is_none());
        assert_eq!(h.oldest_snapshot(), Some(50));
        assert!(History::default().last_start().is_none() && parse_tasks("nope").is_empty());
    }

    #[test]
    fn storage_and_kernel_age_are_read() {
        let j = r#"[{"type":"storage","storage":"local","node":"pve","disk":50,"maxdisk":100,"status":"available"},
                    {"type":"storage","storage":"usb","node":"pve","disk":0,"maxdisk":0,"status":"unknown"},{"type":"node"}]"#;
        let s = parse_storage(j);
        assert_eq!((s.len(), s[0].pct(), s[1].pct()), (2, Some(50.0), None));
        let k = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(kernel_stale("6.8.12-4-pve", &k(&["6.8.12-4-pve", "6.8.12-10-pve"])));
        assert!(!kernel_stale("6.8.12-10-pve", &k(&["6.8.12-4-pve", "6.8.12-10-pve", "6.9.0-generic"])));
    }

    #[test]
    fn held_packages_are_not_upgrades() {
        let t =
            "Listing...\nnodejs/nodistro 24 amd64 [upgradable from: 23]\ncurl/stable 8 amd64 [upgradable from: 7]\n---hold---\nnodejs\n";
        assert_eq!(parse_upgradable(t), (1, 1));
        assert_eq!(parse_upgradable("Listing...\n---hold---\n"), (0, 0));
    }

    #[test]
    fn the_first_apt_error_is_the_one_shown() {
        assert_eq!(apt_error("x\nE: Unmet dependencies.\nE: other\n").as_deref(), Some("E: Unmet dependencies."));
        assert_eq!(apt_error("a\nlast\n\n").as_deref(), Some("last"));
    }

    #[test]
    fn resources_split_into_nodes_and_sorted_guests() {
        let (nodes, guests) = parse_resources(LIST).unwrap();
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].online && nodes[0].cpus == 16);
        assert_eq!(guests.iter().map(|g| g.id).collect::<Vec<_>>(), [100, 101]);
        let web = &guests[1];
        assert_eq!((web.kind, web.status, web.cpu, web.netout), (Kind::Lxc, Status::Running, 0.5, 20));
        assert_eq!(web.tags, ["prod", "web"]);
        assert!(guests[0].template && guests[0].tags.is_empty() && guests[0].status == Status::Stopped);
    }

    #[test]
    fn unreadable_listings_are_errors() {
        assert!(parse_resources("{}").is_err());
        assert!(parse_resources("not json").is_err());
        assert_eq!(parse_resources("[]").unwrap(), (vec![], vec![]));
    }

    #[test]
    fn swap_reads_a_limit_or_none_and_goes_red_near_it() {
        let s = parse_swap("900\n", "1000\n").unwrap();
        assert_eq!(s, Swap { used: 900, max: Some(1000) });
        assert!(s.near_limit());
        let u = parse_swap("5", "max").unwrap();
        assert_eq!((u.max, u.fraction(), u.near_limit()), (None, None, false));
        assert!(parse_swap("x", "max").is_none());
        assert!(!Swap { used: 10, max: Some(0) }.near_limit());
    }

    #[test]
    fn a_containers_swap_comes_from_its_cgroup() {
        let root = crate::util::test_dir("pve-cgroup");
        let d = root.join("lxc/101");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("memory.swap.current"), "512\n").unwrap();
        std::fs::write(d.join("memory.swap.max"), "1024\n").unwrap();
        assert_eq!(lxc_swap(&root, 101), Some(Swap { used: 512, max: Some(1024) }));
        assert_eq!(lxc_swap(&root, 102), None);
        let p = root.join("lxc.payload.103");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("memory.swap.current"), "0\n").unwrap();
        assert_eq!(lxc_swap(&root, 103), Some(Swap { used: 0, max: None }));
    }

    #[test]
    fn what_is_inside_is_counted_from_command_output() {
        let os = "NAME=\"Debian GNU/Linux\"\nPRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\n";
        assert_eq!(parse_os_release(os).as_deref(), Some("Debian GNU/Linux 12 (bookworm)"));
        assert_eq!(parse_os_release("ID=x\n"), None);
        assert_eq!(count_packages("a\nb\n\nc\n"), 3);
        let apt = "Listing...\nbash/stable 5.2 amd64 [upgradable from: 5.1]\ncurl/stable 8 amd64 [upgradable from: 7]\n";
        assert_eq!(count_upgradable(apt), 2);
        assert_eq!(count_upgradable("Listing...\n"), 0);
    }

    #[test]
    fn a_vms_swap_is_its_qemu_processes() {
        assert_eq!(
            parse_vmswap(
                "Name:	qemu
VmSwap:	   2048 kB
"
            ),
            Some(2048 * 1024)
        );
        assert_eq!(
            parse_vmswap(
                "Name:	qemu
"
            ),
            None
        );
        let (run, procs) = (crate::util::test_dir("pve-run"), crate::util::test_dir("pve-proc"));
        std::fs::write(
            run.join("100.pid"),
            "4242
",
        )
        .unwrap();
        std::fs::create_dir_all(procs.join("4242")).unwrap();
        std::fs::write(
            procs.join("4242/status"),
            "VmSwap:	1 kB
",
        )
        .unwrap();
        assert_eq!(vm_swap(&run, &procs, 100), Some(Swap { used: 1024, max: None }));
        assert_eq!(vm_swap(&run, &procs, 101), None);
    }

    #[test]
    fn power_changes_name_the_right_tool_and_fit_the_state() {
        assert_eq!(power_command(Kind::Lxc, 101, Power::Reboot), ("pct", ["reboot".into(), "101".into()]));
        assert_eq!(power_command(Kind::Qemu, 7, Power::Shutdown), ("qm", ["shutdown".into(), "7".into()]));
        assert!(Power::Start.fits(Status::Stopped) && !Power::Start.fits(Status::Running));
        assert!(Power::Reboot.fits(Status::Running) && !Power::Shutdown.fits(Status::Stopped));
        assert!(!Power::Start.fits(Status::Other));
    }

    #[test]
    fn the_guest_agent_answers_are_unwrapped() {
        assert_eq!(parse_guest_exec(r#"{"exitcode":0,"exited":1,"out-data":"a\nb\n"}"#).as_deref(), Some("a\nb\n"));
        assert_eq!(parse_guest_exec(r#"{"exitcode":1,"exited":1}"#), None);
        assert_eq!(parse_guest_exec("nope"), None);
        assert_eq!(parse_osinfo(r#"{"id":"debian","pretty-name":"Debian GNU/Linux 12"}"#), (Some("Debian GNU/Linux 12".into()), false));
        assert_eq!(parse_osinfo(r#"{"result":{"id":"mswindows","name":"Microsoft Windows"}}"#), (Some("Microsoft Windows".into()), true));
        assert_eq!(parse_osinfo("x"), (None, false));
    }
}

/// One process inside a guest, as `ps` reports it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuestProc {
    pub pid: u32,
    pub user: String,
    pub cpu: f32,
    /// Resident memory in bytes.
    pub mem: u64,
    pub name: String,
}

/// The output of `ps -eo pid,user:16,pcpu,rss,args`; lines that do not read as a process (the header) are skipped.
pub fn parse_ps(text: &str) -> Vec<GuestProc> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let user = it.next()?.to_string();
            let cpu = it.next()?.parse().ok()?;
            let rss: u64 = it.next()?.parse().ok()?;
            let name = it.collect::<Vec<_>>().join(" ");
            Some(GuestProc { pid, user, cpu, mem: rss * 1024, name })
        })
        .collect()
}

/// The processes of a running guest, busiest first; `None` when it can not be asked (no agent, not Linux).
/// Slow: call it from a thread of its own.
pub fn processes(kind: Kind, id: u32) -> Option<Vec<GuestProc>> {
    let out = exec(kind, id, &["ps", "-eo", "pid,user:16,pcpu,rss,args", "--sort=-pcpu"])?;
    let v = parse_ps(&out);
    (!v.is_empty()).then_some(v)
}

#[cfg(test)]
mod ps_tests {
    use super::*;

    #[test]
    fn ps_lines_become_processes_and_the_header_does_not() {
        let t = "    PID USER             %CPU   RSS COMMAND\n      1 root              0.0  4096 /sbin/init splash\n  120 www-data       12.5 20480 nginx: worker\n";
        let p = parse_ps(t);
        assert_eq!(p.len(), 2);
        assert_eq!(p[1], GuestProc { pid: 120, user: "www-data".into(), cpu: 12.5, mem: 20480 * 1024, name: "nginx: worker".into() });
        assert_eq!(p[0].name, "/sbin/init splash");
        assert!(parse_ps("nonsense").is_empty());
    }
}
