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

/// `pvesh set .../snapshot/<name>/config --description <text>`: change the note of a snapshot.
pub fn snap_note_cmd(node: &str, kind: Kind, id: u32, name: &str, text: &str) -> Cmd {
    let k = if kind == Kind::Lxc { "lxc" } else { "qemu" };
    Cmd::new("pvesh", &["set", &format!("/nodes/{node}/{k}/{id}/snapshot/{name}/config"), "--description", text])
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

/// The tools that only act on the node they run on (`pvesh` and `ha-manager` reach the whole cluster).
const NODE_LOCAL: [&str; 4] = ["qm", "pct", "vzdump", "pveam"];

/// `cmd` for a guest or storage on `node`: run through `ssh root@<node>` (key login, no password prompt, 10 s to connect) when
/// the tool only works on its own node and `node` is not `local`. `tty` asks ssh for a terminal (a console). Anything else is
/// returned as it is, so a single-node setup never touches ssh.
pub fn remote_cmd(local: &str, node: &str, cmd: Cmd, tty: bool) -> Cmd {
    let host_ok = !node.is_empty() && node.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'));
    if !host_ok || node == local || !NODE_LOCAL.contains(&cmd.prog.as_str()) {
        return cmd;
    }
    // ssh hands the words to the remote shell as one line, so each one is quoted.
    let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    let mut args: Vec<String> = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"].iter().map(|s| s.to_string()).collect();
    if tty {
        args.push("-t".into());
    }
    args.push(format!("root@{node}"));
    args.push(std::iter::once(cmd.prog.as_str()).chain(cmd.args.iter().map(String::as_str)).map(quote).collect::<Vec<_>>().join(" "));
    Cmd { prog: "ssh".into(), args }
}

/// [`remote_cmd`] seen from this node.
pub fn on_node(node: &str, cmd: Cmd) -> Cmd {
    remote_cmd(&this_node(), node, cmd, false)
}

/// [`remote_cmd`] with a terminal, for a console.
pub fn console_on_node(node: &str, cmd: Cmd) -> Cmd {
    remote_cmd(&this_node(), node, cmd, true)
}

/// The vzdump command for a guest (run on the node that holds it).
pub fn dump_cmd(id: u32, d: &Dump) -> Cmd {
    Cmd { prog: "vzdump".into(), args: dump_args(id, d) }
}

/// The snapshot command for a guest.
pub fn snap_cmd(kind: Kind, id: u32, op: &SnapOp) -> Cmd {
    let (prog, args) = snap_command(kind, id, op);
    Cmd { prog: prog.into(), args }
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

/// One volume on a storage: a disk image, ISO, container template, backup or snippet.
#[derive(Clone, Debug, PartialEq)]
pub struct Vol {
    pub volid: String,
    /// `images`, `rootdir`, `iso`, `vztmpl`, `backup`, `snippets`...
    pub content: String,
    pub size: u64,
    pub vmid: u32,
}

/// The volumes in a storage content listing, biggest first.
pub fn parse_content_list(json: &str) -> Vec<Vol> {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return vec![] };
    let mut out: Vec<Vol> = v
        .as_array()
        .map(|l| {
            l.iter()
                .filter_map(|b| {
                    Some(Vol {
                        volid: b["volid"].as_str()?.to_string(),
                        content: b["content"].as_str().unwrap_or("?").to_string(),
                        size: b["size"].as_u64().unwrap_or(0),
                        vmid: b["vmid"].as_u64().unwrap_or(0) as u32,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by_key(|b| std::cmp::Reverse(b.size));
    out
}

/// Everything on a storage (read-only `pvesh`), biggest first.
pub fn contents(node: &str, storage: &str) -> Vec<Vol> {
    pvesh(&format!("/nodes/{node}/storage/{storage}/content"), &[]).map(|j| parse_content_list(&j)).unwrap_or_default()
}

/// Bytes and count per kind of content, biggest first.
pub fn content_totals(list: &[Vol]) -> Vec<(String, u64, usize)> {
    let mut out: Vec<(String, u64, usize)> = vec![];
    for v in list {
        match out.iter_mut().find(|o| o.0 == v.content) {
            Some(o) => {
                o.1 += v.size;
                o.2 += 1;
            }
            None => out.push((v.content.clone(), v.size, 1)),
        }
    }
    out.sort_by_key(|o| std::cmp::Reverse(o.1));
    out
}

/// One scheduled backup job of the cluster (`Datacenter > Backup`).
#[derive(Clone, Debug, PartialEq)]
pub struct BackupJob {
    pub id: String,
    pub enabled: bool,
    pub schedule: String,
    pub storage: String,
    /// `all`, or the guest ids, or the pool.
    pub scope: String,
    pub mode: String,
    pub node: String,
    pub next_run: Option<u64>,
    /// The `prune-backups` setting (`keep-last=3,keep-daily=7`); empty when the job has none.
    pub prune: String,
}

/// The jobs in a `/cluster/backup` listing (a job that does not say `enabled` is on).
pub fn parse_backup_jobs(json: &str) -> Vec<BackupJob> {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return vec![] };
    v.as_array()
        .map(|l| {
            l.iter()
                .filter_map(|j| {
                    let s = |k: &str| j[k].as_str().unwrap_or("").to_string();
                    let on = |k: &str| j[k].as_u64().map(|n| n != 0).or_else(|| j[k].as_bool());
                    let scope = if on("all").unwrap_or(false) {
                        "all".to_string()
                    } else if let Some(p) = j["pool"].as_str() {
                        format!("pool {p}")
                    } else {
                        s("vmid")
                    };
                    Some(BackupJob {
                        id: j["id"].as_str()?.to_string(),
                        enabled: on("enabled").unwrap_or(true),
                        schedule: s("schedule"),
                        storage: s("storage"),
                        scope,
                        mode: s("mode"),
                        node: s("node"),
                        next_run: j["next-run"].as_u64(),
                        prune: s("prune-backups"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The backup jobs of the cluster (read-only `pvesh`).
pub fn backup_jobs() -> Vec<BackupJob> {
    pvesh("/cluster/backup", &[]).map(|j| parse_backup_jobs(&j)).unwrap_or_default()
}

/// `pvesh set /cluster/backup/<id> --enabled 0|1`.
pub fn backup_job_cmd(id: &str, on: bool) -> Cmd {
    Cmd::new("pvesh", &["set", &format!("/cluster/backup/{id}"), "--enabled", if on { "1" } else { "0" }])
}

/// Runs a backup job once, now: `pvesh create /nodes/<node>/vzdump` with the job's storage, mode and guests. It answers with a
/// task, so it shows in the Tasks screen. A job without a node runs on `fallback`.
pub fn backup_job_run_cmd(j: &BackupJob, fallback: &str) -> Cmd {
    let node = if j.node.is_empty() { fallback } else { &j.node };
    let node = if !node.is_empty() && node.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) { node } else { "invalid" };
    let path = format!("/nodes/{node}/vzdump");
    let mut a = vec!["create", path.as_str(), "--storage", j.storage.as_str(), "--mode", j.mode.as_str()];
    match j.scope.as_str() {
        "all" => a.extend(["--all", "1"]),
        s => match s.strip_prefix("pool ") {
            Some(p) => a.extend(["--pool", p]),
            None => a.extend(["--vmid", s]),
        },
    }
    Cmd::new("pvesh", &a)
}

/// `pvesh delete /cluster/backup/<id>`.
pub fn backup_job_delete_cmd(id: &str) -> Cmd {
    Cmd::new("pvesh", &["delete", &format!("/cluster/backup/{id}")])
}

/// The call that creates a backup job (`id` is `None`) or changes one, from the four fields `[storage, schedule, scope, mode]`.
/// The scope is `all` or guest ids like `100,101`; a pool scope (`pool x`, kept as it is) is accepted only when editing.
pub fn backup_job_save_cmd(id: Option<&str>, fields: &[String; 4]) -> Result<Cmd, String> {
    let [storage, schedule, scope, mode] = fields.each_ref().map(|s| s.trim());
    if storage.is_empty() || !storage.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
        return Err("the storage is a name like local or nas-1".into());
    }
    if schedule.is_empty() || schedule.starts_with('-') || schedule.chars().any(char::is_control) {
        return Err("the schedule is like 21:00, daily or mon..fri 02:30".into());
    }
    if !["snapshot", "suspend", "stop"].contains(&mode) {
        return Err("the mode is snapshot, suspend or stop".into());
    }
    let ids = !scope.is_empty() && scope.split(',').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    let mut a: Vec<String> = match id {
        Some(id) => vec!["set".into(), format!("/cluster/backup/{id}")],
        None => vec!["create".into(), "/cluster/backup".into()],
    };
    for (k, v) in [("--storage", storage), ("--schedule", schedule), ("--mode", mode)] {
        a.push(k.into());
        a.push(v.into());
    }
    let pool = id.is_some() && scope.starts_with("pool ");
    let more: &[&str] = match (scope, ids) {
        ("all", _) if id.is_some() => &["--all", "1", "--delete", "vmid,pool"],
        ("all", _) => &["--all", "1"],
        (_, true) if id.is_some() => &["--all", "0", "--vmid", scope],
        (_, true) => &["--vmid", scope],
        _ if pool => &[],
        _ => return Err("the scope is all, or guest ids like 100,101".into()),
    };
    a.extend(more.iter().map(|s| s.to_string()));
    let args: Vec<&str> = a.iter().map(String::as_str).collect();
    Ok(Cmd::new("pvesh", &args))
}

/// The `keep-*` options of `prune-backups`, in the order of the retention form.
pub const PRUNE_KEYS: [&str; 6] = ["keep-last", "keep-hourly", "keep-daily", "keep-weekly", "keep-monthly", "keep-yearly"];

/// The six retention values out of a `prune-backups` string (`keep-last=3,keep-daily=7`); a missing one is empty.
pub fn parse_prune(s: &str) -> [String; 6] {
    PRUNE_KEYS.map(|k| s.split(',').find_map(|p| p.strip_prefix(k)?.strip_prefix('=')).unwrap_or("").to_string())
}

/// `pvesh set /cluster/backup/<id> --prune-backups …` (or `--delete prune-backups` when every value is empty). Values are counts.
pub fn backup_prune_cmd(id: &str, vals: &[String; 6]) -> Result<Cmd, String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
        return Err("the job id is wrong".into());
    }
    let mut parts = vec![];
    for (k, v) in PRUNE_KEYS.iter().zip(vals) {
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        if v.len() > 4 || !v.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("{k} is a number of backups to keep"));
        }
        parts.push(format!("{k}={}", v.parse::<u32>().unwrap_or(0)));
    }
    let path = format!("/cluster/backup/{id}");
    Ok(if parts.is_empty() {
        Cmd::new("pvesh", &["set", &path, "--delete", "prune-backups"])
    } else {
        Cmd::new("pvesh", &["set", &path, "--prune-backups", &parts.join(",")])
    })
}

/// One HA-managed guest (`Datacenter > HA`): what is asked for and what the manager reports.
#[derive(Clone, Debug, PartialEq)]
pub struct HaRes {
    /// `vm:100` or `ct:101`.
    pub sid: String,
    /// The requested state: `started`, `stopped`, `disabled` or `ignored`.
    pub state: String,
    pub group: String,
    /// Where the manager has it, and what it says (`started`, `fence`, `error`...).
    pub node: String,
    pub status: String,
}

/// The resources in `/cluster/ha/resources`, each joined to its row of `/cluster/ha/status/current` (type `service`).
pub fn parse_ha(resources: &str, status: &str) -> Vec<HaRes> {
    let list = |s: &str| serde_json::from_str::<Value>(s).ok().and_then(|v| v.as_array().cloned()).unwrap_or_default();
    let status = list(status);
    list(resources)
        .iter()
        .filter_map(|r| {
            let sid = r["sid"].as_str()?.to_string();
            let live = status.iter().find(|s| s["type"] == "service" && s["id"].as_str() == Some(&format!("service:{sid}")));
            let s = |v: Option<&Value>, k: &str| v.and_then(|v| v[k].as_str()).unwrap_or("").to_string();
            Some(HaRes {
                state: r["state"].as_str().unwrap_or("started").to_string(),
                group: s(Some(r), "group"),
                node: s(live, "node"),
                status: s(live, "state"),
                sid,
            })
        })
        .collect()
}

/// The HA resources of the cluster (read-only `pvesh`).
pub fn ha() -> Vec<HaRes> {
    let res = pvesh("/cluster/ha/resources", &[]).unwrap_or_default();
    let status = pvesh("/cluster/ha/status/current", &[]).unwrap_or_default();
    parse_ha(&res, &status)
}

/// `pvesh set /cluster/ha/resources/<sid> --state started|disabled`.
pub fn ha_cmd(sid: &str, on: bool) -> Cmd {
    Cmd::new("pvesh", &["set", &format!("/cluster/ha/resources/{sid}"), "--state", if on { "started" } else { "disabled" }])
}

/// `pvesh create /cluster/ha/resources --sid vm:<id>|ct:<id> --state started`: put a guest under HA.
pub fn ha_add_cmd(kind: Kind, id: u32) -> Cmd {
    Cmd::new("pvesh", &["create", "/cluster/ha/resources", "--sid", &ha_sid(kind, id), "--state", "started"])
}

/// `pvesh delete /cluster/ha/resources/<sid>`: take a guest out of HA (the guest itself is not touched).
pub fn ha_remove_cmd(kind: Kind, id: u32) -> Cmd {
    Cmd::new("pvesh", &["delete", &format!("/cluster/ha/resources/{}", ha_sid(kind, id))])
}

fn ha_sid(kind: Kind, id: u32) -> String {
    format!("{}:{id}", if kind == Kind::Lxc { "ct" } else { "vm" })
}

/// How a user or token expiry reads: `0` is never.
fn expiry(e: u64, now: u64) -> String {
    match e {
        0 => "never expires".into(),
        e if e <= now => "EXPIRED".into(),
        e => format!("expires in {}d", (e - now).div_ceil(86400)),
    }
}

/// The lines of the access screen from `/access/users --full 1` and `/access/acl`: every user with its tokens, then the permissions.
pub fn parse_access(users: &str, acl: &str, now: u64) -> Vec<String> {
    let mut out = vec![];
    let list = |s: &str| serde_json::from_str::<Value>(s).ok().and_then(|v| v.as_array().cloned()).unwrap_or_default();
    let users = list(users);
    out.push(format!("users ({})", users.len()));
    for u in &users {
        let Some(id) = u["userid"].as_str() else { continue };
        let on = u["enable"].as_u64() != Some(0);
        let groups = u["groups"].as_str().filter(|g| !g.is_empty()).map_or(String::new(), |g| format!("  groups {g}"));
        let exp = u["expire"].as_u64().unwrap_or(0);
        out.push(format!("  {:<24} {}  {}{groups}", id, if on { "on " } else { "OFF" }, expiry(exp, now)));
        for t in u["tokens"].as_array().into_iter().flatten() {
            let Some(name) = t["tokenid"].as_str() else { continue };
            let sep = if t["privsep"].as_u64() != Some(0) { "own permissions" } else { "the user's permissions" };
            out.push(format!("      token {name}: {sep}, {}", expiry(t["expire"].as_u64().unwrap_or(0), now)));
        }
    }
    let acl = list(acl);
    out.push(String::new());
    out.push(format!("permissions ({})", acl.len()));
    for a in &acl {
        let s = |k: &str| a[k].as_str().unwrap_or("").to_string();
        let prop = if a["propagate"].as_u64() != Some(0) { "" } else { "  (this path only)" };
        out.push(format!("  {:<22} {:<18} {} {}{prop}", s("path"), s("roleid"), s("type"), s("ugid")));
    }
    out
}

/// The users, tokens and permissions of the cluster (read-only `pvesh`).
pub fn access(now: u64) -> Vec<String> {
    let users = pvesh("/access/users", &["--full", "1"]).unwrap_or_default();
    let acl = pvesh("/access/acl", &[]).unwrap_or_default();
    if users.is_empty() && acl.is_empty() {
        return vec![];
    }
    parse_access(&users, &acl, now)
}

/// "5m ago" / "in 10m" for a time on the clock (0 or missing reads as "never").
fn when(t: u64, now: u64) -> String {
    let span = |s: u64| match s {
        0..=89 => format!("{s}s"),
        90..=5399 => format!("{}m", s.div_ceil(60)),
        5400..=129_599 => format!("{}h", s.div_ceil(3600)),
        s => format!("{}d", s.div_ceil(86400)),
    };
    match t {
        0 => "never".into(),
        t if t <= now => format!("{} ago", span(now - t)),
        t => format!("in {}", span(t - now)),
    }
}

/// The lines of the replication screen: each job of `/cluster/replication` with its row from the source node's `/nodes/<n>/replication`.
pub fn parse_replication(jobs: &str, statuses: &[String], now: u64) -> Vec<String> {
    let list = |s: &str| serde_json::from_str::<Value>(s).ok().and_then(|v| v.as_array().cloned()).unwrap_or_default();
    let rows: Vec<Value> = statuses.iter().flat_map(|s| list(s)).collect();
    let jobs = list(jobs);
    let mut out = vec![format!("replication jobs ({})", jobs.len())];
    for j in &jobs {
        let Some(id) = j["id"].as_str() else { continue };
        let s = |k: &str| j[k].as_str().unwrap_or("").to_string();
        let off = if j["disable"].as_u64() == Some(1) { "  OFF" } else { "" };
        let rate = j["rate"].as_f64().map_or(String::new(), |r| format!("  rate {r} MB/s"));
        out.push(format!("  {:<8} {} -> {}  {}{rate}{off}", id, s("source"), s("target"), s("schedule")));
        let Some(r) = rows.iter().find(|r| r["id"].as_str() == Some(id)) else { continue };
        let fails = r["fail_count"].as_u64().unwrap_or(0);
        let took = r["duration"].as_f64().map_or(String::new(), |d| format!(" ({d:.0}s)"));
        let last = r["last_sync"].as_u64().unwrap_or(0);
        let next = r["next_sync"].as_u64().unwrap_or(0);
        if fails > 0 {
            let err = r["error"].as_str().unwrap_or("");
            out.push(format!("           FAILING x{fails}: {err}"));
        }
        out.push(format!("           last {}{took}, next {}", when(last, now), when(next, now)));
    }
    out
}

/// The replication jobs of the cluster and how their last runs went (read-only `pvesh`).
pub fn replication(now: u64) -> Vec<String> {
    let jobs = pvesh("/cluster/replication", &[]).unwrap_or_default();
    if jobs.is_empty() {
        return vec![];
    }
    let mut nodes: Vec<String> = serde_json::from_str::<Value>(&jobs)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|j| j["source"].as_str().map(String::from))
        .collect();
    nodes.sort();
    nodes.dedup();
    let rows: Vec<String> = nodes.iter().filter_map(|n| pvesh(&format!("/nodes/{n}/replication"), &[])).collect();
    parse_replication(&jobs, &rows, now)
}

/// The lines of the pool screen: the ZFS pools of `/nodes/<n>/disks/zfs` and the Ceph cluster of `/cluster/ceph/status`.
/// A pool or cluster that is not healthy has `UNHEALTHY` on its line (shown in red).
pub fn parse_pools(zfs: &str, ceph: &str) -> Vec<String> {
    let mut out = vec![];
    let list = serde_json::from_str::<Value>(zfs).ok().and_then(|v| v.as_array().cloned());
    if let Some(pools) = list {
        out.push(format!("zfs pools ({})", pools.len()));
        for p in &pools {
            let s = |k: &str| p[k].as_str().unwrap_or("?").to_string();
            let n = |k: &str| p[k].as_u64().unwrap_or(0);
            let size = n("size");
            let used = (n("alloc") * 100).checked_div(size).unwrap_or(0);
            let health = s("health");
            let bad = if health == "ONLINE" { "" } else { "  UNHEALTHY" };
            out.push(format!("  {:<14} {health}{bad}  {used}% of {}  frag {}%", s("name"), human(size), n("frag")));
        }
    }
    if let Some(c) = serde_json::from_str::<Value>(ceph).ok().filter(|c| c["health"].is_object()) {
        let st = c["health"]["status"].as_str().unwrap_or("?");
        let bad = if st == "HEALTH_OK" { "" } else { "  UNHEALTHY" };
        out.push(String::new());
        out.push(format!("ceph {st}{bad}"));
        if let Some(checks) = c["health"]["checks"].as_object() {
            for (k, v) in checks {
                let msg = v["summary"]["message"].as_str().unwrap_or("");
                let sev = v["severity"].as_str().unwrap_or("");
                out.push(format!("  {k}: {msg}  ({sev}){}", if sev == "HEALTH_ERR" { "  UNHEALTHY" } else { "" }));
            }
        }
        let pg = &c["pgmap"];
        if let (Some(used), Some(total)) = (pg["bytes_used"].as_u64(), pg["bytes_total"].as_u64()) {
            out.push(format!("  used {} of {}", human(used), human(total)));
        }
        let osd = &c["osdmap"];
        if let (Some(up), Some(all)) = (osd["num_up_osds"].as_u64(), osd["num_osds"].as_u64()) {
            out.push(format!("  osds {up} up of {all}"));
        }
    }
    out
}

/// 1.5 TiB / 20 GiB.
fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[i]) }
}

/// The ZFS pools of this node and the Ceph cluster, if there are any (read-only `pvesh`).
pub fn pools() -> Vec<String> {
    let node = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let node = node.trim();
    let zfs = if node.is_empty() { String::new() } else { pvesh(&format!("/nodes/{node}/disks/zfs"), &[]).unwrap_or_default() };
    let ceph = pvesh("/cluster/ceph/status", &[]).unwrap_or_default();
    parse_pools(&zfs, &ceph)
}

/// One service of a node (`/nodes/<n>/services`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub name: String,
    pub desc: String,
    pub state: String,
}

/// The services of `/nodes/<n>/services`, in the order Proxmox gives.
pub fn parse_services(json: &str) -> Vec<Service> {
    let v: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let s = |x: &Value, k: &str| x[k].as_str().unwrap_or("").to_string();
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    let name = x["service"].as_str().or_else(|| x["name"].as_str())?.to_string();
                    Some(Service { name, desc: s(x, "desc"), state: s(x, "state") })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// This node's name, from the kernel.
pub fn this_node() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim().to_string()
}

/// This node and its services (read-only `pvesh`).
pub fn services() -> (String, Vec<Service>) {
    let node = this_node();
    let list = if node.is_empty() {
        vec![]
    } else {
        pvesh(&format!("/nodes/{node}/services"), &[]).map(|j| parse_services(&j)).unwrap_or_default()
    };
    (node, list)
}

/// `pvesh create /nodes/<n>/services/<name>/<action>` (`start`, `stop`, `restart` or `reload`); the names are letters,
/// digits and `-_.@` only.
pub fn service_cmd(node: &str, name: &str, action: &str) -> Cmd {
    let safe = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.@".contains(c));
    let ok = safe(node) && safe(name) && ["start", "stop", "restart", "reload"].contains(&action);
    let (node, name, action) = if ok { (node, name, action) } else { ("invalid", "invalid", "invalid") };
    Cmd::new("pvesh", &["create", &format!("/nodes/{node}/services/{name}/{action}")])
}

/// The editable node settings `[timezone, search, dns1, dns2, dns3]` from the `/time` and `/dns` answers.
pub fn parse_node_values(time: &str, dns: &str) -> Option<[String; 5]> {
    let j = |s: &str| serde_json::from_str::<Value>(s).unwrap_or(Value::Null);
    let (time, dns) = (j(time), j(dns));
    if !time.is_object() || !dns.is_object() {
        return None;
    }
    let g = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_string();
    Some([g(&time, "timezone"), g(&dns, "search"), g(&dns, "dns1"), g(&dns, "dns2"), g(&dns, "dns3")])
}

/// The current time zone and resolver of this node, to start the edit form from.
pub fn node_values() -> Option<[String; 5]> {
    let node = this_node();
    if node.is_empty() {
        return None;
    }
    let get = |p: &str| pvesh(&format!("/nodes/{node}/{p}"), &[]).unwrap_or_default();
    parse_node_values(&get("time"), &get("dns"))
}

/// The two calls that set the time zone and the resolver from `[timezone, search, dns1, dns2, dns3]`. The resolver is written whole,
/// so an empty `dns2` or `dns3` drops that server. `node` comes from this machine's host name.
pub fn node_save_cmds(node: &str, fields: &[String; 5]) -> Result<Vec<Cmd>, String> {
    let [zone, search, d1, d2, d3] = fields.each_ref().map(|s| s.trim());
    let word = |s: &str, extra: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c));
    if !word(node, "-_.") {
        return Err("this machine's host name is not usable".into());
    }
    if !word(zone, "/_+-") || zone.starts_with('/') || zone.contains("..") {
        return Err("the time zone is like Asia/Tokyo or UTC".into());
    }
    if !word(search, ".-_") {
        return Err("the search domain is like example.com or lan".into());
    }
    let ip = |s: &str| s.parse::<std::net::IpAddr>().is_ok();
    if !ip(d1) {
        return Err("dns1 is an IP address like 192.168.1.1".into());
    }
    for (n, d) in [("dns2", d2), ("dns3", d3)] {
        if !d.is_empty() && !ip(d) {
            return Err(format!("{n} is an IP address, or empty"));
        }
    }
    let mut dns = vec!["set".to_string(), format!("/nodes/{node}/dns"), "--search".into(), search.into(), "--dns1".into(), d1.into()];
    for (k, d) in [("--dns2", d2), ("--dns3", d3)] {
        if !d.is_empty() {
            dns.push(k.into());
            dns.push(d.into());
        }
    }
    let dns: Vec<&str> = dns.iter().map(String::as_str).collect();
    Ok(vec![Cmd::new("pvesh", &["set", &format!("/nodes/{node}/time"), "--timezone", zone]), Cmd::new("pvesh", &dns)])
}

/// The call that adds an NFS storage to the cluster from `[name, server, export, content]`. The content is a comma list of
/// images, rootdir, vztmpl, iso, backup and snippets.
pub fn storage_add_nfs_cmd(fields: &[String; 4]) -> Result<Cmd, String> {
    let [name, server, export, content] = fields.each_ref().map(|s| s.trim());
    let word = |s: &str, extra: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c));
    if !word(name, "._-") || !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err("the storage name is like nas-1 (letters, digits, . _ -)".into());
    }
    if !word(server, ".-:") || server.starts_with('-') {
        return Err("the server is a host name or an IP address".into());
    }
    if !word(export, "/._-") || !export.starts_with('/') || export.contains("..") {
        return Err("the export is a path on the server like /export/pve".into());
    }
    let kinds = ["images", "rootdir", "vztmpl", "iso", "backup", "snippets"];
    if content.is_empty() || !content.split(',').all(|c| kinds.contains(&c)) {
        return Err("the content is a list of images, rootdir, vztmpl, iso, backup, snippets".into());
    }
    Ok(Cmd::new(
        "pvesh",
        &["create", "/storage", "--storage", name, "--type", "nfs", "--server", server, "--export", export, "--content", content],
    ))
}

