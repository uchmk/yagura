//! yagura-tui: the host and its Proxmox VE guests on one screen, in the
//! terminal. The collection is `yagura-core`; this only draws. Everything
//! slow (`pvesh`, `pct exec`) runs on threads of its own and reports over a
//! channel, so the keys never wait.

mod config;
mod dialog;
mod find;
mod model;
mod ui;
mod webhook;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use yagura_core::Msg;
use yagura_core::proxmox::rrd::{Point, Timeframe};
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
    /// How far back the graphs reach (`H` steps it).
    pub rrd_span: Timeframe,
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
    /// The `/` search in the entered guest's log.
    pub log_find: find::Find,
    /// A console to open next: the screen is handed to it until it ends.
    pub console: Option<(String, proxmox::ops::Cmd)>,
}

/// One task's log: `lines` is `None` while it is read (or when it could not be).
pub struct TaskLog {
    pub upid: String,
    pub title: String,
    pub lines: Option<Vec<String>>,
    pub loading: bool,
    /// How many lines up from the newest the view is.
    pub back: usize,
    pub find: find::Find,
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
    Ha(Vec<proxmox::ops::HaRes>),
    Services(String, Vec<proxmox::ops::Service>),
    /// The lines for the open `Dialog::Text` with this title.
    Text(&'static str, Vec<String>),
    Jobs(Vec<proxmox::ops::BackupJob>),
    /// How many packages wait for an upgrade (`None` when it could not be read).
    Upgrade(Option<usize>),
    /// The node's time zone and resolver to start the edit form from (`None` when they could not be read).
    NodeValues(Option<[String; 5]>),
    Content(String, String, Vec<proxmox::ops::Vol>),
    Firewall(u32, Option<(bool, Vec<proxmox::ops::FwRule>)>),
    Cluster(Cluster),
    Inside(u32, Inside),
    Power(u32, Power, Result<(), String>),
    Latest(String, Option<String>),
    HostRrd(Timeframe, Vec<Point>),
    GuestRrd(u32, Timeframe, Vec<Point>),
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
    /// A guest's configuration (empty when it could not be read).
    Config(u32, Vec<(String, String)>),
    /// The backup files of a storage (node, storage, list).
    Backups(String, String, Vec<proxmox::ops::Backup>),
    /// A free guest id for restoring this backup.
    RestoreId(proxmox::ops::Backup, Option<u32>),
    CreateId(dialog::CreateForm, Option<u32>),
    Templates(Vec<String>),
    CloneId(dialog::Target, String, bool, Option<u32>),
}

fn main() -> std::io::Result<()> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("yagura-tui {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!(
            "yagura-tui {}\n\nkeys: Tab view (host, guests, procs, storage, tasks)  j/k move  / find  s sort  Enter graphs  Esc back  Space mark  u upgrade  o start  d shutdown  r reboot  S snapshot  b backup  m snapshots  C cleanup  c config  e edit mem/cores  z grow disk  v drives (move, detach, CD-ROM)  a new guest  P download template  W add hardware  I cloud-init  Y clone  D delete  T template  N network  F firewall  M migrate  O add to HA  Q remove from HA  B boot options  e (storage) edit  H graph span  A (host) users and permissions  E (host) HA  R (host) replication  Z (host) ZFS and Ceph health  P (host) services (restart, s start, x stop, l reload)  X (host) time, DNS and hosts  W (host) change the time zone and DNS  L (host) cluster and node firewall  U (host) package updates, V refresh the lists  K (host) certificates and expiry  D (host) disks and SMART  N (host) network interfaces  I (host) upgrade packages  f (tasks) show failed/running/24 h/7 d, / find  t console  w save a log  n node menu  x stop task  Enter (storage) backups  n (storage) add an NFS storage  X (storage) remove one  l logs  / n N find in a log  ? help  q quit",
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
        rrd_span: Timeframe::default(),
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
        log_find: find::Find::default(),
        console: None,
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
                Job::Services(n, l) => {
                    if let Some(dialog::Dialog::Services { node, list, .. }) = app.dialog.as_mut() {
                        *node = n;
                        *list = Some(l);
                    }
                }
                Job::Ha(l) => {
                    if let Some(dialog::Dialog::Ha { list, .. }) = app.dialog.as_mut() {
                        *list = Some(l);
                    }
                }
                Job::Text(title, l) => {
                    if let Some(dialog::Dialog::Text { lines, .. }) =
                        app.dialog.as_mut().filter(|d| matches!(d, dialog::Dialog::Text { title: t, .. } if t == title))
                    {
                        *lines = Some(l);
                    }
                }
                Job::NodeValues(v) => match v {
                    Some(vals) if app.status == NODE_READING => {
                        app.status.clear();
                        app.dialog = Some(dialog::Dialog::NodeEdit { node: proxmox::ops::this_node(), step: 0, vals, err: String::new() });
                    }
                    Some(_) => {}
                    None => app.status = "could not read the node's time and DNS".into(),
                },
                Job::Upgrade(n) => match n {
                    Some(n) if app.status == UPGRADE_READING => {
                        app.status.clear();
                        app.dialog = Some(dialog::Dialog::Sure(dialog::sure_apt_upgrade(&proxmox::ops::this_node(), n)));
                    }
                    Some(_) => {}
                    None => app.status = "could not read the waiting updates".into(),
                },
                Job::Jobs(l) => {
                    if let Some(dialog::Dialog::Jobs { list, .. }) = app.dialog.as_mut() {
                        *list = Some(l);
                    }
                }
                Job::Content(n, s, l) => {
                    if let Some(dialog::Dialog::Content { list, .. }) = app
                        .dialog
                        .as_mut()
                        .filter(|d| matches!(d, dialog::Dialog::Content { node, storage, .. } if *node == n && *storage == s))
                    {
                        *list = Some(l);
                    }
                }
                Job::Firewall(id, r) => {
                    if let Some(dialog::Dialog::Firewall { state, .. }) =
                        app.dialog.as_mut().filter(|d| matches!(d, dialog::Dialog::Firewall { t, .. } if t.id == id))
                    {
                        *state = Some(r.unwrap_or((false, vec![])));
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
                Job::Config(id, l) => {
                    if let Some(dialog::Dialog::Resize { t, disks, keys, .. }) =
                        app.dialog.as_mut().filter(|d| matches!(d, dialog::Dialog::Resize { t, .. } if t.id == id))
                    {
                        *disks = Some(proxmox::ops::disks(t.kind, &l));
                        *keys = l.iter().map(|(k, _)| k.clone()).collect();
                    }
                    if let Some(dialog::Dialog::Net { t, nets, bridge, vlan, mac, .. }) =
                        app.dialog.as_mut().filter(|d| matches!(d, dialog::Dialog::Net { t, .. } if t.id == id))
                    {
                        let l = proxmox::ops::nets(&l);
                        if let Some((_, v)) = l.first() {
                            (*bridge, *vlan, *mac) = proxmox::ops::net_fields(t.kind, v);
                        }
                        *nets = Some(l);
                    }
                    if let Some(dialog::Dialog::Boot { vals, loaded, .. }) =
                        app.dialog.as_mut().filter(|d| matches!(d, dialog::Dialog::Boot { t, .. } if t.id == id))
                    {
                        *vals = proxmox::ops::boot_fields(&l);
                        *loaded = true;
                    }
                    if let Some(dialog::Dialog::CloudInit { t, v, loaded, .. }) =
                        app.dialog.as_mut().filter(|d| matches!(d, dialog::Dialog::CloudInit { t, .. } if t.id == id))
                    {
                        let _ = t;
                        *v = proxmox::ops::cloudinit_fields(&l);
                        *loaded = true;
                    }
                    if let Some(dialog::Dialog::Hardware { t, keys, .. }) =
                        app.dialog.as_mut().filter(|d| matches!(d, dialog::Dialog::Hardware { t, .. } if t.id == id))
                    {
                        let _ = t;
                        *keys = Some(l.iter().map(|(k, _)| k.clone()).collect());
                    }
                    if let Some(dialog::Dialog::Disks { t, list, .. }) =
                        app.dialog.as_mut().filter(|d| matches!(d, dialog::Dialog::Disks { t, .. } if t.id == id))
                    {
                        *list = Some(proxmox::ops::drives(t.kind, &l));
                    }
                    if let Some(dialog::Dialog::Config { list, .. }) =
                        app.dialog.as_mut().filter(|d| matches!(d, dialog::Dialog::Config { t, .. } if t.id == id))
                    {
                        *list = Some(l);
                    }
                }
                Job::Backups(n, s, l) => {
                    if let Some(dialog::Dialog::Backups { list, .. }) = app
                        .dialog
                        .as_mut()
                        .filter(|d| matches!(d, dialog::Dialog::Backups { node, storage, .. } if *node == n && *storage == s))
                    {
                        *list = Some(l);
                    }
                }
                Job::RestoreId(b, id) => match id.and_then(|id| dialog::sure_restore(&b, id)) {
                    Some(s) => {
                        app.status.clear();
                        app.dialog = Some(dialog::Dialog::Sure(s));
                    }
                    None => app.status = "restore: no free guest id, or this file is not a guest backup".into(),
                },
                Job::Templates(l) => {
                    if let Some(dialog::Dialog::Templates { list, .. }) = app.dialog.as_mut() {
                        *list = Some(l);
                    }
                }
                Job::CreateId(f, id) => match id.map(|id| dialog::sure_create(&f, id)) {
                    Some(Ok(s)) => {
                        app.status.clear();
                        app.dialog = Some(dialog::Dialog::Sure(s));
                    }
                    Some(Err(e)) => app.status = format!("create: {e}"),
                    None => app.status = "create: no free guest id".into(),
                },
                Job::CloneId(t, name, full, id) => match id {
                    Some(id) => {
                        app.status.clear();
                        app.dialog = Some(dialog::Dialog::Sure(dialog::sure_clone(&t, &name, full, id)));
                    }
                    None => app.status = "clone: no free guest id".into(),
                },
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
                Job::HostRrd(span, p) => {
                    app.rrd_busy = false;
                    if span == app.rrd_span {
                        app.model.host_rrd = p;
                    } else {
                        // Asked before `H` changed the span: ask again.
                        app.rrd_at = None;
                    }
                }
                Job::GuestRrd(id, span, p) => {
                    app.rrd_busy = false;
                    if span == app.rrd_span {
                        app.model.guest_rrd.insert(id, p);
                    } else {
                        app.rrd_at = None;
                    }
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
            View::Tasks => app.model.tasks(model::unix_now()).len(),
            View::Host => 0,
        };
        app.selected = app.selected.min(rows.saturating_sub(1));
        if let Some((hint, cmd)) = app.console.take() {
            console(term, app, &hint, &cmd);
        }
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
                    let empty = Vec::new();
                    if find_key(&mut t.find, t.lines.as_ref().unwrap_or(&empty), &mut t.back, k.code) {
                        continue;
                    }
                    match k.code {
                        KeyCode::Char('j') | KeyCode::Down => t.back = t.back.saturating_sub(1),
                        KeyCode::Char('k') | KeyCode::Up => t.back = (t.back + 1).min(n),
                        KeyCode::Char('g') | KeyCode::Home => t.back = n,
                        KeyCode::Char('G') | KeyCode::End => t.back = 0,
                        KeyCode::PageUp => t.back = (t.back + 10).min(n),
                        KeyCode::PageDown => t.back = t.back.saturating_sub(10),
                        KeyCode::Char('w') if t.lines.is_some() => {
                            let base = t.title.split(" [saved").next().unwrap_or_default().to_string();
                            let r = save_log(&t.title, t.lines.as_deref().unwrap_or_default());
                            t.title = format!("{base} [{r}]");
                        }
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => app.task_log = None,
                        _ => {}
                    }
                }
            }
            _ if app.dialog.is_some() => dialog_key(app, k.code, jobs_tx),
            _ if app.results.is_some() => app.results = None,
            _ if app.help => app.help = false,
            _ if app.filtering => {
                let text = if app.view == View::Tasks { &mut app.model.task_find } else { &mut app.model.filter };
                match k.code {
                    KeyCode::Char(c) => text.push(c),
                    KeyCode::Backspace => {
                        text.pop();
                    }
                    KeyCode::Esc => {
                        text.clear();
                        app.filtering = false;
                    }
                    KeyCode::Enter => app.filtering = false,
                    _ => {}
                }
                app.selected = 0;
            }
            _ if app.log && matches!(app.view, View::Graph(_)) && guest_log_find(app, k.code) => {}
            _ if app.confirm.is_some() => answer(app, k.code, jobs_tx),
            KeyCode::Char('?') => app.help = true,
            KeyCode::Char('/') if matches!(app.view, View::Guests | View::Tasks) => app.filtering = true,
            KeyCode::Char('f') if app.view == View::Tasks => {
                app.model.task_filter = app.model.task_filter.next();
                app.selected = 0;
            }
            KeyCode::Char('o') if app.view == View::Guests => ask(app, Power::Start),
            KeyCode::Char('d') if app.view == View::Guests => ask(app, Power::Shutdown),
            KeyCode::Char('r') if app.view == View::Guests => ask(app, Power::Reboot),
            KeyCode::Char('u') if matches!(app.view, View::Guests | View::Graph(_)) => ask(app, Power::Upgrade),
            KeyCode::Char('S') if matches!(app.view, View::Guests | View::Graph(_)) => open_snapshot(app),
            KeyCode::Char('b') if matches!(app.view, View::Guests | View::Graph(_)) => open_dump(app, jobs_tx),
            KeyCode::Char('m') if matches!(app.view, View::Guests | View::Graph(_)) => open_snaps(app, jobs_tx),
            KeyCode::Char('C') if app.view == View::Guests => open_cleanup(app, jobs_tx),
            KeyCode::Char('c') if !k.modifiers.contains(KeyModifiers::CONTROL) && matches!(app.view, View::Guests | View::Graph(_)) => {
                open_config(app, jobs_tx)
            }
            KeyCode::Char('e') if app.view == View::Guests => open_edit(app),
            KeyCode::Char('z') if matches!(app.view, View::Guests | View::Graph(_)) => open_resize(app, jobs_tx),
            KeyCode::Char('v') if matches!(app.view, View::Guests | View::Graph(_)) => open_drives(app, jobs_tx),
            KeyCode::Char('a') if app.view == View::Guests => open_create(app),
            KeyCode::Char('P') if app.view == View::Guests => open_templates(app, jobs_tx),
            KeyCode::Char('W') if matches!(app.view, View::Guests | View::Graph(_)) => open_hardware(app, jobs_tx),
            KeyCode::Char('I') if matches!(app.view, View::Guests | View::Graph(_)) => open_cloudinit(app, jobs_tx),
            KeyCode::Char('n') if app.view == View::Guests => open_node(app),
            KeyCode::Char('Y') if matches!(app.view, View::Guests | View::Graph(_)) => open_clone(app),
            KeyCode::Char('M') if matches!(app.view, View::Guests | View::Graph(_)) => open_migrate(app),
            KeyCode::Char('B') if matches!(app.view, View::Guests | View::Graph(_)) => open_boot(app, jobs_tx),
            KeyCode::Char(c @ ('O' | 'Q')) if matches!(app.view, View::Guests | View::Graph(_)) => open_ha_guest(app, c == 'O'),
            KeyCode::Char('D') if matches!(app.view, View::Guests | View::Graph(_)) => open_destroy(app),
            KeyCode::Char('T') if matches!(app.view, View::Guests | View::Graph(_)) => open_template(app),
            KeyCode::Char('H') if matches!(app.view, View::Host | View::Guests | View::Graph(_)) => cycle_span(app),
            KeyCode::Char('N') if matches!(app.view, View::Guests | View::Graph(_)) => open_net(app, jobs_tx),
            KeyCode::Char('F') if matches!(app.view, View::Guests | View::Graph(_)) => open_firewall(app, jobs_tx),
            KeyCode::Char('t') if matches!(app.view, View::Guests | View::Graph(_)) => open_console(app),
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
                app.log_find.clear();
                app.procs_at = None;
                app.procs_busy = false;
            }
            KeyCode::Char('w') if app.log && matches!(app.view, View::Graph(_)) => save_guest_log(app),
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
                app.log_find.clear();
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
            KeyCode::Enter if app.view == View::Storage => open_backups(app, jobs_tx),
            KeyCode::Char('i') if app.view == View::Storage => open_content(app, jobs_tx),
            KeyCode::Char('J') if app.view == View::Storage => open_jobs(app, jobs_tx),
            KeyCode::Char('n') if app.view == View::Storage => {
                app.status.clear();
                let vals = ["", "", "/", "backup,iso,vztmpl"].map(String::from);
                app.dialog = Some(dialog::Dialog::StorageAdd { kind: 0, step: 0, vals, err: String::new() });
            }
            KeyCode::Char('e') if app.view == View::Storage => {
                if let Some(s) = app.model.storages_sorted().get(app.selected).map(|s| (*s).clone()) {
                    app.status.clear();
                    let vals = [s.content.clone(), String::new()];
                    app.dialog = Some(dialog::Dialog::StorageEdit { name: s.name, kind: s.kind, step: 0, vals, err: String::new() });
                }
            }
            KeyCode::Char('X') if app.view == View::Storage => {
                if let Some(s) = app.model.storages_sorted().get(app.selected).map(|s| (*s).clone()) {
                    app.status.clear();
                    app.dialog = Some(dialog::Dialog::Sure(dialog::sure_storage_remove(&s)));
                }
            }
            KeyCode::Char('A') if app.view == View::Host => open_access(app, jobs_tx),
            KeyCode::Char('E') if app.view == View::Host => open_ha(app, jobs_tx),
            KeyCode::Char('R') if app.view == View::Host => open_replication(app, jobs_tx),
            KeyCode::Char('Z') if app.view == View::Host => open_pools(app, jobs_tx),
            KeyCode::Char('P') if app.view == View::Host => open_services(app, jobs_tx),
            KeyCode::Char('X') if app.view == View::Host => open_node_settings(app, jobs_tx),
            KeyCode::Char('U') if app.view == View::Host => open_apt(app, jobs_tx),
            KeyCode::Char('K') if app.view == View::Host => open_certs(app, jobs_tx),
            KeyCode::Char('S') if app.view == View::Host => open_ssh(app, jobs_tx),
            KeyCode::Char('D') if app.view == View::Host => open_disks(app, jobs_tx),
            KeyCode::Char('N') if app.view == View::Host => open_network(app, jobs_tx),
            KeyCode::Char('W') if app.view == View::Host => open_node_edit(app, jobs_tx),
            KeyCode::Char('L') if app.view == View::Host => open_host_firewall(app, jobs_tx),
            KeyCode::Char('I') if app.view == View::Host => open_upgrade(app, jobs_tx),
            KeyCode::Char('V') if app.view == View::Host => {
                app.dialog = Some(dialog::Dialog::Sure(dialog::sure_apt_refresh(&proxmox::ops::this_node())));
            }
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

/// `Y`: ask for the name of a copy of the guest.
fn open_clone(app: &mut App) {
    let Some(t) = target(app) else { return };
    app.status.clear();
    let name = format!("{}-clone", t.name);
    let name = if proxmox::ops::valid_guest_name(&name) { name } else { format!("clone-{}", t.id) };
    // A template is cloned linked at first: it is quick and shares the template's disks.
    let full = !t.template;
    app.dialog = Some(dialog::Dialog::Clone { t, name, full });
}

/// `P`: the container templates that can be downloaded; the list is read in the background.
fn open_templates(app: &mut App, tx: &Sender<Job>) {
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Templates { list: None, sel: 0, filter: String::new(), storage: "local".into(), on_storage: false });
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-templates".into())
        .spawn(move || {
            let _ = tx.send(Job::Templates(proxmox::ops::available_templates()));
        })
        .expect("spawn the templates thread");
}

