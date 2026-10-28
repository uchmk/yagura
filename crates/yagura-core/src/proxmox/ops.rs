//! Operations that write to Proxmox: snapshots (create, delete, roll back) and `vzdump`. Every one runs the node's own tool
//! (`qm`, `pct`, `vzdump`) and waits for it, so call them from a thread of their own; the screen asks before it does.

use std::process::Command;

use serde_json::Value;

use super::{Kind, Storage, pvesh};

/// One snapshot of a guest.
#[derive(Clone, Debug, PartialEq)]
pub struct Snap {
    pub name: String,
    pub time: u64,
    /// A VM snapshot that also holds the memory.
    pub vmstate: bool,
    pub description: String,
}

/// The snapshots in a snapshot list, oldest first (the `current` entry is left out).
pub fn parse_snaps(json: &str) -> Vec<Snap> {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return vec![] };
    let mut out: Vec<Snap> = v
        .as_array()
        .map(|l| {
            l.iter()
                .filter(|s| s["name"].as_str() != Some("current"))
                .filter_map(|s| {
                    Some(Snap {
                        name: s["name"].as_str()?.to_string(),
                        time: s["snaptime"].as_u64().unwrap_or(0),
                        vmstate: s["vmstate"].as_u64().unwrap_or(0) != 0,
                        description: s["description"].as_str().unwrap_or("").trim().to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by_key(|s| s.time);
    out
}

/// The snapshots of a guest (read-only `pvesh`).
pub fn snapshots(node: &str, kind: Kind, id: u32) -> Vec<Snap> {
    let what = if kind == Kind::Lxc { "lxc" } else { "qemu" };
    pvesh(&format!("/nodes/{node}/{what}/{id}/snapshot"), &[]).map(|j| parse_snaps(&j)).unwrap_or_default()
}

/// A snapshot name Proxmox takes: a letter first, then letters, digits, `-` and `_`, at most 40 characters.
pub fn valid_snapname(name: &str) -> bool {
    let mut c = name.chars();
    c.next().is_some_and(|f| f.is_ascii_alphabetic()) && name.len() <= 40 && c.all(|x| x.is_ascii_alphanumeric() || x == '-' || x == '_')
}

/// What to do with a snapshot.
#[derive(Clone, Debug, PartialEq)]
pub enum SnapOp {
    Create { name: String, vmstate: bool },
    Delete(String),
    Rollback(String),
}

impl SnapOp {
    pub fn verb(&self) -> &'static str {
        match self {
            SnapOp::Create { .. } => "snapshot",
            SnapOp::Delete(_) => "delete snapshot",
            SnapOp::Rollback(_) => "roll back to",
        }
    }
}

/// The tool and arguments of a snapshot operation.
pub fn snap_command(kind: Kind, id: u32, op: &SnapOp) -> (&'static str, Vec<String>) {
    let prog = if kind == Kind::Lxc { "pct" } else { "qm" };
    let id = id.to_string();
    let args = match op {
        SnapOp::Create { name, vmstate } => {
            let mut a = vec!["snapshot".to_string(), id, name.clone()];
            if *vmstate && kind != Kind::Lxc {
                a.extend(["--vmstate".to_string(), "1".to_string()]);
            }
            a
        }
        SnapOp::Delete(name) => vec!["delsnapshot".into(), id, name.clone()],
        SnapOp::Rollback(name) => vec!["rollback".into(), id, name.clone()],
    };
    (prog, args)
}

/// Run a snapshot operation and wait for it.
pub fn snapshot(kind: Kind, id: u32, op: &SnapOp) -> Result<(), String> {
    let (prog, args) = snap_command(kind, id, op);
    run(prog, &args)
}

/// How `vzdump` treats a running guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Snapshot,
    Suspend,
    Stop,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Snapshot, Mode::Suspend, Mode::Stop];
    pub fn name(self) -> &'static str {
        match self {
            Mode::Snapshot => "snapshot",
            Mode::Suspend => "suspend",
            Mode::Stop => "stop",
        }
    }
    /// What it does to a running guest, for the question.
    pub fn note(self) -> &'static str {
        match self {
            Mode::Snapshot => "guest keeps running",
            Mode::Suspend => "guest is paused during the copy",
            Mode::Stop => "guest is shut down, copied, started again",
        }
    }
}

