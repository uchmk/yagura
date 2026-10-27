//! yagura-tui: the host and its Proxmox VE guests on one screen, in the
//! terminal. The collection is `yagura-core`; this only draws. Everything
//! slow (`pvesh`, `pct exec`) runs on threads of its own and reports over a
//! channel, so the keys never wait.

mod model;
mod ui;

use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use yagura_core::Msg;
use yagura_core::proxmox::rrd::Point;
use yagura_core::proxmox::{self, Cluster, Inside, Power, Status};

use model::{Model, Sort, View, bytes};

const PVE_EVERY: Duration = Duration::from_secs(5);
/// How often the graphs on screen are read again (the history has a point a minute).
const RRD_EVERY: Duration = Duration::from_secs(20);
/// How many `pct exec` run at once when the containers are looked into in the
/// background.
const PARALLEL: usize = 4;
const SAMPLE: Duration = Duration::from_secs(1);

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
}

enum Job {
    Cluster(Cluster),
    Inside(u32, Inside),
    Power(u32, Power, Result<(), String>),
    Latest(String, Option<String>),
    HostRrd(Vec<Point>),
    GuestRrd(u32, Vec<Point>),
    GuestProcs(u32, Option<Vec<proxmox::GuestProc>>),
}

fn main() -> std::io::Result<()> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("yagura-tui {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!(
            "yagura-tui {}\n\nkeys: Tab view  j/k move  / find  s sort  Enter graphs  Esc back  u upgrade  o start  d shutdown  r reboot  ? help  q quit",
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
                    if r.is_ok()
                        && matches!(p, Power::Upgrade)
                        && let Some(g) = app.model.cluster.guests.iter().find(|g| g.id == id)
                    {
                        let kind = g.kind;
                        start_inspect(&mut app.model, id, kind, jobs_tx);
                    }
                    app.status = match r {
                        Ok(()) => format!("{id}: {} done", p.verb()),
                        Err(e) => format!("{id}: {} failed: {e}", p.verb()),
                    }
                }
            }
        }
        prefetch(app, jobs_tx);
        refresh_rrd(app, jobs_tx);
        refresh_procs(app, jobs_tx);
        let rows = match app.view {
            View::Guests => app.model.guests(app.sort).len(),
            View::Graph(id) => app.model.guest_procs.get(&id).and_then(|p| p.as_ref()).map_or(0, Vec::len),
            View::Procs => app.model.procs.len(),
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
            KeyCode::Esc if matches!(app.view, View::Graph(_)) => {
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
            _ => {}
        }
    }
}

/// Ask for a power change of the selected guest; nothing runs until `y`.
fn ask(app: &mut App, power: Power) {
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
    app.confirm = Some(Ask { id, name, kind, power, note });
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
                let low = g.maxdisk.saturating_sub(g.disk) < LOW_FREE || model::disk_pct(g).is_some_and(|p| p >= model::DISK_FULL);
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
    app.busy.push((a.id, a.power, Instant::now()));
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-power".into())
        .spawn(move || {
            let _ = tx.send(Job::Power(a.id, a.power, proxmox::power(a.kind, a.id, a.power)));
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
    std::thread::Builder::new()
        .name("yagura-ps".into())
        .spawn(move || {
            let _ = tx.send(Job::GuestProcs(id, proxmox::processes(kind, id)));
        })
        .expect("spawn the ps thread");
    app.procs_busy = true;
    app.procs_at = Some(Instant::now());
}