/// `a`: the form of a new guest.
fn open_create(app: &mut App) {
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Create(dialog::CreateForm::new()));
}

/// `O` / `Q`: put the guest under the cursor under HA, or take it out of HA.
fn open_ha_guest(app: &mut App, add: bool) {
    let Some(t) = target(app) else { return };
    if t.template {
        app.status = format!("{}: a template can not be under HA", t.id);
        return;
    }
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Sure(dialog::sure_ha_guest(&t, add)));
}

/// `B`: the boot options of the guest under the cursor (start at boot, order, delays); its config is read in the background.
fn open_boot(app: &mut App, tx: &Sender<Job>) {
    let Some(t) = target(app) else { return };
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Boot { t: t.clone(), step: 0, vals: Default::default(), loaded: false, err: String::new() });
    read_config(t, tx);
}

/// `M`: move the guest under the cursor to another online node of the cluster.
fn open_migrate(app: &mut App) {
    let Some(t) = target(app) else { return };
    if t.template {
        app.status = format!("{}: a template is cloned (Y), not moved", t.id);
        return;
    }
    let nodes: Vec<String> = app.model.cluster.nodes.iter().filter(|n| n.online && n.name != t.node).map(|n| n.name.clone()).collect();
    if nodes.is_empty() {
        app.status = format!("{}: there is no other online node to move it to", t.id);
        return;
    }
    let running = app.model.cluster.guests.iter().any(|g| g.id == t.id && g.status != proxmox::Status::Stopped);
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Migrate { t, nodes, sel: 0, running, online: true });
}

