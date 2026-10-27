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
use yagura_core::proxmox::{self, Cluster, Inside, Status};

use model::{Model, Sort, View};

const PVE_EVERY: Duration = Duration::from_secs(5);
const SAMPLE: Duration = Duration::from_secs(1);

pub struct App {
    pub model: Model,
    pub view: View,
    pub sort: Sort,
    pub selected: usize,
    /// Names the screen could not say because the work is still going.
    pub status: String,
}

enum Job {
    Cluster(Cluster),
    Inside(u32, Inside),
}

fn main() -> std::io::Result<()> {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("yagura-tui {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!("yagura-tui {}\n\nkeys: Tab view  j/k move  s sort  Enter look inside (pct exec)  q quit", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let (rx, _ctl) = yagura_core::spawn(None, SAMPLE);
    let (jobs_tx, jobs) = unbounded();
    spawn_pve(jobs_tx.clone());

    let mut app = App { model: Model::default(), view: View::Guests, sort: Sort::Id, selected: 0, status: String::new() };
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
                    app.model.inside.insert(id, i);
                }
            }
        }
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
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            KeyCode::Tab => {
                app.view = if app.view == View::Guests { View::Procs } else { View::Guests };
                app.selected = 0;
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

/// Start `pct exec` on the selected running container, unless one is already
/// going for it.
fn look_inside(app: &mut App, tx: &Sender<Job>) {
    let Some(g) = app.model.guests(app.sort).get(app.selected).map(|g| (g.id, g.kind, g.status)) else { return };
    let (id, kind, status) = g;
    if kind != proxmox::Kind::Lxc || status != Status::Running {
        app.status = "pct exec needs a running container".into();
        return;
    }
    if app.model.looking.contains(&id) {
        return;
    }
    app.status.clear();
    app.model.looking.push(id);
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("yagura-inspect".into())
        .spawn(move || {
            let _ = tx.send(Job::Inside(id, proxmox::inspect(id)));
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
