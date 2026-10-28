//! yagura-tui: the host and its Proxmox VE guests on one screen, in the
//! terminal. The collection is `yagura-core`; this only draws. Everything
//! slow (`pvesh`, `pct exec`) runs on threads of its own and reports over a
//! channel, so the keys never wait.

mod config;
mod dialog;
mod model;
mod ui;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use yagura_core::Msg;
use yagura_core::proxmox::rrd::Point;
use yagura_core::proxmox::{self, Cluster, History, Inside, Power, Status};

use model::{HIST_EVERY, Model, Sort, View, bytes};

const PVE_EVERY: Duration = Duration::from_secs(5);
/// How often the graphs on screen are read again (the history has a point a minute).
const RRD_EVERY: Duration = Duration::from_secs(20);
/// How many `pct exec` run at once when the containers are looked into in the
/// background.
const PARALLEL: usize = 4;
const SAMPLE: Duration = Duration::from_secs(1);

/// One guest's answer to a power change: its ID, name and result.
pub type Answer = (u32, String, Result<(), String>);

pub struct App {
    pub model: Model,
    pub view: View,
    pub sort: Sort,
    pub selected: usize,
    /// Names the screen could not say because the work is still going.
    pub status: String,
    /// A power change waiting for y or n.
    pub confirm: Option<Ask>,
    /// When the graphs on screen were last asked for, and whether that is still running.
    pub rrd_at: Option<Instant>,
    pub rrd_busy: bool,
    /// The same for the processes inside the entered guest.
    pub procs_at: Option<Instant>,
    pub procs_busy: bool,
    /// The guest row that was selected when it was entered.
    pub guest_sel: usize,
    /// The key list is up.
    pub help: bool,
    /// Keys go to the name filter.
    pub filtering: bool,
    /// Power changes running now, with when they began.
    pub busy: Vec<(u32, Power, Instant)>,
    /// Guests marked with Space; the power keys act on all of them.
    pub marks: HashSet<u32>,
    /// The entered guest shows its log instead of its processes, and how many lines back from the newest it is scrolled.
    pub log: bool,
    pub log_back: usize,
    /// What needs a look now, and the keys already announced (a new key rings the bell once).
    pub alerts: Vec<(String, String)>,
    pub alerts_seen: HashSet<String>,
    /// A power change of several guests in flight: how many, and the answers so far.
    pub batch: Option<(usize, Vec<Answer>)>,
    /// The answers of the last finished batch, shown until a key is pressed.
    pub results: Option<Vec<Answer>>,
    /// The log of the task under the cursor in the task screen, over everything else.
    pub task_log: Option<TaskLog>,
    /// A snapshot or backup being asked for (name, form, list, last y/n).
    pub dialog: Option<dialog::Dialog>,
    /// The cleanup scan is running.
    pub cleaning: bool,
}

/// One task's log: `lines` is `None` while it is read (or when it could not be).
pub struct TaskLog {
    pub upid: String,
    pub title: String,
    pub lines: Option<Vec<String>>,
    pub loading: bool,
    /// How many lines up from the newest the view is.
    pub back: usize,
}

/// What the user asked of a guest, shown until they answer.
#[derive(Clone)]
pub struct Ask {
    pub id: u32,
    pub name: String,
    pub kind: proxmox::Kind,
    pub power: Power,
    /// What it will do, in a few words (shown with the question).
    pub note: String,
    /// More guests asked with the first one (id, name, kind), when several were marked.
    pub more: Vec<(u32, String, proxmox::Kind)>,
}

enum Job {
    Cluster(Cluster),
    Inside(u32, Inside),
    Power(u32, Power, Result<(), String>),
    Latest(String, Option<String>),
    HostRrd(Vec<Point>),
    GuestRrd(u32, Vec<Point>),
    GuestProcs(u32, Option<Vec<proxmox::GuestProc>>),
    /// A guest's task log and snapshots.
    History(u32, History),
    GuestLogs(u32, Option<Vec<String>>),
    HostTasks(Vec<proxmox::HostTask>),
    TaskLog(String, Option<Vec<String>>),
    BackupSize(u32, Option<u64>),
    Snaps(u32, Vec<proxmox::ops::Snap>),
    /// A snapshot or backup finished: guest, what it was, result.
    Op(u32, String, Result<(), String>),
    /// What the cleanup scan found to remove.
    Cleanup(Vec<proxmox::ops::Item>),
}