/// `D`: delete the guest under the cursor, if it is stopped.
fn open_destroy(app: &mut App) {
    let Some(t) = target(app) else { return };
    let running = app.model.cluster.guests.iter().any(|g| g.id == t.id && g.status != proxmox::Status::Stopped);
    if running {
        app.status = format!("{}: stop the guest before deleting it", t.id);
        return;
    }
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Sure(dialog::sure_destroy(&t)));
}

/// `T`: turn the stopped guest under the cursor into a template.
fn open_template(app: &mut App) {
    let Some(t) = target(app) else { return };
    if t.template {
        app.status = format!("{}: already a template (Y makes guests from it)", t.id);
        return;
    }
    if app.model.cluster.guests.iter().any(|g| g.id == t.id && g.status != proxmox::Status::Stopped) {
        app.status = format!("{}: stop the guest before turning it into a template", t.id);
        return;
    }
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Sure(dialog::sure_template(&t)));
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

/// `c`: the configuration of the guest, read in the background.
fn open_config(app: &mut App, tx: &Sender<Job>) {
    let Some(t) = target(app) else { return };
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Config { t: t.clone(), list: None, top: 0 });
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-config".into())
        .spawn(move || {
            let _ = tx.send(Job::Config(t.id, proxmox::ops::config(&t.node, t.kind, t.id).unwrap_or_default()));
        })
        .expect("spawn the config thread");
}

