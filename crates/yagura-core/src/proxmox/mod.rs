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

/// The whole cluster listing, with the containers' swap added. Runs `pvesh`,
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
    for g in guests.iter_mut().filter(|g| g.kind == Kind::Lxc && g.status == Status::Running) {
        g.swap = lxc_swap(Path::new("/sys/fs/cgroup"), g.id);
    }
    Cluster { nodes, guests, note: None }
}

/// What is inside a container, read with `pct exec`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Inside {
    /// `PRETTY_NAME` of `/etc/os-release`.
    pub os: Option<String>,
    pub packages: Option<u32>,
    /// Packages `apt` says can be upgraded, from the lists as they are.
    pub upgradable: Option<u32>,
    /// Seconds since the apt lists were fetched: how far to trust `upgradable`.
    pub lists_age: Option<u64>,
    pub reboot_required: bool,
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

fn exec(id: u32, args: &[&str]) -> Option<String> {
    let o = Command::new("pct").arg("exec").arg(id.to_string()).arg("--").args(args).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Look inside a running container. About two seconds per command, so call
/// it from a thread of its own. Never runs `apt update`: the update count is
/// only as fresh as the lists, and `lists_age` says how fresh that is.
pub fn inspect(id: u32) -> Inside {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    Inside {
        os: exec(id, &["cat", "/etc/os-release"]).and_then(|t| parse_os_release(&t)),
        packages: exec(id, &["dpkg-query", "-W", "-f", "${Package}\\n"]).map(|t| count_packages(&t)),
        upgradable: exec(id, &["apt", "list", "--upgradable"]).map(|t| count_upgradable(&t)),
        lists_age: exec(id, &["stat", "-c", "%Y", "/var/lib/apt/lists"])
            .and_then(|t| t.trim().parse::<u64>().ok())
            .map(|m| now.saturating_sub(m)),
        reboot_required: exec(id, &["test", "-e", "/var/run/reboot-required"]).is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r#"[
      {"type":"node","node":"pve","status":"online","cpu":0.07,"maxcpu":16,"mem":8000000000,"maxmem":64000000000,"uptime":1000,"id":"node/pve"},
      {"type":"lxc","vmid":101,"name":"web","node":"pve","status":"running","cpu":0.5,"maxcpu":2,"mem":100,"maxmem":512,"disk":5,"maxdisk":8,
       "netin":10,"netout":20,"diskread":30,"diskwrite":40,"uptime":99,"tags":"prod;web","template":0},
      {"type":"qemu","vmid":100,"name":"vm","node":"pve","status":"stopped","cpu":0,"maxcpu":4,"mem":0,"maxmem":4096,"tags":"","template":1},
      {"type":"storage","storage":"local","node":"pve"}
    ]"#;

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
}
