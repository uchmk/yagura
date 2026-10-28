//! The questions of the writing operations: a snapshot (name, with the memory or not), a backup (storage, mode,
//! compression), the snapshot list (delete, roll back) and the last y/n that shows what it costs. Pure state and keys; the
//! main loop opens them and runs the answer on a thread of its own.

use std::collections::BTreeSet;

use ratatui::crossterm::event::KeyCode;
use yagura_core::proxmox;
use yagura_core::proxmox::ops::{self, Backup, Cmd, Compress, Dump, Item, Mode, Snap, SnapOp};
use yagura_core::proxmox::{Guest, HostTask, Kind, Power, Storage};

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

    /// No guest: the question is about a task, a storage or the node (id 0 never matches a guest).
    pub fn none() -> Target {
        Target { id: 0, name: String::new(), kind: Kind::Qemu, node: String::new(), maxmem: 0, used: 0, template: false }
    }
}

/// What the last question will run.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Snap(SnapOp),
    Dump(Dump),
    /// Commands run one after the other, under a label for the status line.
    Cmds {
        label: String,
        cmds: Vec<Cmd>,
    },
}

impl Op {
    pub fn label(&self, t: &Target) -> String {
        match self {
            Op::Snap(s) => format!("{}: {} {}", t.id, s.verb(), snap_name(s)),
            Op::Dump(d) => format!("{}: backup to {}", t.id, d.storage),
            Op::Cmds { label, .. } => label.clone(),
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
    Name {
        t: Target,
        text: String,
    },
    Ram {
        t: Target,
        name: String,
    },
    Dump(DumpForm),
    Snaps {
        t: Target,
        list: Option<Vec<Snap>>,
        sel: usize,
    },
    /// The backup files of a storage: the list (`None` while it is read), the cursor and the marked rows.
    /// A guest's configuration, read only (`None` while it is read, empty when it could not be); `top` is the first row shown.
    Config {
        t: Target,
        list: Option<Vec<(String, String)>>,
        top: usize,
    },
    /// Everything on a storage (`None` while it is read), the cursor, and how full the storage is (used, total).
    Content {
        node: String,
        storage: String,
        fill: (u64, u64),
        list: Option<Vec<ops::Vol>>,
        sel: usize,
    },
    /// The firewall of a guest: whether it is on and its rules (`None` while they are read), scrolled to `top`.
    Firewall {
        t: Target,
        state: Option<(bool, Vec<ops::FwRule>)>,
        top: usize,
    },
    /// The network devices of a guest (`None` while the config is read), the one under the cursor and its bridge, VLAN tag and MAC;
    /// `field` is the one being typed (0 bridge, 1 VLAN, 2 MAC).
    Net {
        t: Target,
        nets: Option<Vec<(String, String)>>,
        sel: usize,
        field: usize,
        bridge: String,
        vlan: String,
        mac: String,
    },
    /// A copy of a guest: the new name and whether the disks are copied (`full`) or linked (templates only).
    Clone {
        t: Target,
        name: String,
        full: bool,
    },
    /// Grow a disk of a guest: its disks (`None` while the config is read), the cursor and the GiB typed.
    Resize {
        t: Target,
        disks: Option<Vec<(String, String)>>,
        sel: usize,
        gib: String,
    },
    /// Memory and cores to set on these guests.
    Edit(EditForm),
    /// The node menu: maintenance mode, and starting or shutting down every guest of the node.
    Node {
        node: String,
        stopped: Vec<Target>,
        running: Vec<Target>,
    },
    Backups {
        node: String,
        storage: String,
        list: Option<Vec<Backup>>,
        sel: usize,
        marks: BTreeSet<usize>,
    },
    Sure(Sure),
}

/// What a key leaves behind.
pub enum Next {
    Nothing,
    Run(Target, Op),
    /// Restore this backup: the main loop looks up a free guest id first.
    Restore(Backup),
    /// Clone this guest under this name (`full` or linked): the main loop looks up a free guest id first.
    CloneId(Target, String, bool),
}

/// `YYYY-MM-DD HH:MM` (UTC) of a Unix time.
pub fn stamp(t: u64) -> String {
    let (y, m, d) = civil((t / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", t % 86_400 / 3600, t % 3600 / 60)
}

/// The file name of a backup without the storage prefix.
fn file(b: &Backup) -> &str {
    b.volid.rsplit('/').next().unwrap_or(&b.volid)
}

/// The last question for deleting backup files; protected ones are left out. `None` when nothing is left.
pub fn sure_backup_delete(picked: &[&Backup]) -> Option<Sure> {
    let (prot, go): (Vec<&Backup>, Vec<&Backup>) = picked.iter().partition(|b| b.protected);
    if go.is_empty() {
        return None;
    }
    let mut lines = vec![(format!("delete {} backup files from {}", go.len(), go[0].storage), false)];
    for b in go.iter().take(LISTED) {
        lines.push((format!("  {} {}  ({})", b.vmid, file(b), bytes(b.size)), false));
    }
    if go.len() > LISTED {
        lines.push((format!("  ... and {} more", go.len() - LISTED), false));
    }
    if !prot.is_empty() {
        lines.push((format!("{} protected backups are left alone", prot.len()), false));
    }
    lines.push((format!("gives back about {}; a deleted backup can not be brought back", bytes(go.iter().map(|b| b.size).sum())), true));
    let label = format!("delete {} backups", go.len());
    let cmds = go.iter().map(|b| ops::backup_delete_cmd(b)).collect();
    Some(Sure { t: Target::none(), op: Op::Cmds { label, cmds }, lines })
}

/// The last question for restoring a backup as the new guest `newid` (never over an existing one).
pub fn sure_restore(b: &Backup, newid: u32) -> Option<Sure> {
    let cmd = ops::restore_cmd(b, newid, None)?;
    let kind = if b.kind() == Some(Kind::Lxc) { "container" } else { "VM" };
    let lines = vec![
        (format!("restore {} as a new {kind} {newid}", file(b)), false),
        (format!("the backup of {} taken {} UTC, {}", b.vmid, stamp(b.ctime), bytes(b.size)), false),
        (format!("{newid} is a free id: no existing guest is touched; the copy needs about {} on its storage", bytes(b.size)), false),
        ("the new guest starts stopped; give it a different network address before starting it".into(), true),
    ];
    Some(Sure { t: Target::none(), op: Op::Cmds { label: format!("restore as {newid}"), cmds: vec![cmd] }, lines })
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

/// The last question for stopping a running task.
pub fn sure_stop(t: &HostTask) -> Sure {
    let what = if t.id.is_empty() { t.kind.clone() } else { format!("{} {}", t.kind, t.id) };
    let lines = vec![
        (format!("stop the running task {what} on {}", t.node), false),
        ("the work done so far is abandoned; a backup that is cut off is not kept".into(), t.kind == "vzdump"),
    ];
    let cmds = vec![ops::stop_task_cmd(&t.node, &t.upid)];
    Sure { t: Target::none(), op: Op::Cmds { label: format!("stop {what}"), cmds }, lines }
}

/// The form for memory (MiB) and cores; a blank field keeps the guest's value.
#[derive(Clone, Debug, PartialEq)]
pub struct EditForm {
    pub targets: Vec<Target>,
    pub mem: String,
    pub cores: String,
    /// Containers only.
    pub swap: String,
    pub cpulimit: String,
    /// 0 = memory, 1 = cores, 2 = swap, 3 = CPU limit.
    pub field: usize,
}

impl EditForm {
    pub fn new(targets: Vec<Target>) -> EditForm {
        EditForm { targets, mem: String::new(), cores: String::new(), swap: String::new(), cpulimit: String::new(), field: 0 }
    }

    fn field_mut(&mut self) -> &mut String {
        match self.field {
            0 => &mut self.mem,
            1 => &mut self.cores,
            2 => &mut self.swap,
            _ => &mut self.cpulimit,
        }
    }
}

/// The last question for a memory and cores change; `None` when both fields are blank or out of range.
pub fn sure_edit(f: &EditForm) -> Option<Sure> {
    let num = |s: &str, lo: u32, hi: u32| {
        if s.is_empty() { Some(None) } else { s.parse::<u32>().ok().filter(|v| (lo..=hi).contains(v)).map(Some) }
    };
    let (mem, cores) = (num(&f.mem, 16, 4_194_304)?, num(&f.cores, 1, 512)?);
    let (swap, cpulimit) = (num(&f.swap, 0, 4_194_304)?, num(&f.cpulimit, 0, 512)?);
    let o = ops::SetOpts { mem_mib: mem, cores, swap_mib: swap, cpulimit };
    if o == ops::SetOpts::default() {
        return None;
    }
    let mut what = vec![];
    if let Some(m) = mem {
        what.push(format!("memory {m} MiB"));
    }
    if let Some(c) = cores {
        what.push(format!("{c} cores"));
    }
    if let Some(s) = swap {
        what.push(format!("swap {s} MiB (containers only)"));
    }
    if let Some(l) = cpulimit {
        what.push(if l == 0 { "no CPU limit".to_string() } else { format!("CPU limit {l}") });
    }
    let what = what.join(", ");
    let mut lines = vec![(format!("set {what} on {} guests", f.targets.len()), false)];
    for t in f.targets.iter().take(LISTED) {
        lines.push((
            format!("  {} {} ({} now {})", t.id, t.name, if t.kind == Kind::Lxc { "container" } else { "VM" }, bytes(t.maxmem)),
            false,
        ));
    }
    if f.targets.len() > LISTED {
        lines.push((format!("  ... and {} more", f.targets.len() - LISTED), false));
    }
    lines.push(("a running guest takes most of this only after a restart".into(), true));
    let cmds = f.targets.iter().map(|t| ops::set_cmd(t.kind, t.id, o)).collect();
    Some(Sure { t: Target::none(), op: Op::Cmds { label: format!("set {what}"), cmds }, lines })
}

/// The last question for growing the disk under the cursor by `gib` GiB; `None` for no disk or a number outside 1..=65536.
pub fn sure_resize(t: &Target, disks: &[(String, String)], sel: usize, gib: &str) -> Option<Sure> {
    let (key, value) = disks.get(sel)?;
    let gib: u32 = gib.parse().ok().filter(|g| (1..=65536).contains(g))?;
    let lines = vec![
        (format!("grow {key} of {} {} by {gib} GiB", t.id, t.name), false),
        (format!("  now {value}"), false),
        ("a disk cannot be shrunk again; a VM needs its partition and filesystem grown inside the guest".into(), true),
    ];
    Some(Sure {
        t: Target::none(),
        op: Op::Cmds { label: format!("grow {key} by {gib} GiB"), cmds: vec![ops::resize_cmd(t.kind, t.id, key, gib)] },
        lines,
    })
}

/// The last question for the maintenance mode of a node (HA clusters only; the tool says so otherwise).
pub fn sure_maintenance(node: &str, on: bool) -> Sure {
    let (verb, note) = if on {
        ("turn maintenance mode on for", "HA guests are moved to other nodes; needs an HA cluster")
    } else {
        ("turn maintenance mode off for", "HA guests may move back to the node; needs an HA cluster")
    };
    let lines = vec![(format!("{verb} node {node}"), false), (note.to_string(), true)];
    Sure {
        t: Target::none(),
        op: Op::Cmds { label: format!("maintenance {}", if on { "on" } else { "off" }), cmds: vec![ops::maintenance_cmd(node, on)] },
        lines,
    }
}

/// The last question for deleting one volume of a storage.
pub fn sure_vol(v: &ops::Vol) -> Sure {
    let mut lines = vec![(format!("delete {} ({}, {})", v.volid, v.content, bytes(v.size)), false)];
    if matches!(v.content.as_str(), "images" | "rootdir") {
        lines.push((
            format!("this is a guest disk{}; the guest loses it", if v.vmid > 0 { format!(" of {}", v.vmid) } else { String::new() }),
            true,
        ));
    }
    lines.push(("a deleted volume can not be brought back".into(), true));
    Sure { t: Target::none(), op: Op::Cmds { label: format!("delete {}", v.volid), cmds: vec![ops::vol_delete_cmd(v)] }, lines }
}

/// The last question for switching the firewall of `t` on or off.
pub fn sure_firewall(t: &Target, on: bool) -> Sure {
    let mut lines = vec![(format!("turn the firewall of {} {} {}", t.id, t.name, if on { "on" } else { "off" }), false)];
    if on {
        lines.push(("with no rule that lets traffic in, the default policy drops it and the guest can be cut off".into(), true));
    }
    Sure {
        t: Target::none(),
        op: Op::Cmds { label: format!("firewall {}", t.id), cmds: vec![ops::firewall_cmd(&t.node, t.kind, t.id, on)] },
        lines,
    }
}

/// The last question for the network device under the cursor with the bridge, VLAN tag and MAC typed; `None` for a bad value.
pub fn sure_net(t: &Target, nets: &[(String, String)], sel: usize, bridge: &str, vlan: &str, mac: &str) -> Option<Sure> {
    let (dev, old) = nets.get(sel)?;
    if bridge.is_empty()
        || !(vlan.is_empty() || vlan.parse::<u32>().is_ok_and(|v| (1..=4094).contains(&v)))
        || !(mac.is_empty() || ops::valid_mac(mac))
    {
        return None;
    }
    let new = ops::net_apply(t.kind, old, bridge, vlan, mac);
    let mut lines =
        vec![(format!("{dev} of {} {}", t.id, t.name), false), (format!("  now {old}"), false), (format!("  new {new}"), false)];
    if new == *old {
        lines.push(("nothing changes".into(), false));
    }
    lines.push(("a new MAC or bridge can cut the guest off from the network until it is set right again".into(), true));
    Some(Sure { t: Target::none(), op: Op::Cmds { label: format!("set {dev}"), cmds: vec![ops::net_cmd(t.kind, t.id, dev, &new)] }, lines })
}

/// The last question for cloning `t` as guest `newid`.
pub fn sure_clone(t: &Target, name: &str, full: bool, newid: u32) -> Sure {
    let how = if full { "full copy of the disks" } else { "linked clone (shares the template's disks)" };
    let mut lines = vec![(format!("clone {} {} as {newid} {name}: {how}", t.id, t.name), false)];
    if full {
        lines.push((format!("copies about {} of disk data", bytes(t.used)), false));
    }
    let label = format!("clone as {newid}");
    Sure { t: Target::none(), op: Op::Cmds { label, cmds: vec![ops::clone_cmd(t.kind, t.id, newid, name, full)] }, lines }
}

/// The last question for turning the (stopped) guest `t` into a template.
pub fn sure_template(t: &Target) -> Sure {
    let lines = vec![
        (format!("turn {} {} into a template", t.id, t.name), false),
        ("a template can not be started again; clone it (Y) to get guests from it, linked clones share its disks".into(), true),
    ];
    Sure { t: Target::none(), op: Op::Cmds { label: format!("template {}", t.id), cmds: vec![ops::template_cmd(t.kind, t.id)] }, lines }
}

/// The last question for deleting the (stopped) guest `t` with its disks.
pub fn sure_destroy(t: &Target) -> Sure {
    let lines = vec![
        (format!("delete {} {} with all its disks", t.id, t.name), false),
        ("the guest and its disks can not be brought back; backup files and snapshots of other guests stay".into(), true),
    ];
    Sure { t: Target::none(), op: Op::Cmds { label: format!("delete {}", t.id), cmds: vec![ops::destroy_cmd(t.kind, t.id)] }, lines }
}

/// The last question for rebooting or shutting down the node, or refreshing its package lists.
pub fn sure_node(node: &str, c: ops::NodeCmd, running: usize) -> Sure {
    use ops::NodeCmd::*;
    let (what, label) = match c {
        Reboot => ("reboot", "reboot node"),
        Shutdown => ("shut down", "shut down node"),
        AptUpdate => ("refresh the package lists (apt update) of", "apt update"),
    };
    let mut lines = vec![(format!("{what} node {node}"), false)];
    if c != AptUpdate {
        lines.push((
            format!("{running} running guests stop with it (HA guests move or restart); a shutdown needs power or IPMI to start again"),
            true,
        ));
    }
    Sure { t: Target::none(), op: Op::Cmds { label: format!("{label} {node}"), cmds: vec![ops::node_cmd(node, c)] }, lines }
}

/// The last question for starting or shutting down every guest in `list`, one after the other; `None` for an empty list.
pub fn sure_all(node: &str, list: &[Target], start: bool) -> Option<Sure> {
    if list.is_empty() {
        return None;
    }
    let (power, verb) = if start { (Power::Start, "start") } else { (Power::Shutdown, "shut down") };
    let mut lines = vec![(format!("{verb} {} guests on {node}, one after the other", list.len()), false)];
    for t in list.iter().take(LISTED) {
        lines.push((format!("  {} {} ({})", t.id, t.name, if t.kind == Kind::Lxc { "container" } else { "VM" }), false));
    }
    if list.len() > LISTED {
        lines.push((format!("  ... and {} more", list.len() - LISTED), false));
    }
    if !start {
        lines.push(("a guest that does not stop stalls the rest; the first failure ends the run".into(), true));
    }
    let cmds = list
        .iter()
        .map(|t| {
            let (prog, [v, id]) = proxmox::power_command(t.kind, t.id, power);
            Cmd::new(prog, &[&v, &id])
        })
        .collect();
    Some(Sure { t: Target::none(), op: Op::Cmds { label: format!("{verb} all on {node}"), cmds }, lines })
}

/// How many items the cleanup question lists before it says "and N more".
const LISTED: usize = 12;

/// The last question for the cleanup: what goes, and how much room the backups give back.
pub fn sure_cleanup(items: Vec<Item>, snapshot_days: u64, keep: usize) -> Option<Sure> {
    if items.is_empty() {
        return None;
    }
    let freed: u64 = items.iter().map(|i| i.bytes).sum();
    let mut lines = vec![(
        format!(
            "remove {} old items: snapshots older than {snapshot_days} days, backups beyond the newest {keep} of each guest",
            items.len()
        ),
        false,
    )];
    for i in items.iter().take(LISTED) {
        let size = if i.bytes > 0 { format!("  ({})", bytes(i.bytes)) } else { String::new() };
        lines.push((format!("  {}{size}", i.what), false));
    }
    if items.len() > LISTED {
        lines.push((format!("  ... and {} more", items.len() - LISTED), false));
    }
    lines.push((format!("gives back about {} of backups; the size of a snapshot is not known", bytes(freed)), false));
    lines.push(("what is removed can not be brought back; protected backups are left alone".into(), true));
    let label = format!("cleanup of {} items", items.len());
    let cmds = items.into_iter().map(|i| i.cmd).collect();
    Some(Sure { t: Target::none(), op: Op::Cmds { label, cmds }, lines })
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
        Dialog::Config { t, list, top } => {
            let n = list.as_ref().map_or(0, Vec::len);
            let top = match code {
                KeyCode::Esc | KeyCode::Char('q') => return (None, Next::Nothing),
                KeyCode::Char('j') | KeyCode::Down => (top + 1).min(n.saturating_sub(1)),
                KeyCode::Char('k') | KeyCode::Up => top.saturating_sub(1),
                KeyCode::Char('g') | KeyCode::Home => 0,
                KeyCode::Char('G') | KeyCode::End => n.saturating_sub(1),
                _ => top,
            };
            (Some(Dialog::Config { t, list, top }), Next::Nothing)
        }
        Dialog::Content { node, storage, fill, list, mut sel } => {
            let n = list.as_ref().map_or(0, Vec::len);
            match code {
                KeyCode::Esc | KeyCode::Char('q') => return (None, Next::Nothing),
                KeyCode::Char('j') | KeyCode::Down => sel = (sel + 1).min(n.saturating_sub(1)),
                KeyCode::Char('k') | KeyCode::Up => sel = sel.saturating_sub(1),
                KeyCode::Char('d') => {
                    if let Some(v) = list.as_ref().and_then(|l| l.get(sel)) {
                        return (Some(Dialog::Sure(sure_vol(v))), Next::Nothing);
                    }
                }
                _ => {}
            }
            (Some(Dialog::Content { node, storage, fill, list, sel }), Next::Nothing)
        }
        Dialog::Firewall { t, state, mut top } => {
            let n = state.as_ref().map_or(0, |s| s.1.len());
            match code {
                KeyCode::Esc | KeyCode::Char('q') => return (None, Next::Nothing),
                KeyCode::Char('j') | KeyCode::Down => top = (top + 1).min(n.saturating_sub(1)),
                KeyCode::Char('k') | KeyCode::Up => top = top.saturating_sub(1),
                KeyCode::Enter | KeyCode::Char('f') => {
                    if let Some((on, _)) = &state {
                        return (Some(Dialog::Sure(sure_firewall(&t, !*on))), Next::Nothing);
                    }
                }
                _ => {}
            }
            (Some(Dialog::Firewall { t, state, top }), Next::Nothing)
        }
        Dialog::Net { t, nets, mut sel, mut field, mut bridge, mut vlan, mut mac } => {
            let n = nets.as_ref().map_or(0, Vec::len);
            let mut moved = false;
            match code {
                KeyCode::Esc => return (None, Next::Nothing),
                KeyCode::Down if sel + 1 < n => {
                    sel += 1;
                    moved = true;
                }
                KeyCode::Up if sel > 0 => {
                    sel -= 1;
                    moved = true;
                }
                KeyCode::Tab => field = (field + 1) % 3,
                KeyCode::BackTab => field = (field + 2) % 3,
                KeyCode::Backspace => {
                    [&mut bridge, &mut vlan, &mut mac][field].pop();
                }
                KeyCode::Char(c) => match field {
                    0 if (c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) && bridge.len() < 20 => bridge.push(c),
                    1 if c.is_ascii_digit() && vlan.len() < 4 => vlan.push(c),
                    2 if (c.is_ascii_hexdigit() || c == ':') && mac.len() < 17 => mac.push(c),
                    _ => {}
                },
                KeyCode::Enter => {
                    if let Some(s) = sure_net(&t, nets.as_deref().unwrap_or(&[]), sel, &bridge, &vlan, &mac) {
                        return (Some(Dialog::Sure(s)), Next::Nothing);
                    }
                }
                _ => {}
            }
            if moved && let Some((_, v)) = nets.as_ref().and_then(|l| l.get(sel)) {
                (bridge, vlan, mac) = ops::net_fields(t.kind, v);
            }
            (Some(Dialog::Net { t, nets, sel, field, bridge, vlan, mac }), Next::Nothing)
        }
        Dialog::Clone { t, mut name, mut full } => match code {
            KeyCode::Esc => (None, Next::Nothing),
            KeyCode::Char(c) if (c.is_ascii_alphanumeric() || c == '-' || c == '.') && name.len() < 63 => {
                name.push(c);
                (Some(Dialog::Clone { t, name, full }), Next::Nothing)
            }
            KeyCode::Backspace => {
                name.pop();
                (Some(Dialog::Clone { t, name, full }), Next::Nothing)
            }
            KeyCode::Tab => {
                // A guest that is not a template can only be copied in full.
                full = !full || !t.template;
                (Some(Dialog::Clone { t, name, full }), Next::Nothing)
            }
            KeyCode::Enter if ops::valid_guest_name(&name) => (None, Next::CloneId(t, name, full)),
            _ => (Some(Dialog::Clone { t, name, full }), Next::Nothing),
        },
        Dialog::Resize { t, disks, sel, mut gib } => {
            let n = disks.as_ref().map_or(0, Vec::len);
            let stay = |t, disks, sel, gib| (Some(Dialog::Resize { t, disks, sel, gib }), Next::Nothing);
            match code {
                KeyCode::Esc => (None, Next::Nothing),
                KeyCode::Char('j') | KeyCode::Down => stay(t, disks, (sel + 1).min(n.saturating_sub(1)), gib),
                KeyCode::Char('k') | KeyCode::Up => stay(t, disks, sel.saturating_sub(1), gib),
                KeyCode::Char(c) if c.is_ascii_digit() && gib.len() < 5 => {
                    gib.push(c);
                    stay(t, disks, sel, gib)
                }
                KeyCode::Backspace => {
                    gib.pop();
                    stay(t, disks, sel, gib)
                }
                KeyCode::Enter => match sure_resize(&t, disks.as_deref().unwrap_or(&[]), sel, &gib) {
                    Some(s) => (Some(Dialog::Sure(s)), Next::Nothing),
                    None => stay(t, disks, sel, gib),
                },
                _ => stay(t, disks, sel, gib),
            }
        }
        Dialog::Node { node, stopped, running } => {
            let sure = match code {
                KeyCode::Esc | KeyCode::Char('q') => return (None, Next::Nothing),
                KeyCode::Char('m') => Some(sure_maintenance(&node, true)),
                KeyCode::Char('M') => Some(sure_maintenance(&node, false)),
                KeyCode::Char('s') => sure_all(&node, &stopped, true),
                KeyCode::Char('d') => sure_all(&node, &running, false),
                KeyCode::Char('u') => Some(sure_node(&node, ops::NodeCmd::AptUpdate, running.len())),
                KeyCode::Char('R') => Some(sure_node(&node, ops::NodeCmd::Reboot, running.len())),
                KeyCode::Char('X') => Some(sure_node(&node, ops::NodeCmd::Shutdown, running.len())),
                _ => None,
            };
            match sure {
                Some(s) => (Some(Dialog::Sure(s)), Next::Nothing),
                None => (Some(Dialog::Node { node, stopped, running }), Next::Nothing),
            }
        }
        Dialog::Edit(mut f) => match code {
            KeyCode::Esc => (None, Next::Nothing),
            KeyCode::Tab | KeyCode::Down | KeyCode::Up => {
                f.field = if code == KeyCode::Up { (f.field + 3) % 4 } else { (f.field + 1) % 4 };
                (Some(Dialog::Edit(f)), Next::Nothing)
            }
            KeyCode::Char(c) if c.is_ascii_digit() && f.field_mut().len() < 7 => {
                f.field_mut().push(c);
                (Some(Dialog::Edit(f)), Next::Nothing)
            }
            KeyCode::Backspace => {
                f.field_mut().pop();
                (Some(Dialog::Edit(f)), Next::Nothing)
            }
            KeyCode::Enter => match sure_edit(&f) {
                Some(s) => (Some(Dialog::Sure(s)), Next::Nothing),
                None => (Some(Dialog::Edit(f)), Next::Nothing),
            },
            _ => (Some(Dialog::Edit(f)), Next::Nothing),
        },
        Dialog::Backups { node, storage, list, sel, mut marks } => {
            let n = list.as_ref().map_or(0, Vec::len);
            let stay = |node, storage, list, sel, marks| (Some(Dialog::Backups { node, storage, list, sel, marks }), Next::Nothing);
            match code {
                KeyCode::Esc | KeyCode::Char('q') => (None, Next::Nothing),
                KeyCode::Char('j') | KeyCode::Down => stay(node, storage, list, (sel + 1).min(n.saturating_sub(1)), marks),
                KeyCode::Char('k') | KeyCode::Up => stay(node, storage, list, sel.saturating_sub(1), marks),
                KeyCode::Char(' ') if sel < n => {
                    if !marks.remove(&sel) {
                        marks.insert(sel);
                    }
                    stay(node, storage, list, (sel + 1).min(n - 1), marks)
                }
                KeyCode::Char('d') if n > 0 => {
                    let l = list.as_deref().unwrap_or(&[]);
                    let picked: Vec<&Backup> =
                        if marks.is_empty() { l.get(sel).into_iter().collect() } else { marks.iter().filter_map(|i| l.get(*i)).collect() };
                    match sure_backup_delete(&picked) {
                        Some(s) => (Some(Dialog::Sure(s)), Next::Nothing),
                        None => stay(node, storage, list, sel, marks),
                    }
                }
                KeyCode::Char('r') if n > 0 => match list.as_ref().and_then(|l| l.get(sel)).filter(|b| b.kind().is_some()) {
                    Some(b) => (None, Next::Restore(b.clone())),
                    None => stay(node, storage, list, sel, marks),
                },
                _ => stay(node, storage, list, sel, marks),
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
    fn cleanup_and_task_stop_ask_before_running() {
        assert!(sure_cleanup(vec![], 30, 3).is_none());
        let item = |n: u32, bytes| Item { what: format!("backup {n}"), bytes, cmd: Cmd::new("pvesm", &["free", &n.to_string()]) };
        let s = sure_cleanup((0..15).map(|n| item(n, 1 << 30)).collect(), 30, 3).unwrap();
        assert!(s.lines.iter().any(|(l, _)| l.contains("and 3 more")));
        assert!(s.lines.iter().any(|(l, _)| l.contains("15.0") || l.contains("about 15")));
        let Op::Cmds { cmds, .. } = &s.op else { panic!("not commands") };
        assert_eq!(cmds.len(), 15);
        let (d, n) = key(Dialog::Sure(s), KeyCode::Char('y'), |_| None);
        assert!(d.is_none() && matches!(n, Next::Run(t, _) if t.id == 0));
        let t = HostTask { node: "pve".into(), upid: "UPID:pve:1".into(), kind: "vzdump".into(), id: "101".into(), ..Default::default() };
        let s = sure_stop(&t);
        assert_eq!(s.op.label(&s.t), "stop vzdump 101");
        assert!(s.lines[1].1);
        let (d, n) = key(Dialog::Sure(s), KeyCode::Char('n'), |_| None);
        assert!(d.is_none() && matches!(n, Next::Nothing));
    }

    #[test]
    fn backup_list_deletes_and_restores_behind_questions() {
        let b = |n: u32, ctime, protected| Backup {
            volid: format!("nas:backup/vzdump-lxc-{n}-x.tar.zst"),
            storage: "nas".into(),
            node: "pve".into(),
            vmid: n,
            ctime,
            size: 1 << 30,
            protected,
            notes: String::new(),
        };
        let none = |_: &Target| None;
        let open = |marks: &[usize]| Dialog::Backups {
            node: "pve".into(),
            storage: "nas".into(),
            list: Some(vec![b(100, 3, false), b(101, 2, true), b(102, 1, false)]),
            sel: 0,
            marks: marks.iter().copied().collect(),
        };
        // Space marks and moves on; d asks about the marked rows and leaves the protected one out.
        let (d, _) = key(open(&[]), KeyCode::Char(' '), none);
        let (d, _) = key(d.unwrap(), KeyCode::Char(' '), none);
        let (d, _) = key(d.unwrap(), KeyCode::Char('j'), none);
        let (d, _) = key(d.unwrap(), KeyCode::Char('d'), none);
        let Some(Dialog::Sure(s)) = d else { panic!("no question") };
        let Op::Cmds { cmds, .. } = &s.op else { panic!("not commands") };
        assert_eq!(cmds.len(), 1);
        assert!(s.lines.iter().any(|(l, _)| l.contains("1 protected")));
        // Only a protected backup under the cursor: nothing to ask, the list stays.
        let (d, _) = key(open(&[]), KeyCode::Char('j'), none);
        let (d, _) = key(d.unwrap(), KeyCode::Char('d'), none);
        assert!(matches!(d, Some(Dialog::Backups { .. })));
        let (d, n) = key(open(&[]), KeyCode::Char('r'), none);
        assert!(d.is_none() && matches!(n, Next::Restore(x) if x.vmid == 100));
        let s = sure_restore(&b(100, 1_793_178_900, false), 120).unwrap();
        assert!(s.lines[1].0.contains("2026-10-28 09:15"));
        assert!(matches!(&s.op, Op::Cmds { cmds, .. } if cmds[0].prog == "pct" && cmds[0].args[1] == "120"));
    }

    #[test]
    fn the_node_menu_asks_before_maintenance_and_before_starting_or_stopping_all() {
        let t = |id, kind| Target { id, name: format!("g{id}"), kind, node: "pve".into(), maxmem: 0, used: 0, template: false };
        let menu = || Dialog::Node { node: "pve".into(), stopped: vec![t(100, Kind::Qemu), t(101, Kind::Lxc)], running: vec![] };
        let (d, _) = key(menu(), KeyCode::Char('m'), |_| None);
        let Some(Dialog::Sure(s)) = d else { panic!("no question") };
        assert_eq!(s.op, Op::Cmds { label: "maintenance on".into(), cmds: vec![ops::maintenance_cmd("pve", true)] });
        let (d, _) = key(menu(), KeyCode::Char('s'), |_| None);
        let Some(Dialog::Sure(s)) = d else { panic!("no question") };
        let Op::Cmds { cmds, .. } = &s.op else { panic!("not commands") };
        assert_eq!(cmds, &vec![Cmd::new("qm", &["start", "100"]), Cmd::new("pct", &["start", "101"])]);
        // Nothing is running, so d asks nothing and the menu stays.
        let (d, _) = key(menu(), KeyCode::Char('d'), |_| None);
        assert!(matches!(d, Some(Dialog::Node { .. })));
        let (d, _) = key(menu(), KeyCode::Char('R'), |_| None);
        let Some(Dialog::Sure(s)) = d else { panic!("no question") };
        assert_eq!(s.op, Op::Cmds { label: "reboot node pve".into(), cmds: vec![ops::node_cmd("pve", ops::NodeCmd::Reboot)] });
        assert!(s.lines.iter().any(|l| l.1));
        let (d, _) = key(menu(), KeyCode::Char('u'), |_| None);
        let Some(Dialog::Sure(s)) = d else { panic!("no question") };
        assert_eq!(s.lines.len(), 1);
        let (d, _) = key(menu(), KeyCode::Esc, |_| None);
        assert!(d.is_none());
    }

    #[test]
    fn the_content_dialog_deletes_the_volume_under_the_cursor() {
        let none = |_: &Target| None;
        let v = |id: &str, content: &str, size| ops::Vol { volid: id.into(), content: content.into(), size, vmid: 0 };
        let list = vec![v("local:iso/a.iso", "iso", 9), v("local:100/vm-100-disk-0.raw", "images", 5)];
        let d = Dialog::Content { node: "pve".into(), storage: "local".into(), fill: (1, 2), list: Some(list), sel: 0 };
        let (d, _) = key(d, KeyCode::Down, none);
        let (d, _) = key(d.unwrap(), KeyCode::Char('d'), none);
        let Some(Dialog::Sure(s)) = d else { panic!("no question") };
        assert_eq!(
            s.op,
            Op::Cmds {
                label: "delete local:100/vm-100-disk-0.raw".into(),
                cmds: vec![Cmd::new("pvesm", &["free", "local:100/vm-100-disk-0.raw"])]
            }
        );
        // A guest disk gets the extra warning, an ISO only the general one.
        assert_eq!(s.lines.iter().filter(|l| l.1).count(), 2);
        assert_eq!(sure_vol(&v("local:iso/a.iso", "iso", 1)).lines.iter().filter(|l| l.1).count(), 1);
        let (d, _) = key(
            Dialog::Content { node: "pve".into(), storage: "local".into(), fill: (0, 0), list: None, sel: 0 },
            KeyCode::Char('d'),
            none,
        );
        assert!(matches!(d, Some(Dialog::Content { .. })));
    }

    #[test]
    fn the_firewall_dialog_toggles_behind_a_question() {
        let none = |_: &Target| None;
        let t = target(Kind::Qemu);
        // Nothing to toggle while it is read.
        let (d, _) = key(Dialog::Firewall { t: t.clone(), state: None, top: 0 }, KeyCode::Enter, none);
        assert!(matches!(d, Some(Dialog::Firewall { .. })));
        let rules = vec![ops::FwRule { pos: 0, enabled: true, line: "in ACCEPT SSH".into() }];
        let (d, _) = key(Dialog::Firewall { t: t.clone(), state: Some((false, rules.clone())), top: 0 }, KeyCode::Char('f'), none);
        let Some(Dialog::Sure(s)) = d else { panic!("no question") };
        assert_eq!(s.op, Op::Cmds { label: "firewall 101".into(), cmds: vec![ops::firewall_cmd(&t.node, t.kind, t.id, true)] });
        assert!(s.lines.iter().any(|l| l.1));
        // Turning it off has no warning.
        assert!(!sure_firewall(&t, false).lines.iter().any(|l| l.1));
        let (d, _) = key(Dialog::Firewall { t, state: Some((true, rules)), top: 0 }, KeyCode::Esc, none);
        assert!(d.is_none());
    }

    #[test]
    fn the_net_dialog_follows_the_cursor_and_asks_before_setting() {
        let none = |_: &Target| None;
        let nets = vec![
            ("net0".to_string(), "virtio=BC:24:11:00:00:01,bridge=vmbr0".to_string()),
            ("net1".to_string(), "virtio=BC:24:11:00:00:02,bridge=vmbr1,tag=7".to_string()),
        ];
        let d = Dialog::Net {
            t: target(Kind::Qemu),
            nets: Some(nets),
            sel: 0,
            field: 0,
            bridge: "vmbr0".into(),
            vlan: String::new(),
            mac: "BC:24:11:00:00:01".into(),
        };
        // Down shows the second device.
        let (d, _) = key(d, KeyCode::Down, none);
        let Some(Dialog::Net { ref bridge, ref vlan, sel: 1, .. }) = d else { panic!("moved wrong") };
        assert_eq!((bridge.as_str(), vlan.as_str()), ("vmbr1", "7"));
        // Letters go to the bridge, Tab moves to the VLAN, which takes digits only.
        let (d, _) = key(d.unwrap(), KeyCode::Char('x'), none);
        let (d, _) = key(d.unwrap(), KeyCode::Tab, none);
        let (d, _) = key(d.unwrap(), KeyCode::Char('z'), none);
        let (d, _) = key(d.unwrap(), KeyCode::Backspace, none);
        let (d, _) = key(d.unwrap(), KeyCode::Char('9'), none);
        let (d, _) = key(d.unwrap(), KeyCode::Enter, none);
        let Some(Dialog::Sure(s)) = d else { panic!("no question") };
        let want = ops::net_cmd(Kind::Qemu, 101, "net1", "virtio=BC:24:11:00:00:02,bridge=vmbr1x,tag=9");
        assert_eq!(s.op, Op::Cmds { label: "set net1".into(), cmds: vec![want] });
        assert!(s.lines.iter().any(|l| l.1));
        // A bad VLAN or MAC does not go on.
        let nets = vec![("net0".to_string(), "virtio=BC:24:11:00:00:01,bridge=vmbr0".to_string())];
        assert!(sure_net(&target(Kind::Qemu), &nets, 0, "vmbr0", "5000", "").is_none());
        assert!(sure_net(&target(Kind::Qemu), &nets, 0, "vmbr0", "", "nope").is_none());
        assert!(sure_net(&target(Kind::Qemu), &nets, 0, "", "", "").is_none());
    }

    #[test]
    fn the_clone_dialog_names_the_copy_and_looks_for_an_id() {
        let none = |_: &Target| None;
        let mut t = target(Kind::Qemu);
        let d = Dialog::Clone { t: t.clone(), name: "w".into(), full: true };
        // A plain guest stays a full copy.
        let (d, _) = key(d, KeyCode::Tab, none);
        let Some(Dialog::Clone { full, .. }) = d.clone() else { panic!("closed") };
        assert!(full);
        let (d, next) = key(d.unwrap(), KeyCode::Char('1'), none);
        assert!(matches!(next, Next::Nothing));
        let (d, next) = key(d.unwrap(), KeyCode::Enter, none);
        assert!(d.is_none());
        assert!(matches!(next, Next::CloneId(_, ref n, true) if n == "w1"));
        // A template can switch to a linked clone.
        t.template = true;
        let (d, _) = key(Dialog::Clone { t, name: "a".into(), full: true }, KeyCode::Tab, none);
        assert!(matches!(d, Some(Dialog::Clone { full: false, .. })));
        // An empty or bad name does not go on.
        let (d, next) = key(Dialog::Clone { t: target(Kind::Lxc), name: String::new(), full: true }, KeyCode::Enter, none);
        assert!(d.is_some() && matches!(next, Next::Nothing));
    }

    #[test]
    fn the_clone_and_delete_questions_say_what_happens() {
        let s = sure_clone(&target(Kind::Qemu), "web", false, 105);
        assert_eq!(s.op, Op::Cmds { label: "clone as 105".into(), cmds: vec![ops::clone_cmd(Kind::Qemu, 101, 105, "web", false)] });
        let s = sure_template(&target(Kind::Qemu));
        assert_eq!(s.op, Op::Cmds { label: "template 101".into(), cmds: vec![ops::template_cmd(Kind::Qemu, 101)] });
        assert!(s.lines.iter().any(|l| l.1));
        let s = sure_destroy(&target(Kind::Lxc));
        assert_eq!(s.op, Op::Cmds { label: "delete 101".into(), cmds: vec![ops::destroy_cmd(Kind::Lxc, 101)] });
        assert!(s.lines.iter().any(|l| l.1));
    }

    #[test]
    fn the_resize_dialog_picks_a_disk_and_asks_before_growing() {
        let none = |_: &Target| None;
        let disks = vec![("scsi0".to_string(), "a:d0,size=32G".to_string()), ("scsi1".to_string(), "a:d1,size=8G".to_string())];
        let d = Dialog::Resize { t: target(Kind::Qemu), disks: Some(disks), sel: 0, gib: String::new() };
        // No number yet: Enter stays.
        let (d, _) = key(d, KeyCode::Enter, none);
        let (d, _) = key(d.unwrap(), KeyCode::Char('j'), none);
        let (d, _) = key(d.unwrap(), KeyCode::Char('5'), none);
        let (d, _) = key(d.unwrap(), KeyCode::Enter, none);
        let Some(Dialog::Sure(s)) = d else { panic!("no question") };
        assert_eq!(s.op, Op::Cmds { label: "grow scsi1 by 5 GiB".into(), cmds: vec![ops::resize_cmd(Kind::Qemu, 101, "scsi1", 5)] });
    }

    #[test]
    fn the_edit_form_takes_digits_and_asks_before_setting() {
        let none = |_: &Target| None;
        let d = Dialog::Edit(EditForm::new(vec![target(Kind::Qemu), target(Kind::Lxc)]));
        // Blank fields: Enter stays on the form.
        let (d, _) = key(d, KeyCode::Enter, none);
        let (d, _) = key(d.unwrap(), KeyCode::Char('x'), none);
        let (d, _) = key(d.unwrap(), KeyCode::Char('2'), none);
        let (d, _) = key(d.unwrap(), KeyCode::Char('0'), none);
        let (d, _) = key(d.unwrap(), KeyCode::Char('4'), none);
        let (d, _) = key(d.unwrap(), KeyCode::Char('8'), none);
        let (d, _) = key(d.unwrap(), KeyCode::Tab, none);
        let (d, _) = key(d.unwrap(), KeyCode::Char('4'), none);
        let Some(Dialog::Edit(f)) = &d else { panic!("left the form") };
        assert_eq!((f.mem.as_str(), f.cores.as_str()), ("2048", "4"));
        let (d, _) = key(d.unwrap(), KeyCode::Enter, none);
        let Some(Dialog::Sure(s)) = d else { panic!("no question") };
        let Op::Cmds { cmds, .. } = &s.op else { panic!("not commands") };
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].prog, "qm");
        assert_eq!(cmds[1].prog, "pct");
        assert!(cmds[0].args.contains(&"2048".to_string()) && cmds[0].args.contains(&"4".to_string()));
        // Swap and the CPU limit are set too; the VM ignores the swap.
        let mut f = EditForm::new(vec![target(Kind::Qemu), target(Kind::Lxc)]);
        f.swap = "0".into();
        f.cpulimit = "2".into();
        let s = sure_edit(&f).expect("a question");
        let Op::Cmds { cmds, .. } = &s.op else { panic!("not commands") };
        assert_eq!(cmds[0].args, ["set", "101", "--cpulimit", "2"]);
        assert_eq!(cmds[1].args, ["set", "101", "--swap", "0", "--cpulimit", "2"]);
        // Out of range stays on the form.
        let mut f = EditForm::new(vec![target(Kind::Qemu)]);
        f.cores = "0".into();
        assert!(sure_edit(&f).is_none());
        // The configuration view scrolls and closes.
        let list = Some(vec![("a".to_string(), "1".to_string()), ("b".to_string(), "2".to_string())]);
        let (d, _) = key(Dialog::Config { t: target(Kind::Qemu), list, top: 0 }, KeyCode::Char('j'), none);
        assert!(matches!(d, Some(Dialog::Config { top: 1, .. })));
        assert!(key(d.unwrap(), KeyCode::Esc, none).0.is_none());
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