/// `z`: grow a disk of the guest; the config is read in the background to list its disks.
fn open_resize(app: &mut App, tx: &Sender<Job>) {
    let Some(t) = target(app) else { return };
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Resize { t: t.clone(), disks: None, keys: vec![], sel: 0, gib: String::new() });
    read_config(t, tx);
}

/// `I`: the cloud-init settings of a VM; the config is read in the background.
fn open_cloudinit(app: &mut App, tx: &Sender<Job>) {
    let Some(t) = target(app) else { return };
    if t.kind != proxmox::Kind::Qemu {
        app.status = format!("{}: cloud-init is for VMs", t.id);
        return;
    }
    app.status.clear();
    app.dialog = Some(dialog::Dialog::CloudInit { t: t.clone(), v: Default::default(), field: 0, loaded: false });
    read_config(t, tx);
}

/// `W`: add hardware to a VM (USB, PCI passthrough, serial port, TPM, EFI disk); the config is read in the background.
fn open_hardware(app: &mut App, tx: &Sender<Job>) {
    let Some(t) = target(app) else { return };
    if t.kind != proxmox::Kind::Qemu {
        app.status = format!("{}: hardware can only be added to a VM", t.id);
        return;
    }
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Hardware { t: t.clone(), keys: None, sel: 0, text: String::new() });
    read_config(t, tx);
}