fn main() -> std::io::Result<()> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("yagura-tui {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!(
            "yagura-tui {}\n\nkeys: Tab view (host, guests, procs, storage, tasks)  j/k move  / find  s sort  Enter graphs  Esc back  Space mark  u upgrade  o start  d shutdown  r reboot  S snapshot  b backup  m snapshots  C cleanup  x stop task  l logs  ? help  q quit",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    let (rx, _ctl) = yagura_core::spawn(None, SAMPLE);
    let (jobs_tx, jobs) = unbounded();
    spawn_pve(jobs_tx.clone());

    let mut app = App {
        model: Model::default(),
        view: View::Guests,
        sort: Sort::Id,
        selected: 0,
        status: String::new(),
        confirm: None,
        rrd_at: None,
        rrd_busy: false,
        procs_at: None,
        procs_busy: false,
        guest_sel: 0,
        help: false,
        filtering: false,
        busy: vec![],
        marks: HashSet::new(),
        log: false,
        log_back: 0,
        alerts: vec![],
        alerts_seen: HashSet::new(),
        batch: None,
        results: None,
        task_log: None,
        dialog: None,
        cleaning: false,
    };
    let mut term = ratatui::init();
    // ratatui leaves the hardware cursor where the last cell was written;
    // some terminals (Tera Term) show it as a block over the table.
    let _ = term.hide_cursor();
    let r = run(&mut term, &mut app, &rx, &jobs, &jobs_tx);
    let _ = term.show_cursor();
    ratatui::restore();
    r
}

/// `pvesh` every few seconds, on its own thread; ends when the screen does.
fn spawn_pve(tx: Sender<Job>) {
    std::thread::Builder::new()
        .name("yagura-pve".into())
        .spawn(move || {
            loop {
                if tx.send(Job::Cluster(proxmox::read())).is_err() {
                    return;
                }
                std::thread::sleep(PVE_EVERY);
            }
        })
        .expect("spawn the pve thread");
}

