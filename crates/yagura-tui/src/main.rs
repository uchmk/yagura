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

use model::{Model, Sort, View};

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
}

/// What the user asked of a guest, shown until they answer.
#[derive(Clone)]
pub struct Ask {
    pub id: u32,
    pub name: String,
    pub kind: proxmox::Kind,
    pub power: Power,
}

enum Job {
    Cluster(Cluster),
    Inside(u32, Inside),
    Power(u32, Power, Result<(), String>),
    Latest(String, Option<String>),
    HostRrd(Vec<Point>),
    GuestRrd(u32, Vec<Point>),
}

fn main() -> std::io::Result<()> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("yagura-tui {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!(
            "yagura-tui {}\n\nkeys: Tab view  j/k move  s sort  Enter graphs  Esc back  o start  d shutdown  r reboot  q quit",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    let (rx, _ctl) = yagura_core::spawn(None, SAMPLE);
    let (jobs_tx, jobs) = unbounded();
    spawn_pve(jobs_tx.clone());

    let mut app = App {
        model: Model::default(),
        view: View::Host,
        sort: Sort::Id,
        selected: 0,
        status: String::new(),
        confirm: None,
        rrd_at: None,
        rrd_busy: false,
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
                Job::Power(id, p, r) => {
                    app.status = match r {
                        Ok(()) => format!("{id}: {} done", p.verb()),
                        Err(e) => format!("{id}: {} failed: {e}", p.verb()),
                    }
                }
            }
        }
        prefetch(app, jobs_tx);
        refresh_rrd(app, jobs_tx);
        let rows = match app.view {
            View::Guests => app.model.cluster.guests.len(),
            View::Procs => app.model.procs.len(),
            View::Host | View::Graph(_) => 0,
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
            _ if app.confirm.is_some() => answer(app, k.code, jobs_tx),
            KeyCode::Char('o') if app.view == View::Guests => ask(app, Power::Start),
            KeyCode::Char('d') if app.view == View::Guests => ask(app, Power::Shutdown),
            KeyCode::Char('r') if app.view == View::Guests => ask(app, Power::Reboot),
            KeyCode::Esc if matches!(app.view, View::Graph(_)) => {
                app.view = View::Guests;
                app.rrd_at = None;
                term.clear()?;
            }
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
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
            KeyCode::Char('s') => app.sort = app.sort.next(),
            KeyCode::Enter if app.view == View::Guests => open_graphs(app),
            _ => {}
        }
    }
}

/// Ask for a power change of the selected guest; nothing runs until `y`.
fn ask(app: &mut App, power: Power) {
    let Some(g) = app.model.guests(app.sort).get(app.selected).map(|g| (g.id, g.name.clone(), g.kind, g.status, g.template)) else {
        return;
    };
    let (id, name, kind, status, template) = g;
    if template || !power.fits(status) {
        app.status = format!("{id}: can not {} a guest in this state", power.verb());
        return;
    }
    app.status.clear();
    app.confirm = Some(Ask { id, name, kind, power });
}

/// The answer to a pending question: `y` runs it on a thread of its own, any
/// other key drops it.
fn answer(app: &mut App, key: KeyCode, tx: &Sender<Job>) {
    let Some(a) = app.confirm.take() else { return };
    if key != KeyCode::Char('y') {
        app.status = "cancelled".into();
        return;
    }
    app.status = format!("{}: {}...", a.id, a.power.verb());
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
        View::Host => {
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