/// The storage types the add form offers, and the labels of its fields `[name, a, b, content]` for each (an empty label is skipped).
pub const STORAGE_KINDS: [(&str, [&str; 4]); 4] = [
    ("nfs", ["name", "server", "export", "content"]),
    ("dir", ["name", "path", "", "content"]),
    ("lvmthin", ["name", "volume group", "thin pool", "content"]),
    ("zfspool", ["name", "pool", "", "content"]),
];

/// The call that adds a storage of type `kind` (one of `STORAGE_KINDS`) from `[name, a, b, content]`.
pub fn storage_add_cmd(kind: &str, fields: &[String; 4]) -> Result<Cmd, String> {
    if kind == "nfs" {
        return storage_add_nfs_cmd(fields);
    }
    let [name, a, b, content] = fields.each_ref().map(|s| s.trim());
    let word = |s: &str, extra: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c));
    if !word(name, "._-") || !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err("the storage name is like local-2 (letters, digits, . _ -)".into());
    }
    let kinds = ["images", "rootdir", "vztmpl", "iso", "backup", "snippets"];
    let content_ok = !content.is_empty() && content.split(',').all(|c| kinds.contains(&c));
    let mut args = vec!["create", "/storage", "--storage", name, "--type", kind];
    match kind {
        "dir" => {
            if !word(a, "/._-") || !a.starts_with('/') || a.contains("..") {
                return Err("the path is a directory on every node like /mnt/data".into());
            }
            args.extend(["--path", a]);
        }
        "lvmthin" => {
            if !word(a, "._-+") || a.starts_with('-') {
                return Err("the volume group is like pve".into());
            }
            if !word(b, "._-+") || b.starts_with('-') {
                return Err("the thin pool is like data".into());
            }
            args.extend(["--vgname", a, "--thinpool", b]);
        }
        "zfspool" => {
            if !word(a, "/._-:") || a.starts_with(['-', '/']) || a.contains("..") {
                return Err("the pool is a ZFS dataset like tank/vm".into());
            }
            args.extend(["--pool", a]);
        }
        _ => return Err("this storage type is not supported".into()),
    }
    if !content_ok {
        return Err("the content is a list of images, rootdir, vztmpl, iso, backup, snippets".into());
    }
    args.extend(["--content", content]);
    Ok(Cmd::new("pvesh", &args))
}

/// The call that changes what a storage may hold and whether it is enabled; an empty field is left as it is.
pub fn storage_edit_cmd(name: &str, content: &str, enabled: &str) -> Result<Cmd, String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
        return Err("the storage name is not valid".into());
    }
    let (content, enabled) = (content.trim(), enabled.trim().to_ascii_lowercase());
    let path = format!("/storage/{name}");
    let mut args = vec!["set", path.as_str()];
    if !content.is_empty() {
        let kinds = ["images", "rootdir", "vztmpl", "iso", "backup", "snippets"];
        if !content.split(',').all(|c| kinds.contains(&c)) {
            return Err("the content is a list of images, rootdir, vztmpl, iso, backup, snippets".into());
        }
        args.extend(["--content", content]);
    }
    match enabled.as_str() {
        "" => {}
        "yes" | "y" | "1" => args.extend(["--disable", "0"]),
        "no" | "n" | "0" => args.extend(["--disable", "1"]),
        _ => return Err("enabled is yes or no".into()),
    }
    if args.len() == 2 {
        return Err("nothing to change".into());
    }
    Ok(Cmd::new("pvesh", &args))
}

/// The call that removes a storage definition from the cluster (the data on it is not touched).
pub fn storage_remove_cmd(name: &str) -> Cmd {
    let ok = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    Cmd::new("pvesh", &["delete", &format!("/storage/{}", if ok { name } else { "invalid" })])
}

/// The lines of the node page: the clock (`/nodes/<n>/time`), the resolver (`/dns`) and the hosts file (`/hosts`).
pub fn parse_node_settings(time: &str, dns: &str, hosts: &str) -> Vec<String> {
    let j = |s: &str| serde_json::from_str::<Value>(s).unwrap_or(Value::Null);
    let (time, dns, hosts) = (j(time), j(dns), j(hosts));
    let mut out = vec![];
    if time.is_object() {
        out.push("time".to_string());
        out.push(format!("  zone  {}", time["timezone"].as_str().unwrap_or("?")));
        if let Some(t) = time["localtime"].as_u64() {
            out.push(format!("  local {} (seconds since 1970 as the node's zone counts them)", t));
        }
        if let Some(t) = time["time"].as_u64() {
            out.push(format!("  utc   {t} (seconds since 1970)"));
        }
    }
    if dns.is_object() {
        if !out.is_empty() {
            out.push(String::new());
        }
        out.push("dns".to_string());
        out.push(format!("  search {}", dns["search"].as_str().unwrap_or("?")));
        for k in ["dns1", "dns2", "dns3"] {
            if let Some(s) = dns[k].as_str() {
                out.push(format!("  {k}   {s}"));
            }
        }
    }
    if let Some(d) = hosts["data"].as_str() {
        if !out.is_empty() {
            out.push(String::new());
        }
        out.push("hosts".to_string());
        out.extend(d.lines().map(|l| format!("  {l}")));
    }
    out
}

/// The clock, DNS and hosts file of this node (read-only `pvesh`).
pub fn node_settings() -> Vec<String> {
    let node = this_node();
    if node.is_empty() {
        return vec![];
    }
    let get = |p: &str| pvesh(&format!("/nodes/{node}/{p}"), &[]).unwrap_or_default();
    parse_node_settings(&get("time"), &get("dns"), &get("hosts"))
}

/// The lines of the update page from `/nodes/<n>/apt/update`: every package with a newer version waiting.
pub fn parse_apt(json: &str) -> Vec<String> {
    let v: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let Some(a) = v.as_array() else { return vec![] };
    let s = |x: &Value, k: &str| x[k].as_str().unwrap_or("?").to_string();
    let mut out = vec![format!("updates waiting ({})", a.len())];
    for x in a {
        out.push(format!("  {}  {} -> {}", s(x, "Package"), s(x, "OldVersion"), s(x, "Version")));
    }
    if a.is_empty() {
        out.push("  none; V asks the node to look again (apt update)".into());
    }
    out
}

/// This node's waiting updates (read-only `pvesh`; it shows the list from the last `apt update`).
pub fn apt_updates() -> Vec<String> {
    let node = this_node();
    if node.is_empty() {
        return vec![];
    }
    pvesh(&format!("/nodes/{node}/apt/update"), &[]).map(|j| parse_apt(&j)).unwrap_or_default()
}