fn run(
    term: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rx: &Receiver<Msg>,
    jobs: &Receiver<Job>,
    jobs_tx: &Sender<Job>,
) -> std::io::Result<()> {
    loop {
        for m in rx.try_iter() {
            apply(&mut app.model, m);
        }
        for j in jobs.try_iter() {
            match j {
                Job::Cluster(c) => app.model.set_cluster(c, Instant::now()),
                Job::Inside(id, i) => {
                    app.model.looking.retain(|x| *x != id);
                    for (name, _) in &i.apps {
                        start_latest(&mut app.model, name, jobs_tx);
                    }
                    app.model.inside.insert(id, i);
                }
                Job::History(id, h) => {
                    app.model.hist_looking.retain(|x| *x != id);
                    app.model.hist.insert(id, (h, Instant::now()));
                }
                Job::GuestLogs(id, l) => {
                    app.procs_busy = false;
                    app.model.guest_logs.insert(id, l);
                }
                Job::HostTasks(t) => {
                    app.model.tasks_busy = false;
                    app.model.host_tasks = t;
                }
                Job::TaskLog(upid, l) => {
                    if let Some(t) = app.task_log.as_mut().filter(|t| t.upid == upid) {
                        t.loading = false;
                        t.lines = l;
                    }
                }
                Job::BackupSize(id, size) => {
                    if let Some(dialog::Dialog::Dump(f)) =
                        app.dialog.as_mut().filter(|d| matches!(d, dialog::Dialog::Dump(f) if f.t.id == id))
                    {
                        f.last = Some(size);
                    }
                }
                Job::Snaps(id, l) => {
                    if let Some(dialog::Dialog::Snaps { list, .. }) =
                        app.dialog.as_mut().filter(|d| matches!(d, dialog::Dialog::Snaps { t, .. } if t.id == id))
                    {
                        *list = Some(l);
                    }
                }
                Job::Op(id, label, r) => {
                    app.model.tasks_at = None;
                    if let Some(e) = app.model.hist.get_mut(&id) {
                        e.1 = e.1.checked_sub(HIST_EVERY).unwrap_or(e.1);
                    }
                    app.status = match r {
                        Ok(()) => format!("{label} done"),
                        Err(e) => format!("{label} failed: {e}"),
                    };
                }
                Job::Cleanup(items) => {
                    app.cleaning = false;
                    let c = config::get();
                    match dialog::sure_cleanup(items, c.snapshot_days, c.keep_backups) {
                        Some(s) => {
                            app.status.clear();
                            app.dialog = Some(dialog::Dialog::Sure(s));
                        }
                        None => app.status = "cleanup: nothing to remove".into(),
                    }
                }
                Job::Latest(key, tag) => {
                    app.model.latest.insert(key, tag);
                }
                Job::HostRrd(p) => {
                    app.rrd_busy = false;
                    app.model.host_rrd = p;
                }
                Job::GuestRrd(id, p) => {
                    app.rrd_busy = false;
                    app.model.guest_rrd.insert(id, p);
                }
                Job::GuestProcs(id, p) => {
                    app.procs_busy = false;
                    app.model.guest_procs.insert(id, p);
                }
                Job::Power(id, p, r) => {
                    app.busy.retain(|b| b.0 != id);
                    // Read its task log again soon: the change just made is in it.
                    if let Some(e) = app.model.hist.get_mut(&id) {
                        e.1 = e.1.checked_sub(HIST_EVERY).unwrap_or(e.1);
                    }
                    if r.is_ok()
                        && matches!(p, Power::Upgrade)
                        && let Some(g) = app.model.cluster.guests.iter().find(|g| g.id == id)
                    {
                        let kind = g.kind;
                        start_inspect(&mut app.model, id, kind, jobs_tx);
                    }
                    app.status = match &r {
                        Ok(()) => format!("{id}: {} done", p.verb()),
                        Err(e) => format!("{id}: {} failed: {e}", p.verb()),
                    };
                    if let Some((total, done)) = &mut app.batch {
                        let name = app.model.cluster.guests.iter().find(|g| g.id == id).map_or(String::new(), |g| g.name.clone());
                        done.push((id, name, r));
                        if done.len() >= *total {
                            app.results = app.batch.take().map(|b| b.1);
                        }
                    }
                }
            }
        }
        watch_alerts(app);
        prefetch(app, jobs_tx);
        prefetch_hist(app, jobs_tx);
        refresh_tasks(app, jobs_tx);
        refresh_rrd(app, jobs_tx);
        refresh_procs(app, jobs_tx);
        let rows = match app.view {
            View::Guests => app.model.guests(app.sort).len(),
            View::Graph(id) if app.log => app.model.guest_logs.get(&id).and_then(|l| l.as_ref()).map_or(0, Vec::len),
            View::Graph(id) => app.model.guest_procs.get(&id).and_then(|p| p.as_ref()).map_or(0, Vec::len),
            View::Procs => app.model.procs.len(),
            View::Storage => app.model.cluster.storage.len(),
            View::Tasks => app.model.host_tasks.len(),
            View::Host => 0,
        };
        app.selected = app.selected.min(rows.saturating_sub(1));
        term.draw(|f| ui::draw(f, app))?;

        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        // A sequence crossterm cannot parse (a wheel turn, in some terminals) is dropped, not fatal.
        let Ok(Event::Key(k)) = event::read() else { continue };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match k.code {
            _ if app.task_log.is_some() => {
                let n = app.task_log.as_ref().and_then(|t| t.lines.as_ref()).map_or(0, Vec::len);
                if let Some(t) = app.task_log.as_mut() {
                    match k.code {
                        KeyCode::Char('j') | KeyCode::Down => t.back = t.back.saturating_sub(1),
                        KeyCode::Char('k') | KeyCode::Up => t.back = (t.back + 1).min(n),
                        KeyCode::Char('g') | KeyCode::Home => t.back = n,
                        KeyCode::Char('G') | KeyCode::End => t.back = 0,
                        KeyCode::PageUp => t.back = (t.back + 10).min(n),
                        KeyCode::PageDown => t.back = t.back.saturating_sub(10),
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => app.task_log = None,
                        _ => {}
                    }
                }
            }
            _ if app.dialog.is_some() => dialog_key(app, k.code, jobs_tx),
            _ if app.results.is_some() => app.results = None,
            _ if app.help => app.help = false,
            _ if app.filtering => {
                match k.code {
                    KeyCode::Char(c) => app.model.filter.push(c),
                    KeyCode::Backspace => {
                        app.model.filter.pop();
                    }
                    KeyCode::Esc => {
                        app.model.filter.clear();
                        app.filtering = false;
                    }
                    KeyCode::Enter => app.filtering = false,
                    _ => {}
                }
                app.selected = 0;
            }
            _ if app.confirm.is_some() => answer(app, k.code, jobs_tx),
            KeyCode::Char('?') => app.help = true,
            KeyCode::Char('/') if app.view == View::Guests => app.filtering = true,
            KeyCode::Char('o') if app.view == View::Guests => ask(app, Power::Start),
            KeyCode::Char('d') if app.view == View::Guests => ask(app, Power::Shutdown),
            KeyCode::Char('r') if app.view == View::Guests => ask(app, Power::Reboot),
            KeyCode::Char('u') if matches!(app.view, View::Guests | View::Graph(_)) => ask(app, Power::Upgrade),
            KeyCode::Char('S') if matches!(app.view, View::Guests | View::Graph(_)) => open_snapshot(app),
            KeyCode::Char('b') if matches!(app.view, View::Guests | View::Graph(_)) => open_dump(app, jobs_tx),
            KeyCode::Char('m') if matches!(app.view, View::Guests | View::Graph(_)) => open_snaps(app, jobs_tx),
            KeyCode::Char('C') if app.view == View::Guests => open_cleanup(app, jobs_tx),
            KeyCode::Char('x') if app.view == View::Tasks => open_stop(app),
            KeyCode::Char(' ') if app.view == View::Guests => {
                if let Some(id) = app.model.guests(app.sort).get(app.selected).map(|g| g.id)
                    && !app.marks.remove(&id)
                {
                    app.marks.insert(id);
                }
                app.selected = (app.selected + 1).min(rows.saturating_sub(1));
            }
            KeyCode::Esc if app.view == View::Guests => app.marks.clear(),
            KeyCode::Char('l') if matches!(app.view, View::Graph(_)) => {
                app.log = !app.log;
                app.log_back = 0;
                app.procs_at = None;
                app.procs_busy = false;
            }
            KeyCode::Char('j') | KeyCode::Down if app.log && matches!(app.view, View::Graph(_)) => {
                app.log_back = app.log_back.saturating_sub(1)
            }
            KeyCode::Char('k') | KeyCode::Up if app.log && matches!(app.view, View::Graph(_)) => {
                app.log_back = (app.log_back + 1).min(rows)
            }
            KeyCode::Char('g') | KeyCode::Home if app.log && matches!(app.view, View::Graph(_)) => app.log_back = rows,
            KeyCode::Char('G') | KeyCode::End if app.log && matches!(app.view, View::Graph(_)) => app.log_back = 0,
            KeyCode::Esc if matches!(app.view, View::Graph(_)) => {
                app.log = false;
                app.log_back = 0;
                app.view = View::Guests;
                app.selected = app.guest_sel;
                app.rrd_at = None;
                term.clear()?;
            }
            // Esc does not quit: a wheel turn arrives as ESC [ B, and a split read looks like a lone Esc.
            KeyCode::Char('q') => return Ok(()),
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            KeyCode::Tab if !matches!(app.view, View::Graph(_)) => {
                app.view = match app.view {
                    View::Host => View::Guests,
                    View::Guests => View::Procs,
                    View::Procs => View::Storage,
                    View::Storage => View::Tasks,
                    _ => View::Host,
                };
                app.selected = 0;
                app.rrd_at = None;
                // Drop whatever the terminal still shows from the other view.
                term.clear()?;
            }
            KeyCode::Char('j') | KeyCode::Down => app.selected = (app.selected + 1).min(rows.saturating_sub(1)),
            KeyCode::Char('k') | KeyCode::Up => app.selected = app.selected.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => app.selected = 0,
            KeyCode::Char('G') | KeyCode::End => app.selected = rows.saturating_sub(1),
            KeyCode::Char('s') if !matches!(app.view, View::Graph(_)) => app.sort = app.sort.next(),
            KeyCode::Enter if app.view == View::Guests => open_graphs(app),
            KeyCode::Enter if app.view == View::Tasks => open_task_log(app, jobs_tx),
            _ => {}
        }
    }
}