/// The compression of a backup file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compress {
    Zstd,
    Lzo,
    Gzip,
    None,
}

impl Compress {
    pub const ALL: [Compress; 4] = [Compress::Zstd, Compress::Lzo, Compress::Gzip, Compress::None];
    /// The value `vzdump --compress` takes.
    pub fn name(self) -> &'static str {
        match self {
            Compress::Zstd => "zstd",
            Compress::Lzo => "lzo",
            Compress::Gzip => "gzip",
            Compress::None => "0",
        }
    }
    /// A rough share of the data left after compression, for the estimate.
    fn ratio(self) -> f64 {
        match self {
            Compress::Zstd => 0.5,
            Compress::Lzo => 0.6,
            Compress::Gzip => 0.45,
            Compress::None => 1.0,
        }
    }
}

/// A backup to start.
#[derive(Clone, Debug, PartialEq)]
pub struct Dump {
    pub storage: String,
    pub mode: Mode,
    pub compress: Compress,
}

/// The arguments of `vzdump` for one guest.
pub fn dump_args(id: u32, d: &Dump) -> Vec<String> {
    vec![
        id.to_string(),
        "--storage".into(),
        d.storage.clone(),
        "--mode".into(),
        d.mode.name().into(),
        "--compress".into(),
        d.compress.name().into(),
    ]
}

/// Run `vzdump` for a guest and wait for it (minutes to hours).
pub fn vzdump(id: u32, d: &Dump) -> Result<(), String> {
    run("vzdump", &dump_args(id, d))
}

/// The storages a backup can go to on a node: active and holding `backup`, the one with the most free space first.
pub fn backup_storages<'a>(storage: &'a [Storage], node: &str) -> Vec<&'a Storage> {
    let mut v: Vec<_> = storage.iter().filter(|s| s.node == node && s.active && s.content.split(',').any(|c| c == "backup")).collect();
    v.sort_by_key(|s| std::cmp::Reverse(s.total.saturating_sub(s.used)));
    v
}

/// The size of the newest backup file in a storage content listing.
pub fn parse_last_backup_size(json: &str) -> Option<u64> {
    let v = serde_json::from_str::<Value>(json).ok()?;
    v.as_array()?.iter().filter_map(|b| Some((b["ctime"].as_u64()?, b["size"].as_u64()?))).max().map(|(_, size)| size)
}

/// The size of the guest's newest backup on any of the storages (read-only `pvesh`).
pub fn last_backup_size(node: &str, id: u32, storages: &[String]) -> Option<u64> {
    storages
        .iter()
        .filter_map(|st| pvesh(&format!("/nodes/{node}/storage/{st}/content"), &["--content", "backup", "--vmid", &id.to_string()]))
        .filter_map(|j| parse_last_backup_size(&j))
        .max()
}

/// How big the next backup will be: the last one's size, else the data the guest holds times a typical compression.
pub fn backup_estimate(last: Option<u64>, used: u64, c: Compress) -> u64 {
    last.unwrap_or((used as f64 * c.ratio()) as u64)
}

/// A command to run on the node: the tool and its arguments.
#[derive(Clone, Debug, PartialEq)]
pub struct Cmd {
    pub prog: String,
    pub args: Vec<String>,
}

impl Cmd {
    pub fn new(prog: &str, args: &[&str]) -> Cmd {
        Cmd { prog: prog.to_string(), args: args.iter().map(|a| a.to_string()).collect() }
    }
}

/// Run the commands one after the other; the first that fails ends it, and the error says which one.
pub fn run_all(cmds: &[Cmd]) -> Result<(), String> {
    for (i, c) in cmds.iter().enumerate() {
        run(&c.prog, &c.args).map_err(|e| if cmds.len() > 1 { format!("{} of {}: {e}", i + 1, cmds.len()) } else { e })?;
    }
    Ok(())
}

/// Stop a running task (`pvesh delete` on the task asks the node to kill it).
pub fn stop_task_cmd(node: &str, upid: &str) -> Cmd {
    Cmd::new("pvesh", &["delete", &format!("/nodes/{node}/tasks/{upid}")])
}