/// `v`: the drives of the guest (move to another storage, detach); the config is read in the background.
fn open_drives(app: &mut App, tx: &Sender<Job>) {
    let Some(t) = target(app) else { return };
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Disks { t: t.clone(), list: None, sel: 0, ask: None, text: String::new() });
    read_config(t, tx);
}

/// `F`: the firewall of the guest (on or off, and its rules); read in the background.
fn open_firewall(app: &mut App, tx: &Sender<Job>) {
    let Some(t) = target(app) else { return };
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Firewall { t: t.clone(), state: None, top: 0 });
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-firewall".into())
        .spawn(move || {
            let _ = tx.send(Job::Firewall(t.id, proxmox::ops::firewall(&t.node, t.kind, t.id)));
        })
        .expect("spawn the firewall thread");
}

/// `N`: the network devices of the guest (bridge, VLAN, MAC); the config is read in the background.
fn open_net(app: &mut App, tx: &Sender<Job>) {
    let Some(t) = target(app) else { return };
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Net {
        t: t.clone(),
        nets: None,
        sel: 0,
        field: 0,
        bridge: String::new(),
        vlan: String::new(),
        mac: String::new(),
    });
    read_config(t, tx);
}

/// Read the configuration of a guest on a thread of its own; the answer is a `Job::Config`.
fn read_config(t: dialog::Target, tx: &Sender<Job>) {
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-config".into())
        .spawn(move || {
            let _ = tx.send(Job::Config(t.id, proxmox::ops::config(&t.node, t.kind, t.id).unwrap_or_default()));
        })
        .expect("spawn the config thread");
}

/// The `/` search keys of a log: typing the text, `n` / `N` to step through the matches, Esc to drop the text.
/// True when the key was the search's.
fn find_key(find: &mut find::Find, lines: &[String], back: &mut usize, code: KeyCode) -> bool {
    use find::{Step, Typed};
    let step = if find.typing {
        match find.key(code) {
            Typed::Done => Step::First,
            _ => return true,
        }
    } else {
        match code {
            KeyCode::Char('/') => {
                find.start();
                return true;
            }
            KeyCode::Char('n') if find.active() => Step::Older,
            KeyCode::Char('N') if find.active() => Step::Newer,
            KeyCode::Esc if find.active() => {
                find.clear();
                return true;
            }
            _ => return false,
        }
    };
    if let Some(b) = find.jump(lines, *back, step) {
        *back = b;
    }
    true
}

/// Write a log to a new file in the home directory (or `/tmp`); the answer is what to tell the user.
fn save_log(label: &str, lines: &[String]) -> String {
    let dir = std::env::var_os("HOME").map(std::path::PathBuf::from).filter(|p| p.is_dir()).unwrap_or_else(|| "/tmp".into());
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    match proxmox::ops::export_lines(&dir, label, secs, lines) {
        Ok(p) => format!("saved {} lines to {}", lines.len(), p.display()),
        Err(e) => format!("save failed: {e}"),
    }
}