/// The guest the snapshot and backup keys act on: the one under the cursor, or the entered one.
fn target(app: &App) -> Option<dialog::Target> {
    let guests = app.model.guests(app.sort);
    let g = match app.view {
        View::Graph(id) => guests.iter().find(|g| g.id == id),
        _ => guests.get(app.selected),
    };
    g.map(|g| dialog::Target::of(g))
}

/// The least room on the node's storages that hold guest disks (a snapshot lives next to its disk).
fn snap_free(m: &Model, t: &dialog::Target) -> Option<(String, u64)> {
    m.cluster
        .storage
        .iter()
        .filter(|s| s.node == t.node && s.active && s.total > 0 && s.content.split(',').any(|c| c == "images" || c == "rootdir"))
        .map(|s| (s.name.clone(), s.total.saturating_sub(s.used)))
        .min_by_key(|x| x.1)
}

/// `S`: ask for a snapshot name.
fn open_snapshot(app: &mut App) {
    let Some(t) = target(app) else { return };
    if t.template {
        app.status = format!("{}: a template can not be snapshotted", t.id);
        return;
    }
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Name { t, text: dialog::default_name(model::unix_now()) });
}

/// `b`: the backup form; the size of the last backup is read in the background.
fn open_dump(app: &mut App, tx: &Sender<Job>) {
    let Some(t) = target(app) else { return };
    let f = dialog::DumpForm::new(t.clone(), &app.model.cluster.storage);
    if f.stores.is_empty() {
        app.status = format!("{}: no active storage on {} holds backups", t.id, t.node);
        return;
    }
    app.status.clear();
    let names: Vec<String> = f.stores.iter().map(|s| s.0.clone()).collect();
    app.dialog = Some(dialog::Dialog::Dump(f));
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-lastdump".into())
        .spawn(move || {
            let _ = tx.send(Job::BackupSize(t.id, proxmox::ops::last_backup_size(&t.node, t.id, &names)));
        })
        .expect("spawn the backup size thread");
}