/// One backup file on a storage.
#[derive(Clone, Debug, PartialEq)]
pub struct Backup {
    pub volid: String,
    pub storage: String,
    pub node: String,
    pub vmid: u32,
    pub ctime: u64,
    pub size: u64,
    pub protected: bool,
    pub notes: String,
}

impl Backup {
    /// A VM or a container, from the file name (`vzdump-qemu-100-...`, `vzdump-lxc-101-...`).
    pub fn kind(&self) -> Option<Kind> {
        if self.volid.contains("vzdump-qemu-") {
            Some(Kind::Qemu)
        } else if self.volid.contains("vzdump-lxc-") {
            Some(Kind::Lxc)
        } else {
            None
        }
    }
}

/// The backups in a storage content listing, newest first.
pub fn parse_backup_list(json: &str, node: &str, storage: &str) -> Vec<Backup> {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return vec![] };
    let mut out: Vec<Backup> = v
        .as_array()
        .map(|l| {
            l.iter()
                .filter_map(|b| {
                    Some(Backup {
                        volid: b["volid"].as_str()?.to_string(),
                        storage: storage.to_string(),
                        node: node.to_string(),
                        vmid: b["vmid"].as_u64().unwrap_or(0) as u32,
                        ctime: b["ctime"].as_u64()?,
                        size: b["size"].as_u64().unwrap_or(0),
                        protected: b["protected"].as_u64().unwrap_or(0) != 0 || b["protected"].as_bool().unwrap_or(false),
                        notes: b["notes"].as_str().unwrap_or("").trim().to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by_key(|b| std::cmp::Reverse(b.ctime));
    out
}

/// The backup files on a storage (read-only `pvesh`), newest first.
pub fn backups(node: &str, storage: &str) -> Vec<Backup> {
    pvesh(&format!("/nodes/{node}/storage/{storage}/content"), &["--content", "backup"])
        .map(|j| parse_backup_list(&j, node, storage))
        .unwrap_or_default()
}

/// Delete a backup file (`pvesm free`; Proxmox refuses a protected one).
pub fn backup_delete_cmd(b: &Backup) -> Cmd {
    Cmd::new("pvesm", &["free", &b.volid])
}

/// Restore a backup as a new guest. Never to an id that exists, so nothing is overwritten.
pub fn restore_cmd(b: &Backup, newid: u32, storage: Option<&str>) -> Option<Cmd> {
    let id = newid.to_string();
    let mut c = match b.kind()? {
        Kind::Qemu => Cmd::new("qmrestore", &[&b.volid, &id]),
        Kind::Lxc => Cmd::new("pct", &["restore", &id, &b.volid]),
    };
    if let Some(s) = storage {
        c.args.extend(["--storage".to_string(), s.to_string()]);
    }
    Some(c)
}

/// The id Proxmox would give a new guest (`pvesh get /cluster/nextid` prints it, with quotes).
pub fn parse_next_id(text: &str) -> Option<u32> {
    text.trim().trim_matches('"').parse().ok()
}

pub fn next_id() -> Option<u32> {
    pvesh("/cluster/nextid", &[]).and_then(|t| parse_next_id(&t))
}

/// A guest's configuration as `key: value` pairs, keys sorted (`digest` left out).
pub fn parse_config(json: &str) -> Vec<(String, String)> {
    let Ok(Value::Object(m)) = serde_json::from_str::<Value>(json) else { return vec![] };
    let mut out: Vec<(String, String)> = m
        .into_iter()
        .filter(|(k, _)| k != "digest")
        .map(|(k, v)| {
            let v = match v {
                Value::String(s) => s,
                other => other.to_string(),
            };
            (k, v.replace('\n', " ").trim().to_string())
        })
        .collect();
    out.sort();
    out
}

/// The configuration of a guest (read-only `pvesh`).
pub fn config(node: &str, kind: Kind, id: u32) -> Option<Vec<(String, String)>> {
    let what = if kind == Kind::Lxc { "lxc" } else { "qemu" };
    pvesh(&format!("/nodes/{node}/{what}/{id}/config"), &[]).map(|j| parse_config(&j)).filter(|c| !c.is_empty())
}

/// A name Proxmox takes for a new guest (a DNS-like name: letters, digits, `-` and `.`; starts with a letter or digit).
pub fn valid_guest_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// `qm clone` / `pct clone`: a copy of guest `id` as `newid`. Only a template can be cloned linked (`full` false).
pub fn clone_cmd(kind: Kind, id: u32, newid: u32, name: &str, full: bool) -> Cmd {
    let (prog, name_opt) = if kind == Kind::Lxc { ("pct", "--hostname") } else { ("qm", "--name") };
    let (id, newid) = (id.to_string(), newid.to_string());
    let mut args = vec!["clone", id.as_str(), newid.as_str(), name_opt, name];
    if full {
        args.extend(["--full", "1"]);
    }
    Cmd::new(prog, &args)
}

/// `qm template` / `pct template`: turn a stopped guest into a template (it can not be started again).
pub fn template_cmd(kind: Kind, id: u32) -> Cmd {
    Cmd::new(if kind == Kind::Lxc { "pct" } else { "qm" }, &["template", &id.to_string()])
}

/// `qm destroy` / `pct destroy`: remove a stopped guest and its disks (`--purge` also drops it from backup jobs and HA).
pub fn destroy_cmd(kind: Kind, id: u32) -> Cmd {
    Cmd::new(if kind == Kind::Lxc { "pct" } else { "qm" }, &["destroy", &id.to_string(), "--purge"])
}

/// What to do with a whole node.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NodeCmd {
    Reboot,
    Shutdown,
    /// Refresh the package lists (`apt update`); installs nothing.
    AptUpdate,
}

/// The `pvesh` call for a node (works for any node of the cluster, a task or a command to its status).
pub fn node_cmd(node: &str, c: NodeCmd) -> Cmd {
    match c {
        NodeCmd::Reboot => Cmd::new("pvesh", &["create", &format!("/nodes/{node}/status"), "--command", "reboot"]),
        NodeCmd::Shutdown => Cmd::new("pvesh", &["create", &format!("/nodes/{node}/status"), "--command", "shutdown"]),
        NodeCmd::AptUpdate => Cmd::new("pvesh", &["create", &format!("/nodes/{node}/apt/update")]),
    }
}

/// The disks in a guest's configuration as (key, value), e.g. `("scsi0", "local-lvm:vm-100-disk-0,size=32G")`.
/// CD-ROMs and cloud-init drives are left out; a container's `rootfs` and `mpN` count.
pub fn disks(kind: Kind, cfg: &[(String, String)]) -> Vec<(String, String)> {
    let digits = |k: &str, p: &str| k.strip_prefix(p).is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()));
    cfg.iter()
        .filter(|(k, v)| {
            if kind == Kind::Lxc {
                k == "rootfs" || digits(k, "mp")
            } else {
                ["scsi", "virtio", "sata", "ide"].iter().any(|p| digits(k, p)) && !v.contains("media=cdrom") && !v.contains("cloudinit")
            }
        })
        .cloned()
        .collect()
}

/// `qm resize` / `pct resize`: grow a disk by `gib` GiB (a disk cannot be shrunk this way).
pub fn resize_cmd(kind: Kind, id: u32, disk: &str, gib: u32) -> Cmd {
    Cmd::new(if kind == Kind::Lxc { "pct" } else { "qm" }, &["resize", &id.to_string(), disk, &format!("+{gib}G")])
}

/// The console of a running guest: `qm terminal` for a VM (leave with Ctrl+O), `pct enter` for a container (leave with `exit`).
pub fn console_cmd(kind: Kind, id: u32) -> Cmd {
    if kind == Kind::Lxc { Cmd::new("pct", &["enter", &id.to_string()]) } else { Cmd::new("qm", &["terminal", &id.to_string()]) }
}

/// What `qm set` / `pct set` should change; `None` keeps the guest's value.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SetOpts {
    pub mem_mib: Option<u32>,
    pub cores: Option<u32>,
    /// Containers only (a VM has no swap setting; it is ignored there).
    pub swap_mib: Option<u32>,
    /// Number of host cores the guest may use at most; 0 = unlimited.
    pub cpulimit: Option<u32>,
}

/// `qm set` / `pct set` for the memory, cores, swap and CPU limit.
pub fn set_cmd(kind: Kind, id: u32, o: SetOpts) -> Cmd {
    let lxc = kind == Kind::Lxc;
    let mut c = Cmd::new(if lxc { "pct" } else { "qm" }, &["set", &id.to_string()]);
    let mut add = |flag: &str, v: Option<u32>| {
        if let Some(v) = v {
            c.args.extend([flag.to_string(), v.to_string()]);
        }
    };
    add("--memory", o.mem_mib);
    add("--cores", o.cores);
    if lxc {
        add("--swap", o.swap_mib);
    }
    add("--cpulimit", o.cpulimit);
    c
}

/// Maintenance mode of a node in an HA cluster.
pub fn maintenance_cmd(node: &str, on: bool) -> Cmd {
    Cmd::new("ha-manager", &["crm-command", "node-maintenance", if on { "enable" } else { "disable" }, node])
}

/// Something the cleanup would remove: what it is, the room it frees (0 when unknown) and the command.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub what: String,
    pub bytes: u64,
    pub cmd: Cmd,
}