/// `w` in the entered guest's journal: write what is shown to a file.
fn save_guest_log(app: &mut App) {
    let View::Graph(id) = app.view else { return };
    let Some(lines) = app.model.guest_logs.get(&id).and_then(|l| l.as_ref()).filter(|l| !l.is_empty()) else {
        app.status = "no log to save".into();
        return;
    };
    app.status = save_log(&format!("guest {id}"), lines);
}

/// The search in the entered guest's journal.
fn guest_log_find(app: &mut App, code: KeyCode) -> bool {
    let View::Graph(id) = app.view else { return false };
    let empty = Vec::new();
    let lines = app.model.guest_logs.get(&id).and_then(|l| l.as_ref()).unwrap_or(&empty);
    find_key(&mut app.log_find, lines, &mut app.log_back, code)
}

/// `t`: the console of the running guest under the cursor; the screen goes to it until it ends.
fn open_console(app: &mut App) {
    let guests = app.model.guests(app.sort);
    let g = match app.view {
        View::Graph(id) => guests.iter().find(|g| g.id == id),
        _ => guests.get(app.selected),
    };
    let Some(g) = g else { return };
    if g.status != proxmox::Status::Running {
        app.status = format!("{}: the console needs a running guest", g.id);
        return;
    }
    let hint = if g.kind == proxmox::Kind::Lxc { "leave with exit" } else { "leave with Ctrl+O" };
    app.status.clear();
    app.console =
        Some((format!("{} {} ({hint})", g.id, g.name), proxmox::ops::console_on_node(&g.node, proxmox::ops::console_cmd(g.kind, g.id))));
}

/// Give the terminal to the console command, then take the screen back.
fn console(term: &mut ratatui::DefaultTerminal, app: &mut App, title: &str, cmd: &proxmox::ops::Cmd) {
    use std::io::Write;
    let _ = term.show_cursor();
    ratatui::restore();
    println!(
        "
console {title}
"
    );
    let why = match std::process::Command::new(&cmd.prog).args(&cmd.args).status() {
        Ok(s) if s.success() => None,
        Ok(s) => Some(s.to_string()),
        Err(e) => Some(e.to_string()),
    };
    if let Some(why) = why {
        println!(
            "
{} {} did not end cleanly ({why}). Press Enter.",
            cmd.prog,
            cmd.args.join(" ")
        );
        let _ = std::io::stdout().flush();
        let _ = std::io::stdin().read_line(&mut String::new());
        app.status = format!("console {title}: ended with an error");
    }
    *term = ratatui::init();
    let _ = term.hide_cursor();
    let _ = term.clear();
}

/// `e`: the memory and cores form for the marked guests, or the one under the cursor.
/// `n` in the guest list: the node menu for the node of the guest under the cursor (else the first node).
fn open_node(app: &mut App) {
    let guests = app.model.guests(app.sort);
    let node = guests.get(app.selected).map(|g| g.node.clone()).or_else(|| app.model.cluster.nodes.first().map(|n| n.name.clone()));
    let Some(node) = node else { return };
    let on_node = |st: proxmox::Status| -> Vec<dialog::Target> {
        app.model.cluster.guests.iter().filter(|g| g.node == node && !g.template && g.status == st).map(dialog::Target::of).collect()
    };
    let (stopped, running) = (on_node(proxmox::Status::Stopped), on_node(proxmox::Status::Running));
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Node { node, stopped, running });
}

fn open_edit(app: &mut App) {
    let guests = app.model.guests(app.sort);
    let targets: Vec<dialog::Target> = if app.marks.is_empty() {
        guests.get(app.selected).into_iter().map(|g| dialog::Target::of(g)).collect()
    } else {
        guests.iter().filter(|g| app.marks.contains(&g.id)).map(|g| dialog::Target::of(g)).collect()
    };
    if targets.is_empty() {
        return;
    }
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Edit(dialog::EditForm::new(targets)));
}

/// Enter in the storage screen: the backup files of the storage under the cursor, read in the background.
fn open_backups(app: &mut App, tx: &Sender<Job>) {
    let Some(s) = app.model.storages_sorted().get(app.selected).map(|s| (*s).clone()) else { return };
    if !s.content.split(',').any(|c| c == "backup") {
        app.status = format!("{} holds no backups", s.name);
        return;
    }
    app.status.clear();
    app.dialog =
        Some(dialog::Dialog::Backups { node: s.node.clone(), storage: s.name.clone(), list: None, sel: 0, marks: Default::default() });
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-backups".into())
        .spawn(move || {
            let l = proxmox::ops::backups(&s.node, &s.name);
            let _ = tx.send(Job::Backups(s.node, s.name, l));
        })
        .expect("spawn the backups thread");
}

/// `i` in the storage screen: everything on the storage by kind and size; `d` deletes one volume (asks first).
fn open_content(app: &mut App, tx: &Sender<Job>) {
    let Some(s) = app.model.storages_sorted().get(app.selected).map(|s| (*s).clone()) else { return };
    if !s.active {
        app.status = format!("{} is not active", s.name);
        return;
    }
    app.status.clear();
    app.dialog =
        Some(dialog::Dialog::Content { node: s.node.clone(), storage: s.name.clone(), fill: (s.used, s.total), list: None, sel: 0 });
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-content".into())
        .spawn(move || {
            let l = proxmox::ops::contents(&s.node, &s.name);
            let _ = tx.send(Job::Content(s.node, s.name, l));
        })
        .expect("spawn the content thread");
}

