//! The questions of the writing operations: a snapshot (name, with the memory or not), a backup (storage, mode,
//! compression), the snapshot list (delete, roll back) and the last y/n that shows what it costs. Pure state and keys; the
//! main loop opens them and runs the answer on a thread of its own.

use ratatui::crossterm::event::KeyCode;
use yagura_core::proxmox::ops::{self, Compress, Dump, Mode, Snap, SnapOp};
use yagura_core::proxmox::{Guest, Kind, Storage};

use crate::model::bytes;

/// The guest an operation is for, with the numbers its capacity hint needs.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub id: u32,
    pub name: String,
    pub kind: Kind,
    pub node: String,
    pub maxmem: u64,
    /// The data the guest holds on its disks.
    pub used: u64,
    pub template: bool,
}

impl Target {
    pub fn of(g: &Guest) -> Target {
        Target { id: g.id, name: g.name.clone(), kind: g.kind, node: g.node.clone(), maxmem: g.maxmem, used: g.disk, template: g.template }
    }
}

/// What the last question will run.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Snap(SnapOp),
    Dump(Dump),
}

impl Op {
    pub fn label(&self, t: &Target) -> String {
        match self {
            Op::Snap(s) => format!("{}: {} {}", t.id, s.verb(), snap_name(s)),
            Op::Dump(d) => format!("{}: backup to {}", t.id, d.storage),
        }
    }
}

fn snap_name(s: &SnapOp) -> &str {
    match s {
        SnapOp::Create { name, .. } | SnapOp::Delete(name) | SnapOp::Rollback(name) => name,
    }
}

/// The backup form.
#[derive(Clone, Debug, PartialEq)]
pub struct DumpForm {
    pub t: Target,
    /// Storages a backup can go to: name and free bytes.
    pub stores: Vec<(String, u64)>,
    pub store: usize,
    pub mode: usize,
    pub comp: usize,
    /// The size of the newest backup of the guest: `None` while it is read, `Some(None)` when there is none.
    pub last: Option<Option<u64>>,
}

impl DumpForm {
    pub fn new(t: Target, storage: &[Storage]) -> DumpForm {
        let stores = ops::backup_storages(storage, &t.node).into_iter().map(|s| (s.name.clone(), s.total.saturating_sub(s.used))).collect();
        DumpForm { t, stores, store: 0, mode: 0, comp: 0, last: None }
    }

    pub fn dump(&self) -> Option<Dump> {
        Some(Dump { storage: self.stores.get(self.store)?.0.clone(), mode: Mode::ALL[self.mode], compress: Compress::ALL[self.comp] })
    }

    /// The expected size of the file, and whether it is a guess from the guest's data rather than the last backup.
    pub fn estimate(&self) -> (u64, bool) {
        let last = self.last.flatten();
        (ops::backup_estimate(last, self.t.used, Compress::ALL[self.comp]), last.is_none())
    }
}