/// `m`: the snapshots of the guest, read in the background.
fn open_snaps(app: &mut App, tx: &Sender<Job>) {
    let Some(t) = target(app) else { return };
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Snaps { t: t.clone(), list: None, sel: 0 });
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-snaps".into())
        .spawn(move || {
            let _ = tx.send(Job::Snaps(t.id, proxmox::ops::snapshots(&t.node, t.kind, t.id)));
        })
        .expect("spawn the snapshots thread");
}

/// `x` in the task screen: ask before stopping the running task under the cursor.
fn open_stop(app: &mut App) {
    let Some(t) = app.model.host_tasks.get(app.selected) else { return };
    if t.end.is_some() {
        app.status = "that task has finished".into();
        return;
    }
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Sure(dialog::sure_stop(t)));
}

/// `C`: look for old snapshots and backups beyond the keep count in the background, then ask.
fn open_cleanup(app: &mut App, tx: &Sender<Job>) {
    if app.cleaning {
        return;
    }
    app.cleaning = true;
    app.status = "cleanup: looking for old snapshots and backups...".into();
    let guests: Vec<(u32, String, String, proxmox::Kind)> =
        app.model.cluster.guests.iter().filter(|g| !g.template).map(|g| (g.id, g.name.clone(), g.node.clone(), g.kind)).collect();
    let mut stores: Vec<(String, String)> = vec![];
    for s in &app.model.cluster.storage {
        if s.active && s.content.split(',').any(|c| c == "backup") {
            stores.push((s.node.clone(), s.name.clone()));
        }
    }
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-cleanup".into())
        .spawn(move || {
            let c = config::get();
            let now = model::unix_now();
            let mut items = vec![];
            for (id, name, node, kind) in guests {
                let snaps = proxmox::ops::snapshots(&node, kind, id);
                items.extend(proxmox::ops::plan_snapshots(id, &name, kind, &snaps, now, c.snapshot_days * 86400));
            }
            for (node, storage) in stores {
                items.extend(proxmox::ops::plan_backups(&proxmox::ops::backups(&node, &storage), c.keep_backups));
            }
            let _ = tx.send(Job::Cleanup(items));
        })
        .expect("spawn the cleanup thread");
}

/// A key in a snapshot or backup dialog; `y` at the end runs the operation on a thread of its own.
fn dialog_key(app: &mut App, code: KeyCode, tx: &Sender<Job>) {
    let Some(d) = app.dialog.take() else { return };
    let (next, run) = dialog::key(d, code, |t| snap_free(&app.model, t));
    app.dialog = next;
    let dialog::Next::Run(t, op) = run else { return };
    let label = op.label(&t);
    app.status = format!("{label}: running (a backup shows in the Tasks screen)");
    app.model.tasks_at = None;
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-op".into())
        .spawn(move || {
            let r = match &op {
                dialog::Op::Snap(s) => proxmox::ops::snapshot(t.kind, t.id, s),
                dialog::Op::Dump(d) => proxmox::ops::vzdump(t.id, d),
                dialog::Op::Cmds { cmds, .. } => proxmox::ops::run_all(cmds),
            };
            let _ = tx.send(Job::Op(t.id, label, r));
        })
        .expect("spawn the operation thread");
}