/// How many packages wait for an upgrade, from `/nodes/<n>/apt/update` (`None` when the answer is not a list).
pub fn parse_apt_count(json: &str) -> Option<usize> {
    serde_json::from_str::<Value>(json).ok()?.as_array().map(|a| a.len())
}

/// The number of packages waiting on this node (read-only `pvesh`; the list from the last `apt update`).
pub fn apt_count() -> Option<usize> {
    let node = this_node();
    if node.is_empty() {
        return None;
    }
    pvesh(&format!("/nodes/{node}/apt/update"), &[]).and_then(|j| parse_apt_count(&j))
}

/// `apt-get dist-upgrade` on this machine without questions (the API has no upgrade call, so it is run directly; it keeps the
/// configuration files that were changed by hand).
pub fn apt_upgrade_cmd() -> Cmd {
    Cmd::new("env", &["DEBIAN_FRONTEND=noninteractive", "apt-get", "dist-upgrade", "-y", "-o", "Dpkg::Options::=--force-confold"])
}

/// `pvesh create /nodes/<n>/apt/update`: refreshes the package lists (an `apt update`); it answers with a task.
pub fn apt_refresh_cmd(node: &str) -> Cmd {
    let ok = !node.is_empty() && node.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    Cmd::new("pvesh", &["create", &format!("/nodes/{}/apt/update", if ok { node } else { "invalid" })])
}

/// The lines of the certificate page from `/nodes/<n>/certificates/info`: each certificate with its subject, issuer and how long
/// it still holds. Under 30 days is marked EXPIRES SOON, past is marked EXPIRED.
pub fn parse_certs(json: &str, now: u64) -> Vec<String> {
    let v: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let Some(a) = v.as_array() else { return vec![] };
    let s = |x: &Value, k: &str| x[k].as_str().unwrap_or("?").to_string();
    let mut out = vec![format!("certificates ({})", a.len())];
    for x in a {
        let left = match x["notafter"].as_u64() {
            Some(e) if e <= now => format!("EXPIRED {}d ago", (now - e).div_ceil(86400)),
            Some(e) if e - now < 30 * 86400 => format!("EXPIRES SOON: in {}d", (e - now).div_ceil(86400)),
            Some(e) => format!("valid for {}d", (e - now) / 86400),
            None => "no end date".into(),
        };
        out.push(format!("  {}  {left}", s(x, "filename")));
        out.push(format!("    subject {}", s(x, "subject")));
        out.push(format!("    issuer  {}", s(x, "issuer")));
        if let Some(san) = x["san"].as_array().filter(|l| !l.is_empty()) {
            out.push(format!("    names   {}", san.iter().filter_map(|n| n.as_str()).collect::<Vec<_>>().join(", ")));
        }
    }
    out
}

/// This node's certificates (read-only `pvesh`).
pub fn certificates(now: u64) -> Vec<String> {
    let node = this_node();
    if node.is_empty() {
        return vec![];
    }
    pvesh(&format!("/nodes/{node}/certificates/info"), &[]).map(|j| parse_certs(&j, now)).unwrap_or_default()
}

/// The names of the nodes in `/nodes`, without `local`, sorted.
pub fn parse_other_nodes(json: &str, local: &str) -> Vec<String> {
    let v: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let mut out: Vec<String> = v
        .as_array()
        .map(|a| a.iter().filter_map(|n| n["node"].as_str()).filter(|n| *n != local).map(String::from).collect())
        .unwrap_or_default();
    out.sort();
    out
}

/// A guest whose newest backup is older than this many days is marked.
pub const BACKUP_STALE_DAYS: u64 = 7;

/// The backup page: every guest with the age of its newest backup in any storage, the ones with none or a stale one first.
/// `guests` are (id, name); `now` is the clock in seconds.
pub fn backup_coverage(guests: &[(u32, String)], backups: &[Backup], now: u64) -> Vec<String> {
    let mut rows: Vec<(Option<u64>, u32, &str)> = guests
        .iter()
        .map(|(id, name)| (backups.iter().filter(|b| b.vmid == *id).map(|b| now.saturating_sub(b.ctime) / 86400).min(), *id, name.as_str()))
        .collect();
    // Never backed up first, then the oldest.
    rows.sort_by_key(|(age, id, _)| (std::cmp::Reverse(age.unwrap_or(u64::MAX)), *id));
    let stale = rows.iter().filter(|(a, _, _)| a.is_none_or(|a| a > BACKUP_STALE_DAYS)).count();
    let mut out =
        vec![format!("backups of {} guests: {} without a backup newer than {} days", rows.len(), stale, BACKUP_STALE_DAYS), String::new()];
    for (age, id, name) in rows {
        let (mark, what) = match age {
            None => ("!", "never".to_string()),
            Some(a) if a > BACKUP_STALE_DAYS => ("!", format!("{a} days ago")),
            Some(0) => (" ", "today".to_string()),
            Some(a) => (" ", format!("{a} days ago")),
        };
        out.push(format!(" {mark} {id:>5}  {name:<24} {what}"));
    }
    out
}

/// The backup page for `guests`, from the backup files on `storages` (node, name, type). A storage every node sees (NFS, PBS…) is
/// listed once. Slow: call it from a thread of its own.
pub fn backup_status(guests: &[(u32, String)], storages: &[(String, String, String)]) -> Vec<String> {
    let local = |kind: &str| matches!(kind, "dir" | "zfspool" | "lvm" | "lvmthin" | "btrfs" | "zfs");
    let mut seen: Vec<&str> = vec![];
    let mut all = vec![];
    for (node, name, kind) in storages {
        if !local(kind) {
            if seen.contains(&name.as_str()) {
                continue;
            }
            seen.push(name);
        }
        all.extend(backups(node, name));
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut out = backup_coverage(guests, &all, now);
    out.push(String::new());
    out.push(format!(
        "looked at {} storages; a guest with no backup file there shows as never (a PBS group removed or pruned too)",
        storages.len()
    ));
    out
}

/// The SDN page: each zone with its VNets and their subnets. `vnet_subnets` holds the `subnets` JSON of each VNet by name.
pub fn sdn_report(zones: &str, vnets: &str, vnet_subnets: &[(String, String)]) -> Vec<String> {
    let list = |j: &str| serde_json::from_str::<Value>(j).ok().and_then(|v| v.as_array().cloned()).unwrap_or_default();
    let s = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_string();
    let (zones, vnets) = (list(zones), list(vnets));
    if zones.is_empty() && vnets.is_empty() {
        return vec!["SDN".into(), "  no zones: the software-defined network is not set up on this cluster".into()];
    }
    let mut out = vec![format!("SDN: {} zones, {} VNets", zones.len(), vnets.len())];
    for z in &zones {
        let name = s(z, "zone");
        let nodes = s(z, "nodes");
        out.push(String::new());
        out.push(format!(
            "zone {name}  type {}{}",
            s(z, "type"),
            if nodes.is_empty() { String::new() } else { format!("  nodes {nodes}") }
        ));
        for v in vnets.iter().filter(|v| s(v, "zone") == name) {
            let vn = s(v, "vnet");
            let tag = v["tag"].as_u64().map(|t| format!("  vlan {t}")).unwrap_or_default();
            let alias = s(v, "alias");
            out.push(format!("  vnet {vn}{tag}{}", if alias.is_empty() { String::new() } else { format!("  ({alias})") }));
            let subs = vnet_subnets.iter().find(|(n, _)| *n == vn).map(|(_, j)| list(j)).unwrap_or_default();
            for sub in subs {
                let gw = s(&sub, "gateway");
                let snat = if sub["snat"].as_u64() == Some(1) { "  snat" } else { "" };
                out.push(format!(
                    "    subnet {}{}{snat}",
                    s(&sub, "cidr"),
                    if gw.is_empty() { String::new() } else { format!("  gateway {gw}") }
                ));
            }
        }
    }
    let orphans: Vec<String> = vnets.iter().filter(|v| !zones.iter().any(|z| s(z, "zone") == s(v, "zone"))).map(|v| s(v, "vnet")).collect();
    if !orphans.is_empty() {
        out.push(String::new());
        out.push(format!("VNets in no listed zone: {}", orphans.join(", ")));
    }
    out
}

/// The SDN page for this cluster (read-only `pvesh`; the answer is empty text when SDN is not set up).
pub fn sdn_status() -> Vec<String> {
    let zones = pvesh("/cluster/sdn/zones", &[]).unwrap_or_default();
    let vnets = pvesh("/cluster/sdn/vnets", &[]).unwrap_or_default();
    let names: Vec<String> = serde_json::from_str::<Value>(&vnets)
        .ok()
        .and_then(|v| v.as_array().map(|a| a.iter().filter_map(|v| v["vnet"].as_str().map(str::to_string)).collect()))
        .unwrap_or_default();
    let subs: Vec<(String, String)> = names
        .into_iter()
        .map(|n| {
            let j = pvesh(&format!("/cluster/sdn/vnets/{n}/subnets"), &[]).unwrap_or_default();
            (n, j)
        })
        .collect();
    sdn_report(&zones, &vnets, &subs)
}

/// The lines of the ssh page: one per other node, `ok` or why the key login failed.
pub fn ssh_report(results: &[(String, Result<(), String>)]) -> Vec<String> {
    if results.is_empty() {
        return vec!["ssh to the other nodes".into(), "  this is the only node: nothing is run over ssh".into()];
    }
    let bad = results.iter().filter(|(_, r)| r.is_err()).count();
    let mut out = vec![format!("ssh to the other nodes ({} nodes, {bad} not reachable)", results.len())];
    for (n, r) in results {
        out.push(match r {
            Ok(()) => format!("  {n}  ok"),
            Err(e) => format!("  {n}  CANNOT LOG IN: {e}"),
        });
    }
    if bad > 0 {
        out.push(String::new());
        out.push("guest operations on those nodes fail until `ssh root@<node>` works without a password".into());
        out.push("(on this node: ssh-copy-id root@<node>, or run `pvecm updatecerts` to refresh the cluster keys)".into());
    }
    out
}

/// The command that tries a key login to `node`.
pub fn ssh_probe_cmd(node: &str) -> Option<Cmd> {
    let ok = !node.is_empty() && node.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'));
    ok.then(|| Cmd::new("ssh", &["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", &format!("root@{node}"), "true"]))
}

/// Try `ssh root@<node> true` for every other node of the cluster.
pub fn ssh_check() -> Vec<String> {
    let local = this_node();
    let nodes = pvesh("/nodes", &[]).map(|j| parse_other_nodes(&j, &local)).unwrap_or_default();
    let results: Vec<(String, Result<(), String>)> = nodes
        .into_iter()
        .map(|n| {
            let r = match ssh_probe_cmd(&n) {
                Some(c) => run(&c.prog, &c.args),
                None => Err("bad node name".into()),
            };
            (n, r)
        })
        .collect();
    ssh_report(&results)
}

/// The lines of the disk page from `/nodes/<n>/disks/list`; `smart` answers `/nodes/<n>/disks/smart` for a device path. Each disk
/// shows its model, size, kind, health and wearout, then the SMART values that matter (ATA attributes or NVMe log lines).
/// A health other than PASSED/OK/UNKNOWN is marked UNHEALTHY.
pub fn parse_disks(list: &str, smart: &dyn Fn(&str) -> String) -> Vec<String> {
    let v: Value = serde_json::from_str(list).unwrap_or(Value::Null);
    let Some(a) = v.as_array() else { return vec![] };
    let s = |x: &Value, k: &str| x[k].as_str().unwrap_or("").trim().to_string();
    let mut out = vec![format!("disks ({})", a.len())];
    for x in a {
        let dev = s(x, "devpath");
        let health = s(x, "health");
        let bad = !matches!(health.as_str(), "" | "PASSED" | "OK" | "UNKNOWN");
        let wear = match x["wearout"].as_u64() {
            Some(w) => format!("  wearout {w}% left"),
            None => String::new(),
        };
        out.push(format!(
            "  {dev}  {}  {}  {}  {}{wear}{}",
            s(x, "model"),
            x["size"].as_u64().map_or("?".to_string(), human),
            s(x, "type"),
            if health.is_empty() { "?" } else { &health },
            if bad { "  UNHEALTHY" } else { "" }
        ));
        let serial = s(x, "serial");
        if !serial.is_empty() {
            out.push(format!("    serial {serial}"));
        }
        let safe =
            dev.strip_prefix("/dev/").is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_alphanumeric() || "/_-".contains(c)));
        if !safe {
            continue;
        }
        let d: Value = serde_json::from_str(&smart(&dev)).unwrap_or(Value::Null);
        if let Some(attrs) = d["attributes"].as_array() {
            let keep = [5u64, 9, 187, 188, 194, 196, 197, 198];
            let shown: Vec<String> = attrs
                .iter()
                .filter(|t| {
                    let id = t["id"].as_u64().or_else(|| t["id"].as_str().and_then(|i| i.trim().parse().ok()));
                    id.is_some_and(|i| keep.contains(&i))
                })
                .map(|t| format!("{} {}", t["name"].as_str().unwrap_or("?"), t["raw"].as_str().unwrap_or("?").trim()))
                .collect();
            if !shown.is_empty() {
                out.push(format!("    {}", shown.join(", ")));
            }
        } else if let Some(t) = d["text"].as_str() {
            for l in t.lines() {
                let l = l.trim();
                if [
                    "Percentage Used",
                    "Temperature:",
                    "Media and Data Integrity Errors",
                    "Power On Hours",
                    "Critical Warning",
                    "Available Spare:",
                ]
                .iter()
                .any(|k| l.starts_with(k))
                {
                    out.push(format!("    {l}"));
                }
            }
        }
    }
    out
}

/// This node's disks with their SMART health (read-only `pvesh`; one `smart` call per disk).
pub fn node_disks() -> Vec<String> {
    let node = this_node();
    if node.is_empty() {
        return vec![];
    }
    let Some(list) = pvesh(&format!("/nodes/{node}/disks/list"), &[]) else { return vec![] };
    parse_disks(&list, &|dev| pvesh(&format!("/nodes/{node}/disks/smart"), &["--disk", dev]).unwrap_or_default())
}

/// The lines of the network page from `/nodes/<n>/network`: each bridge, bond, VLAN and port with its address, gateway and members.
/// An interface that is set to start but is not active is marked DOWN.
pub fn parse_network(json: &str) -> Vec<String> {
    let v: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let Some(a) = v.as_array() else { return vec![] };
    let s = |x: &Value, k: &str| x[k].as_str().unwrap_or("").trim().to_string();
    let mut rows: Vec<&Value> = a.iter().filter(|x| s(x, "type") != "unknown" || x["iface"].is_string()).collect();
    rows.sort_by_key(|x| s(x, "iface"));
    let mut out = vec![format!("network interfaces ({})", rows.len())];
    for x in rows {
        let addr = match (s(x, "cidr"), s(x, "address")) {
            (c, _) if !c.is_empty() => c,
            (_, a) if !a.is_empty() => a,
            _ => "no address".to_string(),
        };
        let down = x["autostart"].as_u64() == Some(1) && x["active"].as_u64() != Some(1);
        let gw = s(x, "gateway");
        out.push(format!(
            "  {:<10} {:<10} {addr}{}{}",
            s(x, "iface"),
            s(x, "type"),
            if gw.is_empty() { String::new() } else { format!("  gw {gw}") },
            if down { "  DOWN" } else { "" }
        ));
        for (k, label) in [("bridge_ports", "ports"), ("slaves", "slaves"), ("bond_slaves", "slaves"), ("bond_mode", "mode")] {
            let m = s(x, k);
            if !m.is_empty() {
                out.push(format!("             {label} {m}"));
            }
        }
        if let Some(id) = x["vlan-id"].as_u64() {
            out.push(format!("             vlan {id} on {}", s(x, "vlan-raw-device")));
        }
        let c = s(x, "comments");
        if !c.is_empty() {
            out.push(format!("             # {}", c.replace('\n', " ")));
        }
    }
    out
}

/// This node's network interfaces (read-only `pvesh`).
pub fn network() -> Vec<String> {
    let node = this_node();
    if node.is_empty() {
        return vec![];
    }
    pvesh(&format!("/nodes/{node}/network"), &[]).map(|j| parse_network(&j)).unwrap_or_default()
}

/// The file name for a saved log: `yagura-<label>-<secs>.log`, the label cut down to letters, digits and `-`.
pub fn export_name(label: &str, secs: u64) -> String {
    let l: String = label.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    format!("yagura-{}-{secs}.log", l.trim_matches('-'))
}