/// The last y/n: the lines to show and which of them is a warning.
#[derive(Clone, Debug, PartialEq)]
pub struct Sure {
    pub t: Target,
    pub op: Op,
    pub lines: Vec<(String, bool)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Dialog {
    Name { t: Target, text: String },
    Ram { t: Target, name: String },
    Dump(DumpForm),
    Snaps { t: Target, list: Option<Vec<Snap>>, sel: usize },
    Sure(Sure),
}

/// What a key leaves behind.
pub enum Next {
    Nothing,
    Run(Target, Op),
}

/// The name a new snapshot gets unless the user types one.
pub fn default_name(now: u64) -> String {
    let days = (now / 86_400) as i64;
    let (y, m, d) = civil(days);
    let s = now % 86_400;
    format!("yagura-{y:04}{m:02}{d:02}-{:02}{:02}", s / 3600, s % 3600 / 60)
}

/// Year, month and day of a day count since 1970-01-01 (UTC).
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// The last question for a snapshot operation. `free` is the room left on the storages that hold the guest's disks.
pub fn sure_snap(t: &Target, op: SnapOp, free: Option<(String, u64)>) -> Sure {
    let mut lines = vec![(format!("{} {} ({}) of {}", op.verb(), snap_name(&op), t.id, t.name), false)];
    match &op {
        SnapOp::Create { vmstate, .. } => {
            lines.push(("a snapshot costs almost nothing at first and grows as the guest changes data".into(), false));
            if *vmstate && t.maxmem > 0 {
                let short = free.as_ref().is_some_and(|(_, f)| *f < t.maxmem);
                lines.push((
                    format!(
                        "with memory: about {} more for the RAM state{}",
                        bytes(t.maxmem),
                        if short { " - MORE THAN IS FREE" } else { "" }
                    ),
                    short,
                ));
            }
        }
        SnapOp::Delete(_) => lines.push(("the data only this snapshot holds is lost; the guest is not touched".into(), false)),
        SnapOp::Rollback(_) => {
            lines.push(("the guest goes back to that state; everything changed since is LOST, newer snapshots are removed".into(), true))
        }
    }
    if let Some((name, f)) = free {
        let low = f < 1 << 30;
        lines.push((format!("room on {name}: {} free{}", bytes(f), if low { " - LOW" } else { "" }), low));
    }
    Sure { t: t.clone(), op: Op::Snap(op), lines }
}

/// The last question for a backup.
pub fn sure_dump(f: &DumpForm) -> Option<Sure> {
    let d = f.dump()?;
    let (est, guess) = f.estimate();
    let free = f.stores[f.store].1;
    let short = est > free;
    let mut lines = vec![
        (format!("back up {} ({}) to {} - mode {}, compress {}", f.t.id, f.t.name, d.storage, d.mode.name(), d.compress.name()), false),
        (format!("{}; the task shows in the Tasks screen (Tab)", d.mode.note()), d.mode != Mode::Snapshot),
        (
            format!(
                "size: about {} ({}), room on {}: {} free{}",
                bytes(est),
                if guess { "a guess from the data on its disks" } else { "the size of its last backup" },
                d.storage,
                bytes(free),
                if short { " - NOT ENOUGH" } else { "" }
            ),
            short,
        ),
    ];
    if f.t.kind == Kind::Qemu && d.mode == Mode::Snapshot {
        lines.push(("a running VM keeps running; the backup is consistent as of the start".into(), false));
    }
    Some(Sure { t: f.t.clone(), op: Op::Dump(d), lines })
}

/// A key in a dialog: the dialog that stays (or none), and what to run.
pub fn key(d: Dialog, code: KeyCode, free: impl Fn(&Target) -> Option<(String, u64)>) -> (Option<Dialog>, Next) {
    match d {
        Dialog::Name { t, mut text } => match code {
            KeyCode::Esc => (None, Next::Nothing),
            KeyCode::Char(c) if (c.is_ascii_alphanumeric() || c == '-' || c == '_') && text.len() < 40 => {
                text.push(c);
                (Some(Dialog::Name { t, text }), Next::Nothing)
            }
            KeyCode::Backspace => {
                text.pop();
                (Some(Dialog::Name { t, text }), Next::Nothing)
            }
            KeyCode::Enter if ops::valid_snapname(&text) => {
                if t.kind == Kind::Qemu {
                    (Some(Dialog::Ram { t, name: text }), Next::Nothing)
                } else {
                    let f = free(&t);
                    (Some(Dialog::Sure(sure_snap(&t, SnapOp::Create { name: text, vmstate: false }, f))), Next::Nothing)
                }
            }
            _ => (Some(Dialog::Name { t, text }), Next::Nothing),
        },
        Dialog::Ram { t, name } => match code {
            KeyCode::Char(c @ ('y' | 'n')) => {
                let f = free(&t);
                (Some(Dialog::Sure(sure_snap(&t, SnapOp::Create { name, vmstate: c == 'y' }, f))), Next::Nothing)
            }
            KeyCode::Esc => (None, Next::Nothing),
            _ => (Some(Dialog::Ram { t, name }), Next::Nothing),
        },
        Dialog::Dump(mut f) => match code {
            KeyCode::Esc | KeyCode::Char('q') => (None, Next::Nothing),
            KeyCode::Char('j') | KeyCode::Down if !f.stores.is_empty() => {
                f.store = (f.store + 1) % f.stores.len();
                (Some(Dialog::Dump(f)), Next::Nothing)
            }
            KeyCode::Char('k') | KeyCode::Up if !f.stores.is_empty() => {
                f.store = (f.store + f.stores.len() - 1) % f.stores.len();
                (Some(Dialog::Dump(f)), Next::Nothing)
            }
            KeyCode::Char('m') => {
                f.mode = (f.mode + 1) % Mode::ALL.len();
                (Some(Dialog::Dump(f)), Next::Nothing)
            }
            KeyCode::Char('c') => {
                f.comp = (f.comp + 1) % Compress::ALL.len();
                (Some(Dialog::Dump(f)), Next::Nothing)
            }
            KeyCode::Enter => match sure_dump(&f) {
                Some(s) => (Some(Dialog::Sure(s)), Next::Nothing),
                None => (Some(Dialog::Dump(f)), Next::Nothing),
            },
            _ => (Some(Dialog::Dump(f)), Next::Nothing),
        },
        Dialog::Snaps { t, list, mut sel } => {
            let n = list.as_ref().map_or(0, Vec::len);
            let pick = |list: &Option<Vec<Snap>>, sel: usize| list.as_ref().and_then(|l| l.get(sel)).map(|s| s.name.clone());
            match code {
                KeyCode::Esc | KeyCode::Char('q') => (None, Next::Nothing),
                KeyCode::Char('j') | KeyCode::Down => {
                    sel = (sel + 1).min(n.saturating_sub(1));
                    (Some(Dialog::Snaps { t, list, sel }), Next::Nothing)
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    sel = sel.saturating_sub(1);
                    (Some(Dialog::Snaps { t, list, sel }), Next::Nothing)
                }
                KeyCode::Char(c @ ('d' | 'r')) if pick(&list, sel).is_some() => {
                    let name = pick(&list, sel).unwrap_or_default();
                    let op = if c == 'd' { SnapOp::Delete(name) } else { SnapOp::Rollback(name) };
                    let f = free(&t);
                    (Some(Dialog::Sure(sure_snap(&t, op, f))), Next::Nothing)
                }
                _ => (Some(Dialog::Snaps { t, list, sel }), Next::Nothing),
            }
        }
        Dialog::Sure(s) => match code {
            KeyCode::Char('y') => (None, Next::Run(s.t, s.op)),
            _ => (None, Next::Nothing),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(kind: Kind) -> Target {
        Target { id: 101, name: "immich".into(), kind, node: "pve".into(), maxmem: 4 << 30, used: 10 << 30, template: false }
    }

    fn stor(name: &str, used: u64, total: u64) -> Storage {
        Storage { name: name.into(), node: "pve".into(), used, total, active: true, kind: "dir".into(), content: "backup".into() }
    }

    #[test]
    fn names_come_from_the_clock() {
        assert_eq!(default_name(0), "yagura-19700101-0000");
        assert_eq!(default_name(1_793_178_900), "yagura-20261028-0915");
        assert!(ops::valid_snapname(&default_name(1_793_178_900)));
    }

    #[test]
    fn snapshot_dialog_walks_to_a_confirmation() {
        let none = |_: &Target| Some(("local".to_string(), 100 << 30));
        let (d, _) = key(Dialog::Name { t: target(Kind::Qemu), text: "ab".into() }, KeyCode::Char('c'), none);
        let (d, _) = key(d.unwrap(), KeyCode::Enter, none);
        assert!(matches!(d, Some(Dialog::Ram { .. })));
        let (d, _) = key(d.unwrap(), KeyCode::Char('y'), none);
        let Some(Dialog::Sure(s)) = d else { panic!("no confirmation") };
        assert_eq!(s.op, Op::Snap(SnapOp::Create { name: "abc".into(), vmstate: true }));
        assert!(s.lines.iter().any(|(l, _)| l.contains("4.0") || l.contains("RAM")));
        let (d, n) = key(Dialog::Sure(s), KeyCode::Char('y'), none);
        assert!(d.is_none() && matches!(n, Next::Run(..)));
        // A container has no memory question; a bad name does not go on; any other key cancels.
        let (d, _) = key(Dialog::Name { t: target(Kind::Lxc), text: "x".into() }, KeyCode::Enter, none);
        assert!(matches!(d, Some(Dialog::Sure(_))));
        let (d, _) = key(Dialog::Name { t: target(Kind::Lxc), text: String::new() }, KeyCode::Enter, none);
        assert!(matches!(d, Some(Dialog::Name { .. })));
        let (d, n) = key(Dialog::Sure(sure_snap(&target(Kind::Lxc), SnapOp::Delete("a".into()), None)), KeyCode::Char('n'), none);
        assert!(d.is_none() && matches!(n, Next::Nothing));
    }

    #[test]
    fn backup_form_warns_when_the_target_is_short() {
        let mut f = DumpForm::new(target(Kind::Lxc), &[stor("small", 99 << 30, 100 << 30), stor("big", 0, 500 << 30)]);
        assert_eq!(f.stores[0].0, "big");
        assert_eq!(f.estimate(), (5 << 30, true));
        f.last = Some(Some(3 << 30));
        assert_eq!(f.estimate(), (3 << 30, false));
        assert!(sure_dump(&f).unwrap().lines.iter().all(|(_, warn)| !warn));
        f.store = 1;
        f.last = Some(Some(8 << 30));
        let s = sure_dump(&f).unwrap();
        assert!(s.lines.iter().any(|(l, warn)| *warn && l.contains("NOT ENOUGH")));
        assert_eq!(s.op, Op::Dump(Dump { storage: "small".into(), mode: Mode::Snapshot, compress: Compress::Zstd }));
        f.mode = 2;
        assert!(sure_dump(&f).unwrap().lines[1].1);
    }
}