/// Ask for the latest tasks of every node once a minute (and at once on start).
fn refresh_tasks(app: &mut App, tx: &Sender<Job>) {
    let m = &mut app.model;
    if m.tasks_busy || m.tasks_at.is_some_and(|at| at.elapsed() < Duration::from_secs(60)) || m.cluster.nodes.is_empty() {
        return;
    }
    m.tasks_busy = true;
    m.tasks_at = Some(Instant::now());
    let nodes: Vec<String> = m.cluster.nodes.iter().map(|n| n.name.clone()).collect();
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-tasks".into())
        .spawn(move || {
            let _ = tx.send(Job::HostTasks(proxmox::host_tasks(&nodes)));
        })
        .expect("spawn the tasks thread");
}

/// Open the log of the task under the cursor.
fn open_task_log(app: &mut App, tx: &Sender<Job>) {
    let Some(t) = app.model.host_tasks.get(app.selected).cloned() else { return };
    app.task_log =
        Some(TaskLog { upid: t.upid.clone(), title: format!("{} {} on {}", t.kind, t.id, t.node), lines: None, loading: true, back: 0 });
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-tasklog".into())
        .spawn(move || {
            let _ = tx.send(Job::TaskLog(t.upid.clone(), proxmox::task_log(&t.node, &t.upid)));
        })
        .expect("spawn the task log thread");
}

/// Ask for a power change of the selected guest, or of every marked one in the guest list; nothing runs until `y`.
fn ask(app: &mut App, power: Power) {
    if app.view == View::Guests && !app.marks.is_empty() {
        let marked: Vec<_> = app.model.cluster.guests.iter().filter(|g| app.marks.contains(&g.id)).collect();
        let fit: Vec<(u32, String, proxmox::Kind)> =
            marked.iter().filter(|g| !g.template && power.fits(g.status)).map(|g| (g.id, g.name.clone(), g.kind)).collect();
        let skipped = marked.len() - fit.len();
        let n = fit.len();
        let mut fit = fit.into_iter();
        let Some((id, name, kind)) = fit.next() else {
            app.status = format!("none of the {} marked guests can {} in this state", marked.len(), power.verb());
            return;
        };
        let mut note = impact_many(power, n);
        if skipped > 0 {
            note = format!("{note}; {skipped} skipped (wrong state)");
        }
        app.status.clear();
        app.confirm = Some(Ask { id, name, kind, power, note, more: fit.collect() });
        return;
    }
    let guests = app.model.guests(app.sort);
    let at = match app.view {
        View::Graph(id) => guests.iter().position(|g| g.id == id),
        _ => Some(app.selected),
    };
    let Some(g) = at.and_then(|i| guests.get(i)).map(|g| (g.id, g.name.clone(), g.kind, g.status, g.template)) else {
        return;
    };
    let (id, name, kind, status, template) = g;
    if template || !power.fits(status) {
        app.status = format!("{id}: can not {} a guest in this state", power.verb());
        return;
    }
    app.status.clear();
    let note = impact(&app.model, id, power);
    app.confirm = Some(Ask { id, name, kind, power, note, more: vec![] });
}

/// What a power change of several guests does, for the question.
fn impact_many(power: Power, n: usize) -> String {
    match power {
        Power::Start => format!("starts {n} guests"),
        Power::Shutdown => format!("stops {n} guests; connected users are cut off"),
        Power::Reboot => format!("restarts {n} guests; connected users are cut off"),
        Power::Upgrade => format!("installs the updates in {n} guests, one after another"),
    }
}