/// Write `lines` to a new file in `dir` (never over an existing one); returns the path.
pub fn export_lines(dir: &std::path::Path, label: &str, secs: u64, lines: &[String]) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    let path = dir.join(export_name(label, secs));
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
    for l in lines {
        writeln!(f, "{l}")?;
    }
    Ok(path)
}

/// Delete a volume (`pvesm free`; Proxmox refuses a protected backup and a disk that is in use).
pub fn vol_delete_cmd(v: &Vol) -> Cmd {
    Cmd::new("pvesm", &["free", &v.volid])
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

/// The tags and the note of a guest's configuration, as the form shows them: tags joined with `;`, a note of several lines on one
/// line (` / ` between the lines).
pub fn notes_fields(cfg: &[(String, String)]) -> [String; 2] {
    let get = |k: &str| cfg.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone()).unwrap_or_default();
    let tags = get("tags").split([';', ',', ' ']).filter(|t| !t.is_empty()).collect::<Vec<_>>().join(";");
    let note = get("description").lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" / ");
    [tags, note]
}

/// The tags written the way Proxmox keeps them: split on `;` `,` and blanks, each a word (letters, digits, `_`, and `-+.` after the
/// first), lower-cased, without repeats, joined with `;`.
pub fn clean_tags(s: &str) -> Result<String, String> {
    let mut out: Vec<String> = vec![];
    for t in s.split([';', ',', ' ']).filter(|t| !t.is_empty()) {
        let ok = t.len() <= 32
            && t.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
            && t.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+' | b'.'));
        if !ok {
            return Err(format!("tag {t}: letters, digits and _ - + . only, up to 32"));
        }
        let t = t.to_ascii_lowercase();
        if !out.contains(&t) {
            out.push(t);
        }
    }
    Ok(out.join(";"))
}

/// `qm set` / `pct set` for the tags and the note that differ from `orig` (an emptied one is deleted).
pub fn notes_cmd(kind: Kind, id: u32, v: &[String; 2], orig: &[String; 2]) -> Result<Cmd, String> {
    let tags = clean_tags(&v[0])?;
    let note = v[1].trim();
    if note.chars().any(char::is_control) || note.chars().count() > 200 {
        return Err("the note is one line of up to 200 characters".into());
    }
    if note.starts_with('-') {
        return Err("the note may not start with -".into());
    }
    let mut args = vec!["set".to_string(), id.to_string()];
    let mut gone = vec![];
    if tags != clean_tags(&orig[0]).unwrap_or_default() {
        if tags.is_empty() {
            gone.push("tags");
        } else {
            args.extend(["--tags".to_string(), tags]);
        }
    }
    if note != orig[1].trim() {
        if note.is_empty() {
            gone.push("description");
        } else {
            args.extend(["--description".to_string(), note.to_string()]);
        }
    }
    if !gone.is_empty() {
        args.extend(["--delete".to_string(), gone.join(",")]);
    }
    if args.len() == 2 {
        return Err("nothing changed".into());
    }
    let prog = if kind == Kind::Lxc { "pct" } else { "qm" };
    Ok(Cmd { prog: prog.into(), args })
}

/// A name Proxmox takes for a new guest (a DNS-like name: letters, digits, `-` and `.`; starts with a letter or digit).
pub fn valid_guest_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// `qm clone` / `pct clone`: a copy of guest `id` as `newid`. Only a template can be cloned linked (`full` false).
pub fn migrate_cmd(kind: Kind, id: u32, target: &str, running: bool, online: bool) -> Result<Cmd, String> {
    if target.is_empty() || !target.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') {
        return Err("the target node name is not valid".into());
    }
    let id = id.to_string();
    let mut args = vec!["migrate", id.as_str(), target];
    if running {
        // A VM can move while it runs; a container is stopped and started again on the target.
        args.extend(if kind == Kind::Lxc {
            ["--restart", "1"]
        } else if online {
            ["--online", "1"]
        } else {
            ["--online", "0"]
        });
    }
    Ok(Cmd::new(if kind == Kind::Lxc { "pct" } else { "qm" }, &args))
}

pub fn clone_cmd(kind: Kind, id: u32, newid: u32, name: &str, full: bool) -> Cmd {
    let (prog, name_opt) = if kind == Kind::Lxc { ("pct", "--hostname") } else { ("qm", "--name") };
    let (id, newid) = (id.to_string(), newid.to_string());
    let mut args = vec!["clone", id.as_str(), newid.as_str(), name_opt, name];
    if full {
        args.extend(["--full", "1"]);
    }
    Cmd::new(prog, &args)
}

/// The network devices in a guest's configuration as (key, value), e.g. `("net0", "virtio=BC:24:11:00:00:01,bridge=vmbr0,tag=10")`.
pub fn nets(cfg: &[(String, String)]) -> Vec<(String, String)> {
    cfg.iter()
        .filter(|(k, _)| k.strip_prefix("net").is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit())))
        .cloned()
        .collect()
}

/// The (bridge, VLAN tag, MAC) of a network device value; a VM keeps its MAC after the model (`virtio=MAC`), a container in `hwaddr=`.
pub fn net_fields(kind: Kind, value: &str) -> (String, String, String) {
    let (mut bridge, mut vlan, mut mac) = (String::new(), String::new(), String::new());
    for (i, part) in value.split(',').enumerate() {
        match part.split_once('=') {
            Some(("bridge", v)) => bridge = v.to_string(),
            Some(("tag", v)) => vlan = v.to_string(),
            Some(("hwaddr", v)) if kind == Kind::Lxc => mac = v.to_string(),
            Some((_, v)) if i == 0 && kind == Kind::Qemu => mac = v.to_string(),
            _ => {}
        }
    }
    (bridge, vlan, mac)
}

/// `aa:bb:cc:dd:ee:ff` (hex, either case).
pub fn valid_mac(s: &str) -> bool {
    let p: Vec<&str> = s.split(':').collect();
    p.len() == 6 && p.iter().all(|x| x.len() == 2 && x.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The device value with a new bridge, VLAN tag (empty: no tag) and MAC (empty: unchanged); the other options stay as they are.
pub fn net_apply(kind: Kind, value: &str, bridge: &str, vlan: &str, mac: &str) -> String {
    let mut out: Vec<String> = vec![];
    let (mut saw_bridge, mut saw_tag) = (false, false);
    for (i, part) in value.split(',').enumerate() {
        match part.split_once('=') {
            Some(("bridge", _)) if !bridge.is_empty() => {
                saw_bridge = true;
                out.push(format!("bridge={bridge}"));
            }
            Some(("tag", _)) => {
                saw_tag = true;
                if !vlan.is_empty() {
                    out.push(format!("tag={vlan}"));
                }
            }
            Some(("hwaddr", _)) if kind == Kind::Lxc && !mac.is_empty() => out.push(format!("hwaddr={mac}")),
            Some((model, _)) if i == 0 && kind == Kind::Qemu && !mac.is_empty() => out.push(format!("{model}={mac}")),
            _ => {
                if part.starts_with("bridge=") {
                    saw_bridge = true;
                }
                out.push(part.to_string());
            }
        }
    }
    if !saw_bridge && !bridge.is_empty() {
        out.push(format!("bridge={bridge}"));
    }
    if !saw_tag && !vlan.is_empty() {
        out.push(format!("tag={vlan}"));
    }
    out.join(",")
}

/// `qm set` / `pct set` with a new value for one network device.
pub fn net_cmd(kind: Kind, id: u32, dev: &str, value: &str) -> Cmd {
    Cmd::new(if kind == Kind::Lxc { "pct" } else { "qm" }, &["set", &id.to_string(), &format!("--{dev}"), value])
}

/// One rule of a guest's firewall, as a line to show.
#[derive(Clone, Debug, PartialEq)]
pub struct FwRule {
    pub pos: u32,
    pub enabled: bool,
    pub line: String,
}

/// Whether the guest's firewall is on, from the options (`enable` is absent when it was never set).
pub fn parse_fw_options(json: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return false };
    match &v["enable"] {
        Value::Number(n) => n.as_u64().unwrap_or(0) != 0,
        Value::String(s) => s == "1",
        Value::Bool(b) => *b,
        _ => false,
    }
}

/// The rules of a guest's firewall in order, each as `in ACCEPT tcp dport 22 from 10.0.0.0/24 # comment`.
pub fn parse_fw_rules(json: &str) -> Vec<FwRule> {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return vec![] };
    let mut out: Vec<FwRule> = v
        .as_array()
        .map(|l| {
            l.iter()
                .filter_map(|r| {
                    let s = |k: &str| r[k].as_str().filter(|x| !x.is_empty());
                    let mut line = format!("{} {}", s("type")?, s("action")?);
                    if let Some(m) = s("macro") {
                        line += &format!(" {m}");
                    } else if let Some(p) = s("proto") {
                        line += &format!(" {p}");
                    }
                    for (key, word) in [("dport", "dport"), ("sport", "sport"), ("source", "from"), ("dest", "to"), ("iface", "on")] {
                        if let Some(x) = s(key) {
                            line += &format!(" {word} {x}");
                        }
                    }
                    if let Some(c) = s("comment") {
                        line += &format!(" # {c}");
                    }
                    Some(FwRule { pos: r["pos"].as_u64().unwrap_or(0) as u32, enabled: r["enable"].as_u64().unwrap_or(0) != 0, line })
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by_key(|r| r.pos);
    out
}

/// The lines for one firewall level (`cluster` or a node): whether it is on, the default policies, then its rules in order.
pub fn parse_fw_level(scope: &str, options: &str, rules: &str) -> Vec<String> {
    let o: Value = serde_json::from_str(options).unwrap_or(Value::Null);
    let on = parse_fw_options(options);
    let pol = |k: &str| o[k].as_str().unwrap_or("default").to_string();
    let mut out =
        vec![format!("{scope} firewall {}  policy in {}, out {}", if on { "ON" } else { "OFF" }, pol("policy_in"), pol("policy_out"))];
    let r = parse_fw_rules(rules);
    if r.is_empty() {
        out.push("  no rules".into());
    }
    for x in r {
        out.push(format!("  {:>2}  {}{}", x.pos, if x.enabled { "" } else { "off  " }, x.line));
    }
    out
}

/// The cluster firewall and this node's firewall with their rules (read-only `pvesh`).
pub fn host_firewall() -> Vec<String> {
    let node = this_node();
    let get = |p: &str| pvesh(p, &[]).unwrap_or_default();
    let mut out = parse_fw_level("cluster", &get("/cluster/firewall/options"), &get("/cluster/firewall/rules"));
    if !node.is_empty() {
        out.push(String::new());
        out.extend(parse_fw_level(
            &format!("node {node}"),
            &get(&format!("/nodes/{node}/firewall/options")),
            &get(&format!("/nodes/{node}/firewall/rules")),
        ));
    }
    out
}

fn fw_path(node: &str, kind: Kind, id: u32) -> String {
    format!("/nodes/{node}/{}/{id}/firewall", if kind == Kind::Lxc { "lxc" } else { "qemu" })
}

/// Whether the firewall of a guest is on and its rules (read-only `pvesh`).
pub fn firewall(node: &str, kind: Kind, id: u32) -> Option<(bool, Vec<FwRule>)> {
    let p = fw_path(node, kind, id);
    let on = parse_fw_options(&pvesh(&format!("{p}/options"), &[])?);
    let rules = pvesh(&format!("{p}/rules"), &[]).map(|j| parse_fw_rules(&j)).unwrap_or_default();
    Some((on, rules))
}

/// `pvesh set .../firewall/options --enable 0|1`.
pub fn firewall_cmd(node: &str, kind: Kind, id: u32, on: bool) -> Cmd {
    Cmd::new("pvesh", &["set", &format!("{}/options", fw_path(node, kind, id)), "--enable", if on { "1" } else { "0" }])
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

/// The first `<prefix>N` (N from 0 to `max`) that no configuration key uses.
fn free_key(keys: &[String], prefix: &str, max: u32) -> Option<String> {
    (0..=max).map(|n| format!("{prefix}{n}")).find(|k| !keys.contains(k))
}

/// A new network device: the first free `netN` and its value (a VM gets a virtio card, a container `ethN` with DHCP), and the command.
/// `keys` are all keys of the guest's configuration; an empty `bridge` gives `None`.
pub fn net_add_cmd(kind: Kind, id: u32, keys: &[String], bridge: &str, vlan: &str) -> Option<(String, String, Cmd)> {
    if bridge.is_empty() {
        return None;
    }
    let dev = free_key(keys, "net", if kind == Kind::Lxc { 9 } else { 31 })?;
    let n = dev.trim_start_matches("net");
    let mut value = if kind == Kind::Lxc { format!("name=eth{n},bridge={bridge},ip=dhcp") } else { format!("virtio,bridge={bridge}") };
    if !vlan.is_empty() {
        value.push_str(&format!(",tag={vlan}"));
    }
    let cmd = net_cmd(kind, id, &dev, &value);
    Some((dev, value, cmd))
}

/// `qm set --delete netN` / `pct set --delete netN`.
pub fn net_del_cmd(kind: Kind, id: u32, dev: &str) -> Cmd {
    Cmd::new(if kind == Kind::Lxc { "pct" } else { "qm" }, &["set", &id.to_string(), "--delete", dev])
}

/// The labels of the cloud-init fields, in the order of `cloudinit_fields`.
pub const CLOUDINIT_LABELS: [&str; 5] =
    ["user", "IP (dhcp, or address/prefix [gateway])", "DNS servers", "search domain", "SSH keys file (on the node)"];

/// The cloud-init fields of a VM's configuration: user, IP, DNS servers, search domain and the SSH keys file (always empty: the keys
/// are stored encoded, so an empty field means "leave them").
pub fn cloudinit_fields(cfg: &[(String, String)]) -> [String; 5] {
    let get = |k: &str| cfg.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone()).unwrap_or_default();
    let ip =
        get("ipconfig0").split(',').filter_map(|p| p.strip_prefix("ip=").or_else(|| p.strip_prefix("gw="))).collect::<Vec<_>>().join(" ");
    [get("ciuser"), ip, get("nameserver"), get("searchdomain"), String::new()]
}

/// `qm set` for the cloud-init fields that are not empty. A password is not offered: `qm` takes it only as an argument, which any
/// user of the machine can read in `ps`.
pub fn cloudinit_cmd(id: u32, f: &[String; 5]) -> Result<Cmd, String> {
    let word = |s: &str, extra: &[char]| s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') || extra.contains(&c));
    let mut args = vec!["set".to_string(), id.to_string()];
    let mut put = |k: &str, v: String| {
        args.push(format!("--{k}"));
        args.push(v);
    };
    let user = f[0].trim();
    if !user.is_empty() {
        if !word(user, &[]) || user.starts_with('-') {
            return Err("the user may hold letters, digits, - _ . only".into());
        }
        put("ciuser", user.to_string());
    }
    let ip: Vec<&str> = f[1].split_whitespace().collect();
    match ip.as_slice() {
        [] => {}
        ["dhcp"] => put("ipconfig0", "ip=dhcp".into()),
        [addr] | [addr, _] if addr.contains('/') && word(addr, &[':', '/']) && ip.get(1).is_none_or(|g| word(g, &[':'])) => {
            put("ipconfig0", format!("ip={addr}{}", ip.get(1).map(|g| format!(",gw={g}")).unwrap_or_default()))
        }
        _ => return Err("the IP is dhcp, or address/prefix and an optional gateway".into()),
    }
    let dns: Vec<&str> = f[2].split_whitespace().collect();
    if !dns.is_empty() {
        if !dns.iter().all(|d| word(d, &[':'])) {
            return Err("the DNS servers are addresses separated by spaces".into());
        }
        put("nameserver", dns.join(" "));
    }
    let domain = f[3].trim();
    if !domain.is_empty() {
        if !word(domain, &[]) {
            return Err("the search domain may hold letters, digits, - _ . only".into());
        }
        put("searchdomain", domain.to_string());
    }
    let keys = f[4].trim();
    if !keys.is_empty() {
        if !keys.starts_with('/') || !word(keys, &['/']) || keys.contains("..") {
            return Err("the SSH keys file is an absolute path on the node".into());
        }
        put("sshkeys", keys.to_string());
    }
    if args.len() == 2 {
        return Err("nothing to change".into());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    Ok(Cmd::new("qm", &refs))
}

/// The hardware a VM can be given from the hardware dialog.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hw {
    Usb,
    Pci,
    Serial,
    Tpm,
    Efi,
}

impl Hw {
    pub const ALL: [Hw; 5] = [Hw::Usb, Hw::Pci, Hw::Serial, Hw::Tpm, Hw::Efi];

    /// What the row says, with the kind of value it wants.
    pub fn label(self) -> &'static str {
        match self {
            Hw::Usb => "USB device (vendor:product, e.g. 046d:c52b)",
            Hw::Pci => "PCI passthrough (address, e.g. 0000:01:00.0)",
            Hw::Serial => "serial port (a socket)",
            Hw::Tpm => "TPM 2.0 state (storage)",
            Hw::Efi => "EFI disk (storage; the BIOS must be OVMF)",
        }
    }

    /// What the value starts as when the row is picked.
    pub fn default_arg(self) -> &'static str {
        match self {
            Hw::Tpm | Hw::Efi => "local-lvm",
            _ => "",
        }
    }

    pub fn wants_value(self) -> bool {
        self != Hw::Serial
    }
}

/// Add hardware to a VM: the key it takes (the first free one), its value and the `qm set` command. `keys` are all keys of the
/// configuration; `None` when the value is wrong or there is no free slot (the TPM state and the EFI disk exist once).
pub fn hardware_add_cmd(id: u32, keys: &[String], hw: Hw, arg: &str) -> Option<(String, String, Cmd)> {
    let hex = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit());
    let storage = !arg.is_empty() && !arg.contains([',', ':', ' ', '/']);
    let (key, value) = match hw {
        Hw::Usb => {
            let (v, p) = arg.split_once(':')?;
            (
                free_key(keys, "usb", 4)?,
                format!("host={}", if v.len() == 4 && p.len() == 4 && hex(v) && hex(p) { arg } else { return None }),
            )
        }
        Hw::Pci => {
            let ok = arg.contains('.') && arg.contains(':') && arg.split([':', '.']).all(hex) && arg.len() <= 12;
            (free_key(keys, "hostpci", 15)?, if ok { arg.to_string() } else { return None })
        }
        Hw::Serial => (free_key(keys, "serial", 3)?, "socket".to_string()),
        Hw::Tpm if storage && !keys.iter().any(|k| k == "tpmstate0") => ("tpmstate0".to_string(), format!("{arg}:1,version=v2.0")),
        Hw::Efi if storage && !keys.iter().any(|k| k == "efidisk0") => {
            ("efidisk0".to_string(), format!("{arg}:1,efitype=4m,pre-enrolled-keys=1"))
        }
        _ => return None,
    };
    let cmd = Cmd::new("qm", &["set", &id.to_string(), &format!("--{key}"), &value]);
    Some((key, value, cmd))
}

/// The container templates `pveam` offers, from `pvesh get /nodes/<n>/aplinfo`: the template names, sorted.
pub fn parse_aplinfo(json: &str) -> Vec<String> {
    let v: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let mut out: Vec<String> = v.as_array().into_iter().flatten().filter_map(|x| x["template"].as_str()).map(str::to_string).collect();
    out.sort();
    out.dedup();
    out
}

/// The container templates this node can download (empty when the list cannot be read).
pub fn available_templates() -> Vec<String> {
    let node = this_node();
    if node.is_empty() {
        return vec![];
    }
    pvesh(&format!("/nodes/{node}/aplinfo"), &[]).map(|j| parse_aplinfo(&j)).unwrap_or_default()
}

/// `pveam download <storage> <template>`.
pub fn template_download_cmd(storage: &str, template: &str) -> Option<Cmd> {
    let ok = |s: &str, extra: &[char]| {
        !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') || extra.contains(&c))
    };
    (ok(storage, &[]) && ok(template, &['+', '~'])).then(|| Cmd::new("pveam", &["download", storage, template]))
}

/// A guest to create: what the form of `a` collects.
#[derive(Clone, Debug, PartialEq)]
pub struct NewGuest {
    pub kind: Kind,
    pub id: u32,
    pub name: String,
    pub cores: u32,
    pub mem_mib: u32,
    pub disk_gib: u32,
    pub storage: String,
    pub bridge: String,
    /// A VM: the ISO volume to boot from (may be empty). A container: the template volume (required).
    pub source: String,
}

/// `qm create` / `pct create`. A container gets no password (one on the command line shows in `ps`): enter it with `pct enter`.
pub fn create_cmd(g: &NewGuest) -> Result<Cmd, String> {
    let bad = |s: &str| s.is_empty() || s.contains([',', ':', ' ', '/']);
    if !valid_guest_name(&g.name) {
        return Err("the name needs letters, digits, - and .".into());
    }
    if !(1..=512).contains(&g.cores) {
        return Err("cores must be 1 to 512".into());
    }
    if !(16..=4_194_304).contains(&g.mem_mib) {
        return Err("memory must be 16 MiB or more".into());
    }
    if !(1..=65536).contains(&g.disk_gib) {
        return Err("the disk must be 1 to 65536 GiB".into());
    }
    if bad(&g.storage) || bad(&g.bridge) {
        return Err("the storage and the bridge need a name".into());
    }
    let src = !g.source.is_empty();
    if src && (!g.source.contains(':') || g.source.contains([',', ' '])) {
        return Err("the volume looks like storage:iso/name.iso".into());
    }
    let (id, cores, mem) = (g.id.to_string(), g.cores.to_string(), g.mem_mib.to_string());
    let disk = format!("{}:{}", g.storage, g.disk_gib);
    if g.kind == Kind::Lxc {
        if !src || !g.source.contains("vztmpl") {
            return Err("a container needs a template such as local:vztmpl/debian-12-standard.tar.zst".into());
        }
        let net = format!("name=eth0,bridge={},ip=dhcp", g.bridge);
        return Ok(Cmd::new(
            "pct",
            &[
                "create",
                &id,
                &g.source,
                "--hostname",
                &g.name,
                "--cores",
                &cores,
                "--memory",
                &mem,
                "--rootfs",
                &disk,
                "--net0",
                &net,
                "--unprivileged",
                "1",
            ],
        ));
    }
    let net = format!("virtio,bridge={}", g.bridge);
    let mut args = vec![
        "create",
        &id,
        "--name",
        &g.name,
        "--cores",
        &cores,
        "--memory",
        &mem,
        "--net0",
        &net,
        "--scsihw",
        "virtio-scsi-single",
        "--scsi0",
        &disk,
        "--ostype",
        "l26",
    ];
    if src {
        args.extend(["--cdrom", &g.source]);
    }
    Ok(Cmd::new("qm", &args))
}

/// Puts the ISO `volid` (`local:iso/debian.iso`) into the CD-ROM drive `key` of a VM, or ejects it when `volid` is `none`.
pub fn cdrom_set_cmd(id: u32, key: &str, volid: &str) -> Option<Cmd> {
    let drive =
        ["ide", "sata", "scsi"].iter().any(|p| key.strip_prefix(p).is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())));
    if !drive || volid.is_empty() || volid.contains([',', ' ']) {
        return None;
    }
    Some(Cmd::new("qm", &["set", &id.to_string(), &format!("--{key}"), &format!("{volid},media=cdrom")]))
}