/// The snapshots of a guest older than `limit` seconds at `now`.
pub fn plan_snapshots(id: u32, name: &str, kind: Kind, snaps: &[Snap], now: u64, limit: u64) -> Vec<Item> {
    snaps
        .iter()
        .filter(|s| s.time > 0 && now.saturating_sub(s.time) >= limit)
        .map(|s| {
            let (prog, args) = snap_command(kind, id, &SnapOp::Delete(s.name.clone()));
            Item { what: format!("snapshot {} of {id} {name}", s.name), bytes: 0, cmd: Cmd { prog: prog.to_string(), args } }
        })
        .collect()
}

/// The backups beyond the newest `keep` of each guest; a protected one is never listed and does not count.
pub fn plan_backups(list: &[Backup], keep: usize) -> Vec<Item> {
    let mut vmids: Vec<u32> = list.iter().map(|b| b.vmid).collect();
    vmids.sort_unstable();
    vmids.dedup();
    let mut out = vec![];
    for id in vmids {
        let mut mine: Vec<&Backup> = list.iter().filter(|b| b.vmid == id && !b.protected).collect();
        mine.sort_by_key(|b| std::cmp::Reverse(b.ctime));
        for b in mine.into_iter().skip(keep) {
            let file = b.volid.rsplit('/').next().unwrap_or(&b.volid);
            out.push(Item { what: format!("backup of {id} on {}: {file}", b.storage), bytes: b.size, cmd: backup_delete_cmd(b) });
        }
    }
    out
}