/// What a power change does, for the question: who is cut off, or how many
/// updates will go in and whether the disk has room for them.
fn impact(m: &Model, id: u32, power: Power) -> String {
    match power {
        Power::Start => String::new(),
        Power::Shutdown => "stops it; connected users are cut off".into(),
        Power::Reboot => "restarts it; connected users are cut off".into(),
        Power::Upgrade => {
            let n =
                m.inside.get(&id).and_then(|i| i.upgradable).map_or("updates of unknown number".to_string(), |n| format!("{n} updates"));
            let free = m.cluster.guests.iter().find(|g| g.id == id).filter(|g| g.maxdisk > 0).map(|g| {
                let low = g.maxdisk.saturating_sub(g.disk) < LOW_FREE || model::disk_pct(g).is_some_and(|p| p >= model::disk_full());
                format!(", {} free{}", bytes(g.maxdisk.saturating_sub(g.disk)), if low { " - LOW, may fail" } else { "" })
            });
            format!("installs {n}{}", free.unwrap_or_default())
        }
    }
}

/// Less free space than this on the root disk warns before an upgrade.
const LOW_FREE: u64 = 512 * 1024 * 1024;

/// The answer to a pending question: `y` runs it on a thread of its own, any
/// other key drops it.
fn answer(app: &mut App, key: KeyCode, tx: &Sender<Job>) {
    let Some(a) = app.confirm.take() else { return };
    if key != KeyCode::Char('y') {
        app.status = "cancelled".into();
        return;
    }
    app.status.clear();
    let mut targets = vec![(a.id, a.kind)];
    targets.extend(a.more.iter().map(|(id, _, kind)| (*id, *kind)));
    let power = a.power;
    for (id, _) in &targets {
        app.busy.push((*id, power, Instant::now()));
    }
    app.marks.clear();
    app.batch = (targets.len() > 1).then(|| (targets.len(), vec![]));
    let tx = tx.clone();
    // One thread goes through them one after another: an upgrade of ten containers at once would hammer the disks.
    std::thread::Builder::new()
        .name("yagura-power".into())
        .spawn(move || {
            for (id, kind) in targets {
                if tx.send(Job::Power(id, power, proxmox::power(kind, id, power))).is_err() {
                    return;
                }
            }
        })
        .expect("spawn the power thread");
}

/// Open the graphs of the selected guest.
fn open_graphs(app: &mut App) {
    let Some(id) = app.model.guests(app.sort).get(app.selected).map(|g| g.id) else { return };
    app.status.clear();
    app.guest_sel = app.selected;
    app.selected = 0;
    app.model.guest_procs.remove(&id);
    app.model.guest_logs.remove(&id);
    app.log = false;
    app.log_back = 0;
    app.procs_at = None;
    app.view = View::Graph(id);
    app.rrd_at = None;
}

/// Read the history of what the screen graphs, on a thread of its own, now and
/// then while that screen is up.
fn refresh_rrd(app: &mut App, tx: &Sender<Job>) {
    if app.rrd_busy || app.rrd_at.is_some_and(|t| t.elapsed() < RRD_EVERY) {
        return;
    }
    let tx = tx.clone();
    match app.view {
        View::Host | View::Guests => {
            let Some(node) = app.model.host_node().map(str::to_string) else { return };
            std::thread::Builder::new()
                .name("yagura-rrd".into())
                .spawn(move || {
                    let _ = tx.send(Job::HostRrd(proxmox::rrd::node(&node)));
                })
                .expect("spawn the rrd thread");
        }
        View::Graph(id) => {
            let Some((node, kind)) = app.model.cluster.guests.iter().find(|g| g.id == id).map(|g| (g.node.clone(), g.kind)) else {
                return;
            };
            std::thread::Builder::new()
                .name("yagura-rrd".into())
                .spawn(move || {
                    let _ = tx.send(Job::GuestRrd(id, proxmox::rrd::guest(&node, kind, id)));
                })
                .expect("spawn the rrd thread");
        }
        _ => return,
    }
    app.rrd_busy = true;
    app.rrd_at = Some(Instant::now());
}

/// Look into every running guest nobody has looked into yet, a few at a
/// time, so the table and the pane are already filled when the cursor gets there.
fn prefetch(app: &mut App, tx: &Sender<Job>) {
    let m = &app.model;
    let todo: Vec<(u32, proxmox::Kind)> = m
        .cluster
        .guests
        .iter()
        .filter(|g| g.status == Status::Running && !g.template)
        .filter(|g| !m.inside.contains_key(&g.id) && !m.looking.contains(&g.id))
        .map(|g| (g.id, g.kind))
        .take(PARALLEL.saturating_sub(m.looking.len()))
        .collect();
    for (id, kind) in todo {
        start_inspect(&mut app.model, id, kind, tx);
    }
}