/// A new empty CD-ROM drive on the first free `ideN` (0 to 3) of a VM: its key and the command.
pub fn cdrom_add_cmd(id: u32, keys: &[String]) -> Option<(String, Cmd)> {
    let dev = free_key(keys, "ide", 3)?;
    let cmd = cdrom_set_cmd(id, &dev, "none")?;
    Some((dev, cmd))
}

/// A new disk of `gib` GiB on `storage`: the first free `scsiN` (a VM) or `mpN` mounted at `/mnt/diskN` (a container), its value
/// and the command. `keys` are all keys of the guest's configuration (so a CD-ROM or cloud-init drive is never replaced).
pub fn disk_add_cmd(kind: Kind, id: u32, keys: &[String], storage: &str, gib: u32) -> Option<(String, String, Cmd)> {
    if storage.is_empty() || storage.contains([',', ':', ' ']) || gib == 0 {
        return None;
    }
    let (dev, value) = if kind == Kind::Lxc {
        let dev = free_key(keys, "mp", 255)?;
        let n = dev.trim_start_matches("mp");
        let value = format!("{storage}:{gib},mp=/mnt/disk{n}");
        (dev, value)
    } else {
        (free_key(keys, "scsi", 30)?, format!("{storage}:{gib}"))
    };
    let cmd = Cmd::new(if kind == Kind::Lxc { "pct" } else { "qm" }, &["set", &id.to_string(), &format!("--{dev}"), &value]);
    Some((dev, value, cmd))
}

/// Every drive in a guest's configuration as (key, value): disks, CD-ROMs, cloud-init drives, `efidisk0`, `tpmstate0` and the
/// `unusedN` volumes that a detached disk leaves behind; a container's `rootfs` and `mpN`.
pub fn drives(kind: Kind, cfg: &[(String, String)]) -> Vec<(String, String)> {
    let digits = |k: &str, p: &str| k.strip_prefix(p).is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()));
    cfg.iter()
        .filter(|(k, _)| {
            if kind == Kind::Lxc {
                k == "rootfs" || digits(k, "mp") || digits(k, "unused")
            } else {
                ["scsi", "virtio", "sata", "ide", "unused"].iter().any(|p| digits(k, p)) || k == "efidisk0" || k == "tpmstate0"
            }
        })
        .cloned()
        .collect()
}

/// Whether a drive value is a CD-ROM.
pub fn is_cdrom(value: &str) -> bool {
    value.contains("media=cdrom")
}

/// Whether a drive key is a volume that was detached (`unusedN`).
pub fn is_unused(key: &str) -> bool {
    key.starts_with("unused")
}

/// Moves a disk to another storage and deletes the source (the API call behind `qm disk move` / `pct move-volume`).
pub fn disk_move_cmd(node: &str, kind: Kind, id: u32, key: &str, storage: &str) -> Cmd {
    if kind == Kind::Lxc {
        Cmd::new(
            "pvesh",
            &["create", &format!("/nodes/{node}/lxc/{id}/move_volume"), "--volume", key, "--storage", storage, "--delete", "1"],
        )
    } else {
        Cmd::new("pvesh", &["create", &format!("/nodes/{node}/qemu/{id}/move_disk"), "--disk", key, "--storage", storage, "--delete", "1"])
    }
}

/// A VM disk's value with the option `opt` changed to `new` (`None` removes it, which is the default). The volume stays first.
pub fn disk_with_option(value: &str, opt: &str, new: Option<&str>) -> String {
    let mut parts: Vec<String> =
        value.split(',').filter(|p| !p.is_empty() && p.split('=').next() != Some(opt)).map(str::to_string).collect();
    if let Some(n) = new {
        parts.push(format!("{opt}={n}"));
    }
    parts.join(",")
}

/// The next value of the option `opt` (`discard`, `ssd` or `cache`) of a VM disk, the new disk value, and the command that sets it.
/// `discard` and `ssd` toggle; `cache` goes none, writeback, writethrough, directsync, unsafe and back to the default.
/// Only a VM disk has these; `ssd` is not allowed on a `virtio` disk.
pub fn disk_option_cmd(kind: Kind, id: u32, key: &str, value: &str, opt: &str) -> Result<(String, String, Cmd), String> {
    if kind == Kind::Lxc {
        return Err("disk options are for VM disks".into());
    }
    if opt == "ssd" && key.starts_with("virtio") {
        return Err("a virtio disk has no ssd flag".into());
    }
    let cur = value.split(',').find_map(|p| p.strip_prefix(&format!("{opt}=")));
    let next: Option<&str> = match opt {
        "discard" => {
            if cur == Some("on") {
                None
            } else {
                Some("on")
            }
        }
        "ssd" => {
            if cur == Some("1") {
                None
            } else {
                Some("1")
            }
        }
        "cache" => match cur {
            None => Some("none"),
            Some("none") => Some("writeback"),
            Some("writeback") => Some("writethrough"),
            Some("writethrough") => Some("directsync"),
            Some("directsync") => Some("unsafe"),
            _ => None,
        },
        _ => return Err(format!("unknown disk option {opt}")),
    };
    let new = disk_with_option(value, opt, next);
    let said = format!("{opt} {}", next.unwrap_or("default"));
    let cmd = Cmd::new("qm", &["set", &id.to_string(), &format!("--{key}"), &new]);
    Ok((new, said, cmd))
}

/// `--delete <key>`: detaches a disk (it stays as `unusedN`), or destroys an `unusedN` volume for good.
pub fn disk_detach_cmd(kind: Kind, id: u32, key: &str) -> Cmd {
    net_del_cmd(kind, id, key)
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

/// The boot options of a guest as `[start at boot, order, up delay, down delay]` from its configuration (`onboot` and `startup`).
pub fn boot_fields(cfg: &[(String, String)]) -> [String; 4] {
    let get = |k: &str| cfg.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str()).unwrap_or("");
    let mut out = [if get("onboot") == "1" { "yes" } else { "no" }.to_string(), String::new(), String::new(), String::new()];
    for part in get("startup").split(',') {
        match part.split_once('=') {
            Some(("order", v)) => out[1] = v.to_string(),
            Some(("up", v)) => out[2] = v.to_string(),
            Some(("down", v)) => out[3] = v.to_string(),
            _ => {}
        }
    }
    out
}