/// Run a tool and wait; the error is the line that says what went wrong.
fn run(prog: &str, args: &[String]) -> Result<(), String> {
    let o = Command::new(prog).args(args).output().map_err(|e| format!("{prog}: {e}"))?;
    if o.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&o.stderr);
    let out = String::from_utf8_lossy(&o.stdout);
    let line = err
        .lines()
        .chain(out.lines())
        .find(|l| l.starts_with("ERROR"))
        .or_else(|| err.lines().find(|l| !l.trim().is_empty()))
        .unwrap_or("failed");
    Err(line.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_and_destroy_commands() {
        assert_eq!(clone_cmd(Kind::Qemu, 100, 105, "web", true).args, ["clone", "100", "105", "--name", "web", "--full", "1"]);
        assert_eq!(clone_cmd(Kind::Lxc, 101, 106, "ct", false), Cmd::new("pct", &["clone", "101", "106", "--hostname", "ct"]));
        assert_eq!(template_cmd(Kind::Qemu, 100), Cmd::new("qm", &["template", "100"]));
        assert_eq!(destroy_cmd(Kind::Qemu, 100).args, ["destroy", "100", "--purge"]);
        assert_eq!(destroy_cmd(Kind::Lxc, 101).prog, "pct");
        assert!(valid_guest_name("web-1.lan") && !valid_guest_name("") && !valid_guest_name("-x") && !valid_guest_name("a b"));
    }

    #[test]
    fn node_commands_go_through_pvesh() {
        assert_eq!(node_cmd("pve", NodeCmd::Reboot).args, ["create", "/nodes/pve/status", "--command", "reboot"]);
        assert_eq!(node_cmd("pve", NodeCmd::AptUpdate).args, ["create", "/nodes/pve/apt/update"]);
        assert_eq!(node_cmd("b", NodeCmd::Shutdown).prog, "pvesh");
    }

    #[test]
    fn disks_are_picked_from_the_config_and_grown() {
        let c = |k: &str, v: &str| (k.to_string(), v.to_string());
        let vm = [
            c("cores", "2"),
            c("scsi0", "local-lvm:vm-100-disk-0,size=32G"),
            c("ide2", "local:iso/x.iso,media=cdrom"),
            c("ide0", "local-lvm:vm-100-cloudinit,media=cdrom"),
            c("sata1", "tank:vm-100-disk-1,size=1T"),
            c("scsihw", "virtio-scsi-pci"),
        ];
        let keys: Vec<String> = disks(Kind::Qemu, &vm).into_iter().map(|d| d.0).collect();
        assert_eq!(keys, ["scsi0", "sata1"]);
        let ct = [c("rootfs", "local-lvm:vm-101-disk-0,size=8G"), c("mp0", "tank:vm-101-disk-1,mp=/data,size=100G"), c("memory", "512")];
        assert_eq!(disks(Kind::Lxc, &ct).len(), 2);
        assert_eq!(resize_cmd(Kind::Lxc, 101, "rootfs", 4).args, ["resize", "101", "rootfs", "+4G"]);
        assert_eq!(resize_cmd(Kind::Qemu, 100, "scsi0", 10), Cmd::new("qm", &["resize", "100", "scsi0", "+10G"]));
    }

    #[test]
    fn snapshot_commands_and_names() {
        let c = SnapOp::Create { name: "before".into(), vmstate: true };
        assert_eq!(
            snap_command(Kind::Qemu, 7, &c),
            ("qm", vec!["snapshot".into(), "7".into(), "before".into(), "--vmstate".into(), "1".into()])
        );
        assert_eq!(snap_command(Kind::Lxc, 101, &c).1, vec!["snapshot", "101", "before"]);
        assert_eq!(snap_command(Kind::Lxc, 101, &SnapOp::Rollback("a".into())), ("pct", vec!["rollback".into(), "101".into(), "a".into()]));
        assert_eq!(snap_command(Kind::Qemu, 7, &SnapOp::Delete("a".into())).1[0], "delsnapshot");
        assert!(valid_snapname("yagura-20261028_1015") && !valid_snapname("1abc") && !valid_snapname("a b") && !valid_snapname(""));
        assert!(!valid_snapname(&"a".repeat(41)));
    }

    #[test]
    fn snapshot_list_skips_current() {
        let j = r#"[{"name":"b","snaptime":20,"vmstate":1},{"name":"current"},{"name":"a","snaptime":10,"description":" x "}]"#;
        let s = parse_snaps(j);
        assert_eq!(s.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert!(s[1].vmstate && !s[0].vmstate && s[0].description == "x");
    }

    #[test]
    fn dump_arguments_storages_and_estimate() {
        let d = Dump { storage: "nas".into(), mode: Mode::Snapshot, compress: Compress::Zstd };
        assert_eq!(dump_args(101, &d), ["101", "--storage", "nas", "--mode", "snapshot", "--compress", "zstd"]);
        let st = |n: &str, used, content: &str, active| Storage {
            name: n.into(),
            node: "pve".into(),
            used,
            total: 100,
            active,
            kind: "dir".into(),
            content: content.into(),
        };
        let all = [st("a", 90, "backup", true), st("b", 10, "backup,iso", true), st("c", 0, "images", true), st("d", 0, "backup", false)];
        assert_eq!(backup_storages(&all, "pve").iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(parse_last_backup_size(r#"[{"ctime":1,"size":5},{"ctime":9,"size":7}]"#), Some(7));
        assert_eq!(backup_estimate(Some(7), 100, Compress::Zstd), 7);
        assert_eq!(backup_estimate(None, 100, Compress::Zstd), 50);
        assert_eq!(Compress::None.name(), "0");
    }

    #[test]
    fn backups_restore_config_and_cleanup() {
        let j = r#"[{"volid":"nas:backup/vzdump-lxc-101-2026_10_01.tar.zst","ctime":10,"size":5,"vmid":101,"protected":0,"notes":" n "},
            {"volid":"nas:backup/vzdump-qemu-100-2026_10_02.vma.zst","ctime":20,"size":7,"vmid":100,"protected":1},
            {"volid":"nas:backup/vzdump-lxc-101-2026_10_03.tar.zst","ctime":30,"size":6,"vmid":101},
            {"volid":"nas:backup/vzdump-lxc-101-2026_10_04.tar.zst","ctime":40,"size":6,"vmid":101}]"#;
        let l = parse_backup_list(j, "pve", "nas");
        assert_eq!(l.iter().map(|b| b.ctime).collect::<Vec<_>>(), [40, 30, 20, 10]);
        assert!(l[2].protected && !l[3].protected && l[3].notes == "n");
        assert_eq!(l[2].kind(), Some(Kind::Qemu));
        assert_eq!(
            restore_cmd(&l[2], 120, Some("local-lvm")).unwrap(),
            Cmd::new("qmrestore", &["nas:backup/vzdump-qemu-100-2026_10_02.vma.zst", "120", "--storage", "local-lvm"])
        );
        assert_eq!(restore_cmd(&l[3], 121, None).unwrap().args[..2], ["restore", "121"]);
        // Keep the newest 1 of guest 101; the protected backup of 100 is never offered.
        let plan = plan_backups(&l, 1);
        assert_eq!(plan.iter().map(|i| i.bytes).collect::<Vec<_>>(), [6, 5]);
        assert_eq!(plan[0].cmd, Cmd::new("pvesm", &["free", "nas:backup/vzdump-lxc-101-2026_10_03.tar.zst"]));
        assert!(plan_backups(&l, 3).is_empty());
        assert_eq!(parse_next_id("\"105\"\n"), Some(105));
        assert_eq!(parse_next_id("x"), None);
        let c = parse_config(r#"{"memory":2048,"digest":"x","name":"web","net0":"virtio,bridge=vmbr0"}"#);
        assert_eq!(c.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["memory", "name", "net0"]);
        assert_eq!(c[0].1, "2048");
        assert_eq!(
            set_cmd(Kind::Lxc, 101, SetOpts { mem_mib: Some(1024), cores: Some(2), ..Default::default() }).args,
            ["set", "101", "--memory", "1024", "--cores", "2"]
        );
        let all = SetOpts { swap_mib: Some(0), cpulimit: Some(1), ..Default::default() };
        assert_eq!(set_cmd(Kind::Lxc, 5, all).args, ["set", "5", "--swap", "0", "--cpulimit", "1"]);
        assert_eq!(set_cmd(Kind::Qemu, 5, all).args, ["set", "5", "--cpulimit", "1"]);
        assert_eq!(set_cmd(Kind::Qemu, 7, SetOpts { cores: Some(4), ..Default::default() }), Cmd::new("qm", &["set", "7", "--cores", "4"]));
        assert_eq!(console_cmd(Kind::Qemu, 7), Cmd::new("qm", &["terminal", "7"]));
        assert_eq!(console_cmd(Kind::Lxc, 101), Cmd::new("pct", &["enter", "101"]));
        assert_eq!(maintenance_cmd("pve", true).args, ["crm-command", "node-maintenance", "enable", "pve"]);
        assert_eq!(stop_task_cmd("pve", "UPID:pve:1").args, ["delete", "/nodes/pve/tasks/UPID:pve:1"]);
        let snap = |name: &str, time| Snap { name: name.into(), time, vmstate: false, description: String::new() };
        let p = plan_snapshots(7, "web", Kind::Qemu, &[snap("old", 100), snap("new", 900)], 1000, 500);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].cmd, Cmd::new("qm", &["delsnapshot", "7", "old"]));
    }
}