/// Announce alerts that were not there a moment ago: a status line and, unless switched off, the terminal bell.
fn watch_alerts(app: &mut App) {
    use std::io::Write;
    let now = model::unix_now();
    let cur = app.model.alerts(now);
    let new: Vec<&str> = cur.iter().filter(|(k, _)| !app.alerts_seen.contains(k)).map(|(_, t)| t.as_str()).collect();
    if !new.is_empty() {
        app.status = format!("ALERT: {}", new.join("; "));
        if config::get().bell {
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x07");
            let _ = out.flush();
        }
    }
    app.alerts_seen = cur.iter().map(|(k, _)| k.clone()).collect();
    app.alerts = cur;
}

/// Read every guest's task log and snapshots, a few at a time, and again every few minutes (so a job that wakes a
/// guest weekly and stops it shows its last run, and a failed task shows soon).
fn prefetch_hist(app: &mut App, tx: &Sender<Job>) {
    let m = &mut app.model;
    let storages =
        |node: &str| -> Vec<String> { m.cluster.storage.iter().filter(|s| s.active && s.node == node).map(|s| s.name.clone()).collect() };
    let todo: Vec<(u32, String, proxmox::Kind, Vec<String>)> = m
        .cluster
        .guests
        .iter()
        .filter(|g| !g.template && !m.hist_looking.contains(&g.id))
        .filter(|g| m.hist.get(&g.id).is_none_or(|(_, at)| at.elapsed() >= HIST_EVERY))
        .map(|g| (g.id, g.node.clone(), g.kind, storages(&g.node)))
        .take(PARALLEL.saturating_sub(m.hist_looking.len()))
        .collect();
    for (id, node, kind, stores) in todo {
        m.hist_looking.push(id);
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("yagura-hist".into())
            .spawn(move || {
                let _ = tx.send(Job::History(id, proxmox::history(&node, kind, id, &stores)));
            })
            .expect("spawn the history thread");
    }
}

/// Ask GitHub (or the cache) for the latest release of an app once per run.
fn start_latest(m: &mut Model, name: &str, tx: &Sender<Job>) {
    let key = proxmox::apps::key(name);
    if proxmox::apps::repo(name).is_none() || m.latest.contains_key(&key) {
        return;
    }
    m.latest.insert(key.clone(), None);
    let (tx, name) = (tx.clone(), name.to_string());
    std::thread::Builder::new()
        .name("yagura-latest".into())
        .spawn(move || {
            let _ = tx.send(Job::Latest(key, proxmox::apps::latest(&name)));
        })
        .expect("spawn the latest thread");
}

fn start_inspect(m: &mut Model, id: u32, kind: proxmox::Kind, tx: &Sender<Job>) {
    if m.looking.contains(&id) {
        return;
    }
    m.looking.push(id);
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-inspect".into())
        .spawn(move || {
            let _ = tx.send(Job::Inside(id, proxmox::inspect(kind, id)));
        })
        .expect("spawn the inspect thread");
}

fn apply(m: &mut Model, msg: Msg) {
    match msg {
        Msg::Static(s) => m.host = s.host,
        Msg::Cpu { total, load, .. } => {
            m.cpu = total;
            m.load = load;
        }
        Msg::Mem(x) => m.mem = x,
        Msg::Gpu(devs) => m.push_gpu(devs.first().and_then(|d| d.usage)),
        Msg::Procs { procs, .. } => m.procs = procs,
        _ => {}
    }
}

/// How often the processes inside the entered guest are read again.
const PROCS_EVERY: Duration = Duration::from_secs(5);

/// Read the processes inside the entered guest, on a thread of its own, now and then.
fn refresh_procs(app: &mut App, tx: &Sender<Job>) {
    let View::Graph(id) = app.view else { return };
    if app.procs_busy || app.procs_at.is_some_and(|t| t.elapsed() < PROCS_EVERY) {
        return;
    }
    let Some(kind) = app.model.cluster.guests.iter().find(|g| g.id == id).map(|g| g.kind) else { return };
    let tx = tx.clone();
    let log = app.log;
    std::thread::Builder::new()
        .name("yagura-ps".into())
        .spawn(move || {
            let _ =
                tx.send(if log { Job::GuestLogs(id, proxmox::logs(kind, id)) } else { Job::GuestProcs(id, proxmox::processes(kind, id)) });
        })
        .expect("spawn the ps thread");
    app.procs_busy = true;
    app.procs_at = Some(Instant::now());
}