/// `E` in the host screen: the HA resources of the cluster; Enter turns HA on or off for one (asks first).
fn open_ha(app: &mut App, tx: &Sender<Job>) {
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Ha { list: None, sel: 0 });
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-ha".into())
        .spawn(move || {
            let _ = tx.send(Job::Ha(proxmox::ops::ha()));
        })
        .expect("spawn the HA thread");
}

/// `R` in the host screen: the replication jobs and how their last runs went (read only).
fn open_replication(app: &mut App, tx: &Sender<Job>) {
    const TITLE: &str = "replication";
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Text { title: TITLE.into(), lines: None, top: 0 });
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-replication".into())
        .spawn(move || {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let _ = tx.send(Job::Text(TITLE, proxmox::ops::replication(now)));
        })
        .expect("spawn the replication thread");
}

/// `P` in the host screen: the services of this node; Enter restarts one (asks first).
fn open_services(app: &mut App, tx: &Sender<Job>) {
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Services { node: String::new(), list: None, sel: 0 });
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-services".into())
        .spawn(move || {
            let (node, list) = proxmox::ops::services();
            let _ = tx.send(Job::Services(node, list));
        })
        .expect("spawn the services thread");
}

/// `L` in the host screen: the cluster and node firewall, on or off, policies and rules (read only).
fn open_host_firewall(app: &mut App, tx: &Sender<Job>) {
    const TITLE: &str = "cluster and node firewall";
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Text { title: TITLE.into(), lines: None, top: 0 });
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-hostfw".into())
        .spawn(move || {
            let _ = tx.send(Job::Text(TITLE, proxmox::ops::host_firewall()));
        })
        .expect("spawn the host firewall thread");
}

const NODE_READING: &str = "reading the node's time and DNS...";

/// `W` in the host screen: read the time zone and resolver, then open the form to change them (asks before it writes).
fn open_node_edit(app: &mut App, tx: &Sender<Job>) {
    app.status = NODE_READING.into();
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-node-edit".into())
        .spawn(move || {
            let _ = tx.send(Job::NodeValues(proxmox::ops::node_values()));
        })
        .expect("spawn the node edit thread");
}

const UPGRADE_READING: &str = "counting the waiting updates...";

/// `I` in the host screen: counts the waiting updates in the background, then asks before `apt-get dist-upgrade` on this node.
fn open_upgrade(app: &mut App, tx: &Sender<Job>) {
    app.status = UPGRADE_READING.into();
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-upgrade".into())
        .spawn(move || {
            let _ = tx.send(Job::Upgrade(proxmox::ops::apt_count()));
        })
        .expect("spawn the upgrade thread");
}

/// `N` in the host screen: this node's bridges, bonds, VLANs and ports (read only).
fn open_network(app: &mut App, tx: &Sender<Job>) {
    const TITLE: &str = "network interfaces";
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Text { title: TITLE.into(), lines: None, top: 0 });
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-network".into())
        .spawn(move || {
            let _ = tx.send(Job::Text(TITLE, proxmox::ops::network()));
        })
        .expect("spawn the network thread");
}

/// `D` in the host screen: this node's disks with their SMART health, wearout and values (read only).
fn open_disks(app: &mut App, tx: &Sender<Job>) {
    const TITLE: &str = "disks and SMART";
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Text { title: TITLE.into(), lines: None, top: 0 });
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-disks".into())
        .spawn(move || {
            let _ = tx.send(Job::Text(TITLE, proxmox::ops::node_disks()));
        })
        .expect("spawn the disks thread");
}

/// `K` in the host screen: this node's certificates and when they run out (read only).
fn open_certs(app: &mut App, tx: &Sender<Job>) {
    const TITLE: &str = "certificates";
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Text { title: TITLE.into(), lines: None, top: 0 });
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-certs".into())
        .spawn(move || {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let _ = tx.send(Job::Text(TITLE, proxmox::ops::certificates(now)));
        })
        .expect("spawn the certificates thread");
}

/// `S` in the host screen: whether this node can log in to the other nodes over ssh with a key (read only).
fn open_ssh(app: &mut App, tx: &Sender<Job>) {
    const TITLE: &str = "ssh to the other nodes";
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Text { title: TITLE.into(), lines: None, top: 0 });
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-ssh".into())
        .spawn(move || {
            let _ = tx.send(Job::Text(TITLE, proxmox::ops::ssh_check()));
        })
        .expect("spawn the ssh thread");
}

/// `U` in the host screen: the package updates waiting on this node (read only); `V` refreshes the lists (asks first).
fn open_apt(app: &mut App, tx: &Sender<Job>) {
    const TITLE: &str = "package updates";
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Text { title: TITLE.into(), lines: None, top: 0 });
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-apt".into())
        .spawn(move || {
            let _ = tx.send(Job::Text(TITLE, proxmox::ops::apt_updates()));
        })
        .expect("spawn the apt thread");
}

/// `X` in the host screen: this node's clock, DNS and hosts file (read only).
fn open_node_settings(app: &mut App, tx: &Sender<Job>) {
    const TITLE: &str = "node time, DNS and hosts";
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Text { title: TITLE.into(), lines: None, top: 0 });
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-node".into())
        .spawn(move || {
            let _ = tx.send(Job::Text(TITLE, proxmox::ops::node_settings()));
        })
        .expect("spawn the node settings thread");
}

