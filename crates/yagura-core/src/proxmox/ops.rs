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
}
