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
use yagura_core::proxmox::{self, Cluster, Inside, Power, Status};

use model::{Model, Sort, View};

const PVE_EVERY: Duration = Duration::from_secs(5);
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
}

fn main() -> std::io::Result<()> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("yagura-tui {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!(
            "yagura-tui {}\n\nkeys: Tab view  j/k move  s sort  Enter look inside (pct exec / qm guest exec)  o start  d shutdown  r reboot  q quit",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    let (rx, _ctl) = yagura_core::spawn(None, SAMPLE);
    let (jobs_tx, jobs) = unbounded();
    spawn_pve(jobs_tx.clone());

    let mut app = App { model: Model::default(), view: View::Guests, sort: Sort::Id, selected: 0, status: String::new(), confirm: None };
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
                Job::Power(id, p, r) => {
                    app.status = match r {
                        Ok(()) => format!("{id}: {} done", p.verb()),
                        Err(e) => format!("{id}: {} failed: {e}", p.verb()),
                    }
                }
            }
        }
        prefetch(app, jobs_tx);
        let rows = match app.view {
            View::Guests => app.model.cluster.guests.len(),
            View::Procs => app.model.procs.len(),
        };
        app.selected = app.selected.min(rows.saturating_sub(1));
        term.draw(|f| ui::draw(f, app))?;

        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(k) = event::read()? else { continue };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match k.code {
            _ if app.confirm.is_some() => answer(app, k.code, jobs_tx),
            KeyCode::Char('o') if app.view == View::Guests => ask(app, Power::Start),
            KeyCode::Char('d') if app.view == View::Guests => ask(app, Power::Shutdown),
            KeyCode::Char('r') if app.view == View::Guests => ask(app, Power::Reboot),
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            KeyCode::Tab => {
                app.view = if app.view == View::Guests { View::Procs } else { View::Guests };
                app.selected = 0;
                // Drop whatever the terminal still shows from the other view.
                term.clear()?;
            }
            KeyCode::Char('j') | KeyCode::Down => app.selected = (app.selected + 1).min(rows.saturating_sub(1)),
            KeyCode::Char('k') | KeyCode::Up => app.selected = app.selected.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => app.selected = 0,
            KeyCode::Char('G') | KeyCode::End => app.selected = rows.saturating_sub(1),
            KeyCode::Char('s') => app.sort = app.sort.next(),
            KeyCode::Enter if app.view == View::Guests => look_inside(app, jobs_tx),
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

/// Start looking into the selected running guest (again, if it was looked
/// into before), unless one is already going for it.
fn look_inside(app: &mut App, tx: &Sender<Job>) {
    let Some(g) = app.model.guests(app.sort).get(app.selected).map(|g| (g.id, g.kind, g.status)) else { return };
    let (id, kind, status) = g;
    if status != Status::Running {
        app.status = "only a running guest can be looked into".into();
        return;
    }
    app.status.clear();
    start_inspect(&mut app.model, id, kind, tx);
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
        Msg::Procs { procs, .. } => m.procs = procs,
        _ => {}
    }
}