/// `Z` in the host screen: the health of the ZFS pools and the Ceph cluster (read only).
fn open_pools(app: &mut App, tx: &Sender<Job>) {
    const TITLE: &str = "ZFS and Ceph";
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Text { title: TITLE.into(), lines: None, top: 0 });
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-pools".into())
        .spawn(move || {
            let _ = tx.send(Job::Text(TITLE, proxmox::ops::pools()));
        })
        .expect("spawn the pools thread");
}

/// `A` in the host screen: the users, API tokens and permissions of the cluster (read only; Proxmox keeps these in the web UI).
fn open_access(app: &mut App, tx: &Sender<Job>) {
    const TITLE: &str = "users, tokens and permissions";
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Text { title: TITLE.into(), lines: None, top: 0 });
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-access".into())
        .spawn(move || {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let _ = tx.send(Job::Text(TITLE, proxmox::ops::access(now)));
        })
        .expect("spawn the access thread");
}

/// `J` in the storage screen: the scheduled backup jobs of the cluster; Enter turns one on or off (asks first).
fn open_jobs(app: &mut App, tx: &Sender<Job>) {
    app.status.clear();
    app.dialog = Some(dialog::Dialog::Jobs { list: None, sel: 0 });
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-jobs".into())
        .spawn(move || {
            let _ = tx.send(Job::Jobs(proxmox::ops::backup_jobs()));
        })
        .expect("spawn the backup jobs thread");
}

/// `x` in the task screen: ask before stopping the running task under the cursor.
fn open_stop(app: &mut App) {
    let Some(t) = app.model.tasks(model::unix_now()).get(app.selected).copied() else { return };
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
    if let dialog::Next::Restore(b) = run {
        app.status = "restore: looking for a free guest id...".into();
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("yagura-nextid".into())
            .spawn(move || {
                let _ = tx.send(Job::RestoreId(b, proxmox::ops::next_id()));
            })
            .expect("spawn the next id thread");
        return;
    }
    if let dialog::Next::CreateId(f) = run {
        app.status = "create: looking for a free guest id...".into();
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("yagura-nextid".into())
            .spawn(move || {
                let _ = tx.send(Job::CreateId(f, proxmox::ops::next_id()));
            })
            .expect("spawn the next id thread");
        return;
    }
    if let dialog::Next::CloneId(t, name, full) = run {
        app.status = "clone: looking for a free guest id...".into();
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("yagura-nextid".into())
            .spawn(move || {
                let _ = tx.send(Job::CloneId(t, name, full, proxmox::ops::next_id()));
            })
            .expect("spawn the next id thread");
        return;
    }
    let dialog::Next::Run(t, op) = run else { return };
    let label = op.label(&t);
    app.status = format!("{label}: running (a backup shows in the Tasks screen)");
    app.model.tasks_at = None;
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-op".into())
        .spawn(move || {
            let r = match &op {
                // qm, pct, vzdump and pveam only work on their own node: a guest elsewhere is reached over ssh.
                dialog::Op::Snap(s) => proxmox::ops::run_all(&[proxmox::ops::on_node(&t.node, proxmox::ops::snap_cmd(t.kind, t.id, s))]),
                dialog::Op::Dump(d) => proxmox::ops::run_all(&[proxmox::ops::on_node(&t.node, proxmox::ops::dump_cmd(t.id, d))]),
                dialog::Op::Cmds { cmds, .. } => {
                    proxmox::ops::run_all(&cmds.iter().map(|c| proxmox::ops::on_node(&t.node, c.clone())).collect::<Vec<_>>())
                }
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
    let Some(t) = app.model.tasks(model::unix_now()).get(app.selected).map(|t| (*t).clone()) else { return };
    app.task_log = Some(TaskLog {
        upid: t.upid.clone(),
        title: format!("{} {} on {}", t.kind, t.id, t.node),
        lines: None,
        loading: true,
        back: 0,
        find: find::Find::default(),
    });
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

/// `H`: the graphs reach one step further back (an hour, a day, a week, a month, a year, and round).
fn cycle_span(app: &mut App) {
    app.rrd_span = app.rrd_span.next();
    app.model.host_rrd.clear();
    app.model.guest_rrd.clear();
    app.rrd_at = None;
    app.status = format!("graphs: last {}", app.rrd_span.short());
}

/// Read the history of what the screen graphs, on a thread of its own, now and
/// then while that screen is up.
fn refresh_rrd(app: &mut App, tx: &Sender<Job>) {
    if app.rrd_busy || app.rrd_at.is_some_and(|t| t.elapsed() < RRD_EVERY) {
        return;
    }
    let tx = tx.clone();
    let span = app.rrd_span;
    match app.view {
        View::Host | View::Guests => {
            let Some(node) = app.model.host_node().map(str::to_string) else { return };
            std::thread::Builder::new()
                .name("yagura-rrd".into())
                .spawn(move || {
                    let _ = tx.send(Job::HostRrd(span, proxmox::rrd::node(&node, span)));
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
                    let _ = tx.send(Job::GuestRrd(id, span, proxmox::rrd::guest(&node, kind, id, span)));
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
        let cfg = config::get();
        let sent: Vec<&str> = cur
            .iter()
            .filter(|(k, _)| !app.alerts_seen.contains(k) && webhook::wanted(&cfg.webhook_kinds, k))
            .map(|(_, t)| t.as_str())
            .collect();
        webhook::send(&cfg.webhook, &app.model.host, &sent, &cfg.webhook_format);
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