/// `qm set` / `pct set` for the boot options `[start at boot (yes or no), order, up delay, down delay]`; empty numbers are left out,
/// and with none of them the `startup` option is removed.
pub fn boot_cmd(kind: Kind, id: u32, fields: &[String; 4]) -> Result<Cmd, String> {
    let [onboot, order, up, down] = fields.each_ref().map(|s| s.trim().to_ascii_lowercase());
    let onboot = match onboot.as_str() {
        "yes" | "y" | "1" => "1",
        "no" | "n" | "0" => "0",
        _ => return Err("start at boot is yes or no".into()),
    };
    let mut parts = vec![];
    for (name, v, what) in [("order", &order, "the order"), ("up", &up, "the start delay"), ("down", &down, "the stop delay")] {
        if v.is_empty() {
            continue;
        }
        if v.len() > 5 || !v.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!(
                "{what} is a number of {}",
                if name == "order" { "the place in line (1 starts first)" } else { "seconds" }
            ));
        }
        parts.push(format!("{name}={v}"));
    }
    let mut c = Cmd::new(if kind == Kind::Lxc { "pct" } else { "qm" }, &["set", &id.to_string(), "--onboot", onboot]);
    if parts.is_empty() {
        c.args.extend(["--delete".to_string(), "startup".to_string()]);
    } else {
        c.args.extend(["--startup".to_string(), parts.join(",")]);
    }
    Ok(c)
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
    fn a_backup_job_runs_once_as_a_vzdump_task() {
        let job = |scope: &str, node: &str| BackupJob {
            id: "backup-1".into(),
            enabled: false,
            schedule: "daily".into(),
            storage: "nas".into(),
            scope: scope.into(),
            mode: "snapshot".into(),
            node: node.into(),
            next_run: None,
            prune: String::new(),
        };
        let base = ["create", "/nodes/pve2/vzdump", "--storage", "nas", "--mode", "snapshot"];
        assert_eq!(backup_job_run_cmd(&job("all", "pve2"), "pve1").args, [&base[..], &["--all", "1"]].concat());
        assert_eq!(backup_job_run_cmd(&job("100,101", "pve2"), "pve1").args, [&base[..], &["--vmid", "100,101"]].concat());
        assert_eq!(backup_job_run_cmd(&job("pool web", "pve2"), "pve1").args[6..], ["--pool", "web"]);
        // Without a node it runs on this one; with no name at all it fails loudly.
        assert_eq!(backup_job_run_cmd(&job("all", ""), "pve1").args[1], "/nodes/pve1/vzdump");
        assert_eq!(backup_job_run_cmd(&job("all", ""), "").args[1], "/nodes/invalid/vzdump");
        assert_eq!(backup_job_run_cmd(&job("all", "a/b"), "pve1").args[1], "/nodes/invalid/vzdump");
    }

    #[test]
    fn backup_jobs_are_built_for_create_edit_and_delete() {
        let f = |a: &str, b: &str, c: &str, d: &str| [a, b, c, d].map(String::from);
        let c = backup_job_save_cmd(None, &f("nas", "21:00", "all", "snapshot")).unwrap();
        assert_eq!(c.args, ["create", "/cluster/backup", "--storage", "nas", "--schedule", "21:00", "--mode", "snapshot", "--all", "1"]);
        let c = backup_job_save_cmd(None, &f(" nas ", "mon..fri 02:30", "100,101", "stop")).unwrap();
        assert_eq!(&c.args[6..], ["--mode", "stop", "--vmid", "100,101"]);
        let c = backup_job_save_cmd(Some("backup-1"), &f("nas", "daily", "all", "suspend")).unwrap();
        assert_eq!(&c.args[..2], ["set", "/cluster/backup/backup-1"]);
        assert_eq!(&c.args[8..], ["--all", "1", "--delete", "vmid,pool"]);
        let c = backup_job_save_cmd(Some("backup-1"), &f("nas", "daily", "7", "stop")).unwrap();
        assert_eq!(&c.args[8..], ["--all", "0", "--vmid", "7"]);
        // A pool scope is kept when editing and refused when creating.
        let c = backup_job_save_cmd(Some("backup-1"), &f("nas", "daily", "pool x", "stop")).unwrap();
        assert_eq!(c.args.len(), 8);
        assert!(backup_job_save_cmd(None, &f("nas", "daily", "pool x", "stop")).is_err());
        for bad in [
            f("", "daily", "all", "stop"),
            f("a b", "daily", "all", "stop"),
            f("nas", "", "all", "stop"),
            f("nas", "-x", "all", "stop"),
            f("nas", "daily", "all", "full"),
            f("nas", "daily", "1,,2", "stop"),
            f("nas", "daily", "", "stop"),
        ] {
            assert!(backup_job_save_cmd(None, &bad).is_err(), "{bad:?}");
        }
        assert_eq!(backup_job_delete_cmd("backup-1").args, ["delete", "/cluster/backup/backup-1"]);
    }

    #[test]
    fn a_firewall_level_shows_state_policy_and_rules() {
        let o = r#"{"enable":1,"policy_in":"DROP","policy_out":"ACCEPT"}"#;
        let r = r#"[{"pos":1,"type":"in","action":"ACCEPT","proto":"tcp","dport":"22","enable":1},
                    {"pos":0,"type":"in","action":"ACCEPT","macro":"Ping","enable":0,"comment":"ping"}]"#;
        let l = parse_fw_level("cluster", o, r);
        assert_eq!(l[0], "cluster firewall ON  policy in DROP, out ACCEPT");
        assert_eq!(l[1], "   0  off  in ACCEPT Ping # ping");
        assert_eq!(l[2], "   1  in ACCEPT tcp dport 22");
        let l = parse_fw_level("node pve", "{}", "[]");
        assert_eq!(l, ["node pve firewall OFF  policy in default, out default", "  no rules"]);
    }

    #[test]
    fn an_nfs_storage_is_added_and_removed_by_name() {
        let f = |a: [&str; 4]| a.map(String::from);
        let c = storage_add_nfs_cmd(&f(["nas-1", "10.0.0.5", "/export/pve", "backup,iso"])).unwrap();
        assert_eq!(
            c.args,
            [
                "create",
                "/storage",
                "--storage",
                "nas-1",
                "--type",
                "nfs",
                "--server",
                "10.0.0.5",
                "--export",
                "/export/pve",
                "--content",
                "backup,iso"
            ]
        );
        for bad in [
            ["", "s", "/e", "backup"],
            ["-x", "s", "/e", "backup"],
            ["n", "", "/e", "backup"],
            ["n", "s;rm", "/e", "backup"],
            ["n", "s", "e", "backup"],
            ["n", "s", "/a/../b", "backup"],
            ["n", "s", "/e", "backup,movies"],
            ["n", "s", "/e", ""],
        ] {
            assert!(storage_add_nfs_cmd(&f(bad)).is_err(), "{bad:?}");
        }
        assert_eq!(storage_remove_cmd("nas-1").args, ["delete", "/storage/nas-1"]);
        assert_eq!(storage_remove_cmd("a b").args, ["delete", "/storage/invalid"]);
    }

    #[test]
    fn local_storages_are_added_by_type() {
        let f = |a: [&str; 4]| a.map(String::from);
        let c = storage_add_cmd("dir", &f(["data", "/mnt/data", "", "backup,iso"])).unwrap();
        assert_eq!(c.args, ["create", "/storage", "--storage", "data", "--type", "dir", "--path", "/mnt/data", "--content", "backup,iso"]);
        let c = storage_add_cmd("lvmthin", &f(["thin", "pve", "data", "images,rootdir"])).unwrap();
        assert_eq!(
            c.args,
            [
                "create",
                "/storage",
                "--storage",
                "thin",
                "--type",
                "lvmthin",
                "--vgname",
                "pve",
                "--thinpool",
                "data",
                "--content",
                "images,rootdir"
            ]
        );
        let c = storage_add_cmd("zfspool", &f(["tank", "tank/vm", "", "images"])).unwrap();
        assert_eq!(c.args[6..], ["--pool", "tank/vm", "--content", "images"]);
        assert_eq!(storage_add_cmd("nfs", &f(["nas", "10.0.0.5", "/e", "backup"])).unwrap().args[5], "nfs");
        for (k, bad) in [
            ("dir", ["d", "mnt", "", "backup"]),
            ("dir", ["d", "/a/../b", "", "backup"]),
            ("dir", ["d", "/a", "", "movies"]),
            ("lvmthin", ["t", "", "data", "images"]),
            ("lvmthin", ["t", "pve", "-x", "images"]),
            ("zfspool", ["z", "/tank", "", "images"]),
            ("zfspool", ["z", "t;x", "", "images"]),
            ("cifs", ["c", "a", "b", "backup"]),
            ("dir", ["", "/a", "", "backup"]),
        ] {
            assert!(storage_add_cmd(k, &f(bad)).is_err(), "{k} {bad:?}");
        }
    }

    #[test]
    fn drives_are_listed_moved_and_detached() {
        let cfg = |l: &[(&str, &str)]| l.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
        let c = cfg(&[
            ("name", "x"),
            ("scsi0", "a:d0"),
            ("ide2", "none,media=cdrom"),
            ("unused0", "a:d9"),
            ("efidisk0", "a:e"),
            ("net0", "virtio"),
        ]);
        let keys: Vec<String> = drives(Kind::Qemu, &c).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["scsi0", "ide2", "unused0", "efidisk0"]);
        let keys: Vec<String> =
            drives(Kind::Lxc, &cfg(&[("rootfs", "a:r"), ("mp0", "a:m"), ("scsi0", "x")])).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["rootfs", "mp0"]);
        assert!(is_cdrom("none,media=cdrom") && !is_cdrom("a:d0") && is_unused("unused1") && !is_unused("scsi1"));
        let m = disk_move_cmd("pve", Kind::Qemu, 100, "scsi0", "tank");
        assert_eq!(m.args, ["create", "/nodes/pve/qemu/100/move_disk", "--disk", "scsi0", "--storage", "tank", "--delete", "1"]);
        let m = disk_move_cmd("pve", Kind::Lxc, 200, "mp0", "tank");
        assert_eq!(m.args[1], "/nodes/pve/lxc/200/move_volume");
        assert_eq!(disk_detach_cmd(Kind::Qemu, 100, "scsi1").args, ["set", "100", "--delete", "scsi1"]);
    }

    #[test]
    fn commands_for_another_node_go_through_ssh() {
        let qm = || Cmd::new("qm", &["set", "7", "--name", "it's"]);
        assert_eq!(remote_cmd("pve", "pve", qm(), false), qm());
        assert_eq!(remote_cmd("pve", "", qm(), false), qm());
        let pvesh = Cmd::new("pvesh", &["get", "/nodes"]);
        assert_eq!(remote_cmd("pve", "pve2", pvesh.clone(), false), pvesh);
        let r = remote_cmd("pve", "pve2", qm(), false);
        assert_eq!(r.prog, "ssh");
        assert_eq!(r.args, ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "root@pve2", "'qm' 'set' '7' '--name' 'it'\\''s'"]);
        assert!(remote_cmd("pve", "pve2", qm(), true).args.contains(&"-t".to_string()));
        // A node name that is not a host name is never put on a command line.
        assert_eq!(remote_cmd("pve", "x; rm -rf /", qm(), false), qm());
        assert_eq!(remote_cmd("pve", "-oProxyCommand=x", Cmd::new("pct", &["enter", "1"]), false).prog, "pct");
    }

    #[test]
    fn backup_coverage_lists_the_forgotten_guests_first() {
        let b = |vmid: u32, ctime: u64| Backup {
            volid: format!("nas:backup/vzdump-qemu-{vmid}.vma.zst"),
            storage: "nas".into(),
            node: "pve".into(),
            vmid,
            ctime,
            size: 1,
            protected: false,
            notes: String::new(),
        };
        let now = 100 * 86400;
        let guests = vec![(100, "web".to_string()), (101, "db".to_string()), (102, "old".to_string()), (103, "none".to_string())];
        let list = vec![b(100, now - 100), b(100, now - 3 * 86400), b(101, now - 2 * 86400 - 5), b(102, now - 30 * 86400)];
        let r = backup_coverage(&guests, &list, now);
        assert_eq!(r[0], "backups of 4 guests: 2 without a backup newer than 7 days");
        assert!(r[2].contains("103") && r[2].contains("never") && r[2].contains('!'), "{r:?}");
        assert!(r[3].contains("102") && r[3].contains("30 days ago") && r[3].contains('!'), "{r:?}");
        assert!(r[4].contains("101") && r[4].contains("2 days ago") && !r[4].contains('!'), "{r:?}");
        assert!(r[5].contains("100") && r[5].contains("today"), "{r:?}");
    }

    #[test]
    fn the_sdn_page_nests_vnets_and_subnets_in_zones() {
        let zones = r#"[{"zone":"lan","type":"vlan","nodes":"pve1,pve2"},{"zone":"ov","type":"vxlan"}]"#;
        let vnets = r#"[{"vnet":"web","zone":"lan","tag":20,"alias":"front"},{"vnet":"lost","zone":"gone"}]"#;
        let subs = vec![("web".to_string(), r#"[{"cidr":"10.0.20.0/24","gateway":"10.0.20.1","snat":1}]"#.to_string())];
        let r = sdn_report(zones, vnets, &subs);
        assert_eq!(r[0], "SDN: 2 zones, 2 VNets");
        assert!(r.contains(&"zone lan  type vlan  nodes pve1,pve2".to_string()), "{r:?}");
        assert!(r.contains(&"  vnet web  vlan 20  (front)".to_string()), "{r:?}");
        assert!(r.contains(&"    subnet 10.0.20.0/24  gateway 10.0.20.1  snat".to_string()), "{r:?}");
        assert!(r.contains(&"zone ov  type vxlan".to_string()) && r.last().unwrap().contains("lost"), "{r:?}");
        assert!(sdn_report("[]", "[]", &[])[1].contains("not set up"));
    }

    #[test]
    fn disk_options_toggle_and_cycle() {
        let v = "local-lvm:vm-100-disk-0,size=32G";
        let (n, said, c) = disk_option_cmd(Kind::Qemu, 100, "scsi0", v, "discard").unwrap();
        assert_eq!(n, "local-lvm:vm-100-disk-0,size=32G,discard=on");
        assert_eq!(said, "discard on");
        assert_eq!(c.args, ["set", "100", "--scsi0", "local-lvm:vm-100-disk-0,size=32G,discard=on"]);
        let (n, said, _) = disk_option_cmd(Kind::Qemu, 100, "scsi0", &n, "discard").unwrap();
        assert_eq!((n.as_str(), said.as_str()), (v, "discard default"));
        let (n, _, _) = disk_option_cmd(Kind::Qemu, 100, "sata0", "a:b,cache=unsafe,ssd=1", "cache").unwrap();
        assert_eq!(n, "a:b,ssd=1");
        let (n, said, _) = disk_option_cmd(Kind::Qemu, 100, "sata0", "a:b,ssd=1", "cache").unwrap();
        assert_eq!((n.as_str(), said.as_str()), ("a:b,ssd=1,cache=none", "cache none"));
        assert!(disk_option_cmd(Kind::Qemu, 100, "virtio0", "a:b", "ssd").is_err());
        assert!(disk_option_cmd(Kind::Lxc, 100, "rootfs", "a:b", "discard").is_err());
        assert!(disk_option_cmd(Kind::Qemu, 100, "scsi0", "a:b", "iothread").is_err());
    }

    #[test]
    fn ssh_to_the_other_nodes_is_reported() {
        let j = r#"[{"node":"pve2"},{"node":"pve"},{"node":"pve3"}]"#;
        assert_eq!(parse_other_nodes(j, "pve"), ["pve2", "pve3"]);
        assert!(parse_other_nodes("nope", "pve").is_empty());
        assert_eq!(ssh_report(&[])[1], "  this is the only node: nothing is run over ssh");
        let r = ssh_report(&[("pve2".into(), Ok(())), ("pve3".into(), Err("Permission denied".into()))]);
        assert_eq!(r[0], "ssh to the other nodes (2 nodes, 1 not reachable)");
        assert_eq!((r[1].as_str(), r[2].as_str()), ("  pve2  ok", "  pve3  CANNOT LOG IN: Permission denied"));
        assert!(r.iter().any(|l| l.contains("ssh-copy-id")));
        assert!(!ssh_report(&[("a".into(), Ok(()))]).iter().any(|l| l.contains("ssh-copy-id")));
        assert_eq!(ssh_probe_cmd("pve2").unwrap().args.last().unwrap(), "true");
        assert!(ssh_probe_cmd("a b").is_none() && ssh_probe_cmd("").is_none());
    }

    #[test]
    fn backup_retention_is_read_and_set() {
        assert_eq!(parse_prune("keep-last=3,keep-daily=7"), ["3", "", "7", "", "", ""]);
        assert_eq!(parse_prune(""), [""; 6].map(String::from));
        let jobs = parse_backup_jobs(r#"[{"id":"backup-1","prune-backups":"keep-weekly=4"},{"id":"b2"}]"#);
        assert_eq!((jobs[0].prune.as_str(), jobs[1].prune.as_str()), ("keep-weekly=4", ""));
        let v = ["3", "", "7", "", "", "1"].map(String::from);
        assert_eq!(
            backup_prune_cmd("backup-1", &v).unwrap().args,
            ["set", "/cluster/backup/backup-1", "--prune-backups", "keep-last=3,keep-daily=7,keep-yearly=1"]
        );
        assert_eq!(
            backup_prune_cmd("backup-1", &Default::default()).unwrap().args,
            ["set", "/cluster/backup/backup-1", "--delete", "prune-backups"]
        );
        let bad = ["x", "", "", "", "", ""].map(String::from);
        assert!(backup_prune_cmd("b", &bad).is_err() && backup_prune_cmd("a b", &v).is_err());
        assert!(backup_prune_cmd("b", &["99999", "", "", "", "", ""].map(String::from)).is_err());
    }

    #[test]
    fn tags_and_note_are_read_and_set() {
        let cfg = vec![("tags".to_string(), "Prod;web".to_string()), ("description".to_string(), "front\n\nend ".to_string())];
        let o = notes_fields(&cfg);
        assert_eq!(o, ["Prod;web", "front / end"]);
        assert_eq!(clean_tags("Prod, web web;db").unwrap(), "prod;web;db");
        assert!(clean_tags("a/b").is_err() && clean_tags("-x").is_err() && clean_tags("").unwrap().is_empty());
        let v = |a: &str, b: &str| [a.to_string(), b.to_string()];
        let c = notes_cmd(Kind::Qemu, 100, &v("prod;web;db", "front / end"), &o).unwrap();
        assert_eq!((c.prog.as_str(), c.args), ("qm", ["set", "100", "--tags", "prod;web;db"].map(String::from).to_vec()));
        let c = notes_cmd(Kind::Lxc, 101, &v("", "new note"), &o).unwrap();
        assert_eq!(
            (c.prog.as_str(), c.args),
            ("pct", ["set", "101", "--description", "new note", "--delete", "tags"].map(String::from).to_vec())
        );
        assert_eq!(notes_cmd(Kind::Qemu, 1, &v("", ""), &o).unwrap().args, ["set", "1", "--delete", "tags,description"]);
        assert_eq!(notes_cmd(Kind::Qemu, 1, &v("Prod web", "front / end"), &o).unwrap_err(), "nothing changed");
        assert!(notes_cmd(Kind::Qemu, 1, &v("", "-x"), &o).is_err());
    }

    #[test]
    fn cloud_init_is_read_and_set() {
        let cfg: Vec<(String, String)> = [
            ("ciuser", "ubuntu"),
            ("ipconfig0", "ip=10.0.0.5/24,gw=10.0.0.1"),
            ("nameserver", "1.1.1.1"),
            ("sshkeys", "ssh-ed25519%20AAAA"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let f = cloudinit_fields(&cfg);
        assert_eq!(f, ["ubuntu", "10.0.0.5/24 10.0.0.1", "1.1.1.1", "", ""]);
        // Setting what was read changes the user, the IP with its gateway and the DNS; nothing more.
        let c = cloudinit_cmd(101, &f).unwrap();
        assert_eq!(c.args, ["set", "101", "--ciuser", "ubuntu", "--ipconfig0", "ip=10.0.0.5/24,gw=10.0.0.1", "--nameserver", "1.1.1.1"]);
        let mut g = f.clone();
        g[1] = "dhcp".into();
        g[3] = "lan.example".into();
        g[4] = "/root/.ssh/id_ed25519.pub".into();
        let c = cloudinit_cmd(101, &g).unwrap();
        assert!(
            c.args.windows(2).any(|w| w == ["--ipconfig0", "ip=dhcp"])
                && c.args.windows(2).any(|w| w == ["--sshkeys", "/root/.ssh/id_ed25519.pub"])
        );
        let bad = |i: usize, v: &str| {
            let mut h = f.clone();
            h[i] = v.into();
            cloudinit_cmd(101, &h).is_err()
        };
        assert!(
            bad(0, "a b")
                && bad(0, "-x")
                && bad(1, "10.0.0.5")
                && bad(1, "a/24 b c")
                && bad(2, "1.1.1.1;ls")
                && bad(3, "a b")
                && bad(4, "rel/key")
                && bad(4, "/a/../b")
        );
        assert!(cloudinit_cmd(101, &Default::default()).is_err());
    }

    #[test]
    fn hardware_is_added_to_a_vm() {
        let keys: Vec<String> = ["scsi0", "usb0", "serial0"].iter().map(|s| s.to_string()).collect();
        let (k, v, c) = hardware_add_cmd(101, &keys, Hw::Usb, "046d:c52b").unwrap();
        assert_eq!((k.as_str(), v.as_str()), ("usb1", "host=046d:c52b"));
        assert_eq!(c.args, ["set", "101", "--usb1", "host=046d:c52b"]);
        assert!(hardware_add_cmd(101, &keys, Hw::Usb, "046d").is_none() && hardware_add_cmd(101, &keys, Hw::Usb, "046d:zzzz").is_none());
        let (k, v, _) = hardware_add_cmd(101, &keys, Hw::Pci, "0000:01:00.0").unwrap();
        assert_eq!((k.as_str(), v.as_str()), ("hostpci0", "0000:01:00.0"));
        assert!(hardware_add_cmd(101, &keys, Hw::Pci, "01").is_none() && hardware_add_cmd(101, &keys, Hw::Pci, "0000:01:00.0,x").is_none());
        assert_eq!(hardware_add_cmd(101, &keys, Hw::Serial, "").unwrap().0, "serial1");
        let (k, v, _) = hardware_add_cmd(101, &keys, Hw::Tpm, "local-lvm").unwrap();
        assert_eq!((k.as_str(), v.as_str()), ("tpmstate0", "local-lvm:1,version=v2.0"));
        assert_eq!(hardware_add_cmd(101, &keys, Hw::Efi, "local-lvm").unwrap().1, "local-lvm:1,efitype=4m,pre-enrolled-keys=1");
        // Those two exist once; a storage never holds a comma or a colon.
        let has: Vec<String> = ["tpmstate0".to_string(), "efidisk0".to_string()].to_vec();
        assert!(hardware_add_cmd(101, &has, Hw::Tpm, "x").is_none() && hardware_add_cmd(101, &has, Hw::Efi, "x").is_none());
        assert!(hardware_add_cmd(101, &keys, Hw::Efi, "a,b").is_none() && hardware_add_cmd(101, &keys, Hw::Tpm, "").is_none());
    }

    #[test]
    fn templates_are_listed_and_downloaded() {
        let j = r#"[{"template":"debian-12-standard_12.7-1_amd64.tar.zst","section":"system"},{"template":"alpine-3.20-default_20240908_amd64.tar.xz"},{"template":"debian-12-standard_12.7-1_amd64.tar.zst"},{"x":1}]"#;
        assert_eq!(parse_aplinfo(j), ["alpine-3.20-default_20240908_amd64.tar.xz", "debian-12-standard_12.7-1_amd64.tar.zst"]);
        assert!(parse_aplinfo("x").is_empty());
        let c = template_download_cmd("local", "debian-12-standard_12.7-1_amd64.tar.zst").unwrap();
        assert_eq!(
            (c.prog.as_str(), c.args),
            ("pveam", vec!["download".to_string(), "local".into(), "debian-12-standard_12.7-1_amd64.tar.zst".into()])
        );
        assert!(
            template_download_cmd("", "a").is_none()
                && template_download_cmd("local", "a b").is_none()
                && template_download_cmd("lo:cal", "a").is_none()
        );
    }

    #[test]
    fn guests_are_created_from_a_form() {
        let g = NewGuest {
            kind: Kind::Qemu,
            id: 120,
            name: "web".into(),
            cores: 2,
            mem_mib: 2048,
            disk_gib: 32,
            storage: "local-lvm".into(),
            bridge: "vmbr0".into(),
            source: String::new(),
        };
        let c = create_cmd(&g).unwrap();
        assert_eq!(c.prog, "qm");
        assert_eq!(c.args[..3], ["create", "120", "--name"]);
        assert!(c.args.windows(2).any(|w| w == ["--scsi0", "local-lvm:32"]) && !c.args.contains(&"--cdrom".to_string()));
        let iso = NewGuest { source: "local:iso/d.iso".into(), ..g.clone() };
        assert!(create_cmd(&iso).unwrap().args.windows(2).any(|w| w == ["--cdrom", "local:iso/d.iso"]));
        let ct = NewGuest { kind: Kind::Lxc, source: "local:vztmpl/debian-12.tar.zst".into(), ..g.clone() };
        let c = create_cmd(&ct).unwrap();
        assert_eq!((c.prog.as_str(), c.args[1].as_str(), c.args[2].as_str()), ("pct", "120", "local:vztmpl/debian-12.tar.zst"));
        assert!(
            c.args.windows(2).any(|w| w == ["--rootfs", "local-lvm:32"])
                && c.args.windows(2).any(|w| w == ["--net0", "name=eth0,bridge=vmbr0,ip=dhcp"])
        );
        // A container needs a template; the rest is checked too.
        assert!(create_cmd(&NewGuest { kind: Kind::Lxc, ..g.clone() }).is_err());
        assert!(create_cmd(&NewGuest { name: "-x".into(), ..g.clone() }).is_err());
        assert!(create_cmd(&NewGuest { cores: 0, ..g.clone() }).is_err());
        assert!(create_cmd(&NewGuest { mem_mib: 4, ..g.clone() }).is_err());
        assert!(create_cmd(&NewGuest { disk_gib: 0, ..g.clone() }).is_err());
        assert!(create_cmd(&NewGuest { storage: "a,b".into(), ..g.clone() }).is_err());
        assert!(create_cmd(&NewGuest { source: "nocolon".into(), ..g }).is_err());
    }

    #[test]
    fn a_cdrom_gets_an_iso_or_is_ejected() {
        let c = cdrom_set_cmd(100, "ide2", "local:iso/d.iso").unwrap();
        assert_eq!(
            (c.prog.as_str(), c.args.clone()),
            ("qm", vec!["set".to_string(), "100".into(), "--ide2".into(), "local:iso/d.iso,media=cdrom".into()])
        );
        assert_eq!(cdrom_set_cmd(100, "ide2", "none").unwrap().args[3], "none,media=cdrom");
        assert!(
            cdrom_set_cmd(100, "net0", "none").is_none()
                && cdrom_set_cmd(100, "ide2", "").is_none()
                && cdrom_set_cmd(100, "ide2", "a,b").is_none()
        );
        let keys: Vec<String> = ["ide0", "ide2"].iter().map(|s| s.to_string()).collect();
        assert_eq!(cdrom_add_cmd(100, &keys).unwrap().0, "ide1");
        let full: Vec<String> = (0..4).map(|n| format!("ide{n}")).collect();
        assert!(cdrom_add_cmd(100, &full).is_none());
    }

    #[test]
    fn a_nic_and_a_disk_are_added_in_the_first_free_slot() {
        let keys = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (dev, v, c) = net_add_cmd(Kind::Qemu, 100, &keys(&["net0", "net2", "scsi0"]), "vmbr1", "7").unwrap();
        assert_eq!((dev.as_str(), v.as_str()), ("net1", "virtio,bridge=vmbr1,tag=7"));
        assert_eq!((c.prog.as_str(), c.args.last().unwrap().as_str()), ("qm", "virtio,bridge=vmbr1,tag=7"));
        let (dev, v, _) = net_add_cmd(Kind::Lxc, 200, &keys(&["net0"]), "vmbr0", "").unwrap();
        assert_eq!((dev.as_str(), v.as_str()), ("net1", "name=eth1,bridge=vmbr0,ip=dhcp"));
        assert!(net_add_cmd(Kind::Qemu, 100, &[], "", "").is_none());
        let full: Vec<String> = (0..=9).map(|n| format!("net{n}")).collect();
        assert!(net_add_cmd(Kind::Lxc, 200, &full, "vmbr0", "").is_none());
        let c = net_del_cmd(Kind::Qemu, 100, "net1");
        assert_eq!(c.args, ["set", "100", "--delete", "net1"]);

        // scsi1 is a cloud-init drive that is not in the disk list, but it is in the keys.
        let (dev, v, c) = disk_add_cmd(Kind::Qemu, 100, &keys(&["scsi0", "scsi1"]), "local-lvm", 8).unwrap();
        assert_eq!((dev.as_str(), v.as_str()), ("scsi2", "local-lvm:8"));
        assert_eq!(c.args, ["set", "100", "--scsi2", "local-lvm:8"]);
        let (dev, v, _) = disk_add_cmd(Kind::Lxc, 200, &keys(&["rootfs", "mp0"]), "tank", 4).unwrap();
        assert_eq!((dev.as_str(), v.as_str()), ("mp1", "tank:4,mp=/mnt/disk1"));
        assert!(disk_add_cmd(Kind::Qemu, 100, &[], "", 8).is_none());
        assert!(disk_add_cmd(Kind::Qemu, 100, &[], "a,b", 8).is_none());
        assert!(disk_add_cmd(Kind::Qemu, 100, &[], "local", 0).is_none());
    }

    #[test]
    fn the_upgrade_is_counted_and_noninteractive() {
        assert_eq!(parse_apt_count(r#"[{"Package":"a"},{"Package":"b"}]"#), Some(2));
        assert_eq!(parse_apt_count("[]"), Some(0));
        assert_eq!(parse_apt_count("x"), None);
        let c = apt_upgrade_cmd();
        assert_eq!(c.prog, "env");
        assert!(c.args.contains(&"dist-upgrade".to_string()) && c.args.contains(&"DEBIAN_FRONTEND=noninteractive".to_string()));
    }

    #[test]
    fn a_storage_is_edited() {
        assert_eq!(storage_edit_cmd("nas", "backup,iso", "").unwrap().args, ["set", "/storage/nas", "--content", "backup,iso"]);
        assert_eq!(storage_edit_cmd("nas", "", "No").unwrap().args, ["set", "/storage/nas", "--disable", "1"]);
        assert_eq!(
            storage_edit_cmd("nas", "images", "yes").unwrap().args,
            ["set", "/storage/nas", "--content", "images", "--disable", "0"]
        );
        for bad in [("nas", "", ""), ("nas", "photos", ""), ("nas", "", "maybe"), ("a b", "iso", ""), ("", "iso", "")] {
            assert!(storage_edit_cmd(bad.0, bad.1, bad.2).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn boot_options_are_read_and_set() {
        let cfg = |l: &[(&str, &str)]| l.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
        assert_eq!(boot_fields(&cfg(&[("onboot", "1"), ("startup", "order=3,up=30,down=60")])), ["yes", "3", "30", "60"]);
        assert_eq!(boot_fields(&cfg(&[("startup", "up=5")])), ["no", "", "5", ""]);
        assert_eq!(boot_fields(&[]), ["no", "", "", ""]);
        let f = |a: [&str; 4]| a.map(String::from);
        assert_eq!(
            boot_cmd(Kind::Qemu, 100, &f(["Yes", "3", "30", ""])).unwrap().args,
            ["set", "100", "--onboot", "1", "--startup", "order=3,up=30"]
        );
        let c = boot_cmd(Kind::Lxc, 101, &f(["no", "", "", ""])).unwrap();
        assert_eq!(
            (c.prog.as_str(), c.args),
            ("pct", vec!["set", "101", "--onboot", "0", "--delete", "startup"].into_iter().map(String::from).collect())
        );
        for bad in [["maybe", "", "", ""], ["yes", "x", "", ""], ["yes", "", "-1", ""], ["yes", "", "", "123456"]] {
            assert!(boot_cmd(Kind::Qemu, 1, &f(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_guest_is_added_to_and_removed_from_ha() {
        assert_eq!(ha_add_cmd(Kind::Qemu, 100).args, ["create", "/cluster/ha/resources", "--sid", "vm:100", "--state", "started"]);
        assert_eq!(ha_add_cmd(Kind::Lxc, 101).args[3], "ct:101");
        assert_eq!(ha_remove_cmd(Kind::Lxc, 101).args, ["delete", "/cluster/ha/resources/ct:101"]);
    }

    #[test]
    fn a_guest_moves_to_another_node() {
        assert_eq!(migrate_cmd(Kind::Qemu, 100, "pve2", true, true).unwrap().args, ["migrate", "100", "pve2", "--online", "1"]);
        assert_eq!(migrate_cmd(Kind::Qemu, 100, "pve2", true, false).unwrap().args, ["migrate", "100", "pve2", "--online", "0"]);
        assert_eq!(migrate_cmd(Kind::Qemu, 100, "pve2", false, true).unwrap().args, ["migrate", "100", "pve2"]);
        let c = migrate_cmd(Kind::Lxc, 101, "pve2", true, true).unwrap();
        assert_eq!(
            (c.prog.as_str(), c.args),
            ("pct", vec!["migrate".to_string(), "101".into(), "pve2".into(), "--restart".into(), "1".into()])
        );
        assert!(migrate_cmd(Kind::Qemu, 100, "", true, true).is_err());
        assert!(migrate_cmd(Kind::Qemu, 100, "a;b", true, true).is_err());
    }

    #[test]
    fn node_time_and_dns_are_read_and_set() {
        let v = parse_node_values(r#"{"timezone":"UTC","time":1}"#, r#"{"search":"lan","dns1":"1.1.1.1","dns2":"9.9.9.9"}"#).unwrap();
        assert_eq!(v, ["UTC", "lan", "1.1.1.1", "9.9.9.9", ""].map(String::from));
        assert!(parse_node_values("x", "{}").is_none());
        let c = node_save_cmds("pve", &v).unwrap();
        assert_eq!(c[0].args, ["set", "/nodes/pve/time", "--timezone", "UTC"]);
        assert_eq!(c[1].args, ["set", "/nodes/pve/dns", "--search", "lan", "--dns1", "1.1.1.1", "--dns2", "9.9.9.9"]);
        let bad = |i: usize, s: &str| {
            let mut f = v.clone();
            f[i] = s.into();
            node_save_cmds("pve", &f).is_err()
        };
        assert!(bad(0, "") && bad(0, "../x") && bad(0, "/etc") && bad(1, "a b") && bad(2, "") && bad(2, "host") && bad(3, "x;y"));
        assert!(node_save_cmds("../x", &v).is_err());
    }

    #[test]
    fn network_interfaces_show_addresses_members_and_down() {
        let j = r#"[{"iface":"vmbr0","type":"bridge","cidr":"192.168.1.10/24","gateway":"192.168.1.1","bridge_ports":"eno1","autostart":1,"active":1},
                    {"iface":"eno1","type":"eth","autostart":1,"active":0},
                    {"iface":"bond0","type":"bond","slaves":"a b","bond_mode":"802.3ad","address":"10.0.0.2","active":1,"comments":"storage"},
                    {"iface":"vlan5","type":"vlan","vlan-id":5,"vlan-raw-device":"vmbr0","active":1}]"#;
        let l = parse_network(j);
        assert_eq!(l[0], "network interfaces (4)");
        assert!(l[1].contains("bond0") && l[1].contains("10.0.0.2"), "{l:?}");
        assert!(l.contains(&"             slaves a b".to_string()));
        assert!(l.contains(&"             mode 802.3ad".to_string()));
        assert!(l.contains(&"             # storage".to_string()));
        assert!(l.iter().any(|x| x.contains("eno1") && x.contains("no address") && x.ends_with("DOWN")), "{l:?}");
        assert!(
            l.iter().any(|x| x.contains("vmbr0") && x.contains("192.168.1.10/24") && x.contains("gw 192.168.1.1") && !x.contains("DOWN"))
        );
        assert!(l.contains(&"             vlan 5 on vmbr0".to_string()));
        assert!(parse_network("x").is_empty());
    }

    #[test]
    fn disks_show_health_wearout_and_smart_values() {
        let list = r#"[{"devpath":"/dev/sda","model":"Samsung SSD 870","size":1000204886016,"type":"ssd","health":"PASSED","wearout":98,"serial":"S1"},
                       {"devpath":"/dev/nvme0n1","model":"WD","size":500107862016,"type":"nvme","health":"FAILED"},
                       {"devpath":"/dev/../etc","model":"x","size":1,"type":"hdd","health":"OK"}]"#;
        let smart = |d: &str| {
            match d {
            "/dev/sda" => r#"{"health":"PASSED","attributes":[{"id":" 5","name":"Reallocated_Sector_Ct","raw":"0"},{"id":9,"name":"Power_On_Hours","raw":"1234"},{"id":"1","name":"Raw_Read_Error_Rate","raw":"7"}]}"#.to_string(),
            "/dev/nvme0n1" => r#"{"health":"FAILED","text":"Critical Warning:   0x04\nTemperature:  41 Celsius\nFoo: 1"}"#.to_string(),
            other => panic!("asked for {other}"),
        }
        };
        let l = parse_disks(list, &smart);
        assert_eq!(l[0], "disks (3)");
        assert_eq!(l[1], "  /dev/sda  Samsung SSD 870  931.5 GiB  ssd  PASSED  wearout 98% left");
        assert_eq!(l[2], "    serial S1");
        assert_eq!(l[3], "    Reallocated_Sector_Ct 0, Power_On_Hours 1234");
        assert!(l[4].contains("/dev/nvme0n1") && l[4].ends_with("FAILED  UNHEALTHY"), "{l:?}");
        assert_eq!(l[5], "    Critical Warning:   0x04");
        assert_eq!(l[6], "    Temperature:  41 Celsius");
        assert!(l[7].contains("/dev/../etc") && l.len() == 8, "{l:?}");
        assert!(parse_disks("x", &smart).is_empty());
    }

    #[test]
    fn certificates_show_how_long_they_hold() {
        let j = r#"[{"filename":"pve-ssl.pem","subject":"CN=pve1","issuer":"CN=PVE CA","notafter":1000000,"san":["pve1","10.0.0.1"]},
                    {"filename":"pveproxy-ssl.pem","subject":"CN=a","issuer":"CN=b","notafter":5000000},
                    {"filename":"old.pem","notafter":10}]"#;
        let l = parse_certs(j, 900_000);
        assert_eq!(l[0], "certificates (3)");
        assert_eq!(l[1], "  pve-ssl.pem  EXPIRES SOON: in 2d");
        assert!(l[2].contains("CN=pve1") && l[3].contains("CN=PVE CA") && l[4].ends_with("pve1, 10.0.0.1"), "{l:?}");
        assert!(l.contains(&"  pveproxy-ssl.pem  valid for 47d".to_string()), "{l:?}");
        assert!(l.iter().any(|s| s.contains("old.pem  EXPIRED 11d ago")), "{l:?}");
        assert!(parse_certs("x", 0).is_empty());
    }

    #[test]
    fn apt_updates_are_listed_and_refreshed() {
        let l = parse_apt(
            r#"[{"Package":"pve-manager","OldVersion":"8.2.2","Version":"8.2.4","Title":"x"},{"Package":"curl","Version":"8.1"}]"#,
        );
        assert_eq!(l, ["updates waiting (2)", "  pve-manager  8.2.2 -> 8.2.4", "  curl  ? -> 8.1"]);
        assert!(parse_apt("[]")[1].contains("none"));
        assert!(parse_apt("x").is_empty());
        assert_eq!(apt_refresh_cmd("pve1").args, ["create", "/nodes/pve1/apt/update"]);
        assert_eq!(apt_refresh_cmd("a/b").args, ["create", "/nodes/invalid/apt/update"]);
    }

    #[test]
    fn services_and_node_settings_are_read() {
        let l = parse_services(
            r#"[{"service":"pveproxy","desc":"PVE API Proxy Server","state":"running"},{"name":"cron","state":"stopped"},{"x":1}]"#,
        );
        assert_eq!(l.len(), 2);
        assert_eq!((l[0].name.as_str(), l[0].state.as_str(), l[0].desc.as_str()), ("pveproxy", "running", "PVE API Proxy Server"));
        assert_eq!(l[1].name, "cron");
        assert!(parse_services("x").is_empty());
        assert_eq!(service_cmd("pve1", "pveproxy", "restart").args, ["create", "/nodes/pve1/services/pveproxy/restart"]);
        assert_eq!(service_cmd("pve1", "cron", "stop").args, ["create", "/nodes/pve1/services/cron/stop"]);
        assert_eq!(service_cmd("pve1", "a/../b", "restart").args, ["create", "/nodes/invalid/services/invalid/invalid"]);
        assert_eq!(service_cmd("pve1", "cron", "rm -rf").args, ["create", "/nodes/invalid/services/invalid/invalid"]);
        let s = parse_node_settings(
            r#"{"timezone":"Asia/Tokyo","time":1000,"localtime":33400}"#,
            r#"{"search":"lan","dns1":"192.168.0.1"}"#,
            r#"{"data":"127.0.0.1 localhost\n10.0.0.2 pve1\n"}"#,
        );
        assert_eq!(s[0], "time");
        assert!(s[1].contains("Asia/Tokyo") && s[2].contains("33400") && s[3].contains("1000"), "{s:?}");
        assert_eq!(&s[5..8], ["dns", "  search lan", "  dns1   192.168.0.1"]);
        assert_eq!(&s[9..], ["hosts", "  127.0.0.1 localhost", "  10.0.0.2 pve1"]);
        assert!(parse_node_settings("x", "x", "x").is_empty());
    }

    #[test]
    fn pools_show_zfs_and_ceph_health() {
        let zfs = r#"[{"name":"rpool","health":"ONLINE","size":1073741824000,"alloc":536870912000,"frag":3},{"name":"tank","health":"DEGRADED","size":0,"alloc":0}]"#;
        let ceph = r#"{"health":{"status":"HEALTH_WARN","checks":{"OSD_DOWN":{"severity":"HEALTH_WARN","summary":{"message":"1 osds down"}}}},"pgmap":{"bytes_used":1073741824,"bytes_total":10737418240},"osdmap":{"num_up_osds":2,"num_osds":3}}"#;
        let l = parse_pools(zfs, ceph);
        assert_eq!(l[0], "zfs pools (2)");
        assert!(
            l[1].contains("rpool") && l[1].contains("ONLINE") && !l[1].contains("UNHEALTHY") && l[1].contains("50% of 1000.0 GiB"),
            "{l:?}"
        );
        assert!(l[2].contains("DEGRADED  UNHEALTHY"), "{l:?}");
        assert_eq!(l[4], "ceph HEALTH_WARN  UNHEALTHY");
        assert!(l[5].contains("OSD_DOWN: 1 osds down") && !l[5].contains("UNHEALTHY"), "{l:?}");
        assert_eq!(l[6], "  used 1.0 GiB of 10.0 GiB");
        assert_eq!(l[7], "  osds 2 up of 3");
        assert!(parse_pools("x", "{}").is_empty());
        let ok = parse_pools("[]", r#"{"health":{"status":"HEALTH_OK"}}"#);
        assert_eq!(ok, ["zfs pools (0)", "", "ceph HEALTH_OK"]);
        assert_eq!(human(512), "512 B");
    }

    #[test]
    fn replication_jobs_show_their_last_run() {
        let jobs = r#"[{"id":"100-0","source":"pve1","target":"pve2","schedule":"*/15","rate":10.0},{"id":"101-0","source":"pve1","target":"pve3","schedule":"daily","disable":1},{"x":1}]"#;
        let st = r#"[{"id":"100-0","last_sync":940,"next_sync":1600,"duration":3.2,"fail_count":2,"error":"no route"}]"#.to_string();
        let l = parse_replication(jobs, &[st], 1000);
        assert_eq!(l[0], "replication jobs (3)");
        assert!(l[1].contains("100-0") && l[1].contains("pve1 -> pve2") && l[1].contains("*/15") && l[1].contains("rate 10 MB/s"), "{l:?}");
        assert!(l[2].contains("FAILING x2: no route"), "{l:?}");
        assert!(l[3].contains("last 60s ago (3s)") && l[3].contains("next in 10m"), "{l:?}");
        assert!(l[4].contains("101-0") && l[4].contains("OFF"));
        assert_eq!(l.len(), 5);
        assert_eq!(when(0, 5), "never");
        assert_eq!(when(10_000 - 7200, 10_000), "2h ago");
        assert!(parse_replication("x", &[], 0)[0].contains("(0)"));
    }

    #[test]
    fn ha_resources_join_their_live_state() {
        let res = r#"[{"sid":"vm:100","state":"started","group":"main"},{"sid":"ct:101","state":"disabled"},{"state":"x"}]"#;
        let st =
            r#"[{"id":"quorum","type":"quorum","status":"OK"},{"id":"service:vm:100","type":"service","node":"pve2","state":"started"}]"#;
        let l = parse_ha(res, st);
        assert_eq!(l.len(), 2);
        assert_eq!(
            (l[0].sid.as_str(), l[0].state.as_str(), l[0].group.as_str(), l[0].node.as_str(), l[0].status.as_str()),
            ("vm:100", "started", "main", "pve2", "started")
        );
        assert_eq!((l[1].state.as_str(), l[1].node.as_str(), l[1].status.as_str()), ("disabled", "", ""));
        assert!(parse_ha("x", "y").is_empty());
        assert_eq!(ha_cmd("vm:100", false).args, ["set", "/cluster/ha/resources/vm:100", "--state", "disabled"]);
        assert_eq!(ha_cmd("ct:101", true).args[3], "started");
    }

    #[test]
    fn users_tokens_and_permissions_are_listed() {
        let users = r#"[{"userid":"root@pam","enable":1,"expire":0,"groups":"admins"},
                        {"userid":"bob@pve","enable":0,"expire":1000,"tokens":[{"tokenid":"ci","privsep":0,"expire":0},{"tokenid":"ro","privsep":1,"expire":90000}]},
                        {"enable":1}]"#;
        let acl = r#"[{"path":"/","roleid":"Administrator","type":"user","ugid":"root@pam","propagate":1},
                      {"path":"/vms/100","roleid":"PVEVMUser","type":"token","ugid":"bob@pve!ro","propagate":0}]"#;
        let l = parse_access(users, acl, 2000).join("|");
        assert!(l.contains("users (3)") && l.contains("permissions (2)"), "{l}");
        assert!(l.contains("root@pam") && l.contains("groups admins") && l.contains("never expires"), "{l}");
        assert!(l.contains("bob@pve") && l.contains("OFF") && l.contains("EXPIRED"), "{l}");
        assert!(l.contains("token ci: the user's permissions") && l.contains("token ro: own permissions, expires in 2d"), "{l}");
        assert!(l.contains("/vms/100") && l.contains("token bob@pve!ro") && l.contains("(this path only)"), "{l}");
        assert_eq!(expiry(0, 5), "never expires");
        assert!(parse_access("x", "y", 0).iter().all(|s| !s.contains('@')));
    }

    #[test]
    fn a_snapshot_note_is_set_through_pvesh() {
        let c = snap_note_cmd("pve", Kind::Lxc, 101, "before-upgrade", "kernel 6.8");
        assert_eq!(c.prog, "pvesh");
        assert_eq!(c.args, ["set", "/nodes/pve/lxc/101/snapshot/before-upgrade/config", "--description", "kernel 6.8"]);
        assert_eq!(snap_note_cmd("n", Kind::Qemu, 5, "s", "").args[1], "/nodes/n/qemu/5/snapshot/s/config");
    }

    #[test]
    fn backup_jobs_are_read_and_switched() {
        let j = r#"[{"id":"backup-1","schedule":"sat 02:00","storage":"nas","all":1,"mode":"snapshot","enabled":1,"next-run":1700000000},
                    {"id":"backup-2","schedule":"daily","storage":"local","vmid":"100,101","enabled":0,"node":"pve"},
                    {"id":"backup-3","schedule":"mon 01:00","storage":"nas","pool":"web"},
                    {"schedule":"x"}]"#;
        let l = parse_backup_jobs(j);
        assert_eq!(l.len(), 3);
        assert_eq!((l[0].enabled, l[0].scope.as_str(), l[0].next_run), (true, "all", Some(1700000000)));
        assert_eq!((l[1].enabled, l[1].scope.as_str(), l[1].node.as_str()), (false, "100,101", "pve"));
        assert_eq!((l[2].enabled, l[2].scope.as_str()), (true, "pool web"));
        assert_eq!(backup_job_cmd("backup-2", true).args, ["set", "/cluster/backup/backup-2", "--enabled", "1"]);
    }

    #[test]
    fn a_log_is_saved_to_a_new_file() {
        assert_eq!(export_name("qmstart 100 on pve", 42), "yagura-qmstart-100-on-pve-42.log");
        assert_eq!(export_name("../x", 1), "yagura-x-1.log");
        let dir = std::env::temp_dir().join(format!("yagura-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lines = vec!["a".to_string(), "b".to_string()];
        let p = export_lines(&dir, "t", 7, &lines).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nb\n");
        // The same name again is refused, not overwritten.
        assert!(export_lines(&dir, "t", 7, &lines).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn storage_contents_are_totalled_and_freed() {
        let j = r#"[{"volid":"local:iso/a.iso","content":"iso","size":300},
                    {"volid":"local:vztmpl/d.tar.zst","content":"vztmpl","size":100},
                    {"volid":"local:iso/b.iso","content":"iso","size":200},
                    {"volid":"local:100/vm-100-disk-0.qcow2","content":"images","size":50,"vmid":100},
                    {"content":"iso"}]"#;
        let l = parse_content_list(j);
        assert_eq!(l.len(), 4);
        assert_eq!(l[0].volid, "local:iso/a.iso");
        assert_eq!(content_totals(&l), [("iso".to_string(), 500, 2), ("vztmpl".to_string(), 100, 1), ("images".to_string(), 50, 1)]);
        assert_eq!(vol_delete_cmd(&l[0]), Cmd::new("pvesm", &["free", "local:iso/a.iso"]));
        assert!(parse_content_list("nope").is_empty());
    }

    #[test]
    fn the_firewall_is_read_and_switched() {
        assert!(parse_fw_options(r#"{"enable":1,"policy_in":"DROP"}"#));
        assert!(!parse_fw_options(r#"{"enable":0}"#) && !parse_fw_options("{}") && !parse_fw_options("nope"));
        let j = r#"[{"pos":1,"type":"in","action":"ACCEPT","enable":1,"macro":"SSH","source":"10.0.0.0/24","comment":"admin"},
                    {"pos":0,"type":"in","action":"DROP","enable":0,"proto":"tcp","dport":"80"}]"#;
        let r = parse_fw_rules(j);
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].pos, r[0].enabled, r[0].line.as_str()), (0, false, "in DROP tcp dport 80"));
        assert_eq!(r[1].line, "in ACCEPT SSH from 10.0.0.0/24 # admin");
        assert_eq!(firewall_cmd("pve", Kind::Lxc, 101, true).args, ["set", "/nodes/pve/lxc/101/firewall/options", "--enable", "1"]);
        assert_eq!(firewall_cmd("pve", Kind::Qemu, 100, false).args.last().map(String::as_str), Some("0"));
    }

    #[test]
    fn network_devices_are_read_and_rewritten() {
        let c = |k: &str, v: &str| (k.to_string(), v.to_string());
        let cfg = [
            c("cores", "2"),
            c("net0", "virtio=BC:24:11:00:00:01,bridge=vmbr0,tag=10,firewall=1"),
            c("net1", "e1000=AA:BB:CC:DD:EE:FF,bridge=vmbr1"),
            c("netx", "no"),
        ];
        assert_eq!(nets(&cfg).len(), 2);
        let v0 = &cfg[1].1;
        assert_eq!(net_fields(Kind::Qemu, v0), ("vmbr0".into(), "10".into(), "BC:24:11:00:00:01".into()));
        // The bridge changes, the tag goes, the MAC stays; firewall stays.
        assert_eq!(net_apply(Kind::Qemu, v0, "vmbr2", "", ""), "virtio=BC:24:11:00:00:01,bridge=vmbr2,firewall=1");
        // A tag is added, the MAC changes.
        assert_eq!(net_apply(Kind::Qemu, &cfg[2].1, "vmbr1", "20", "aa:bb:cc:00:00:01"), "e1000=aa:bb:cc:00:00:01,bridge=vmbr1,tag=20");
        let ct = "name=eth0,bridge=vmbr0,hwaddr=BC:24:11:00:00:09,ip=dhcp,type=veth";
        assert_eq!(net_fields(Kind::Lxc, ct), ("vmbr0".into(), "".into(), "BC:24:11:00:00:09".into()));
        assert_eq!(
            net_apply(Kind::Lxc, ct, "vmbr3", "5", "bc:24:11:00:00:0a"),
            "name=eth0,bridge=vmbr3,hwaddr=bc:24:11:00:00:0a,ip=dhcp,type=veth,tag=5"
        );
        assert_eq!(net_cmd(Kind::Lxc, 101, "net0", "name=eth0").args, ["set", "101", "--net0", "name=eth0"]);
        assert!(valid_mac("bc:24:11:00:00:0a") && !valid_mac("bc:24:11:00:00") && !valid_mac("zz:24:11:00:00:0a"));
    }

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
