//! Drawing. Colours are the terminal's own: red for what needs a look (swap
//! near its limit, a failed read), a blue background for the selection (not
//! reverse video: where the terminal's default colours coincide, the text of
//! a reversed row vanishes). Only
//! ASCII symbols: some terminals draw "ambiguous width" ones (arrows, `…`)
//! two columns wide, which leaves stale cells behind when the view changes.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::bar;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table, TableState};
use yagura_core::proxmox::{Kind, Note, Status, apps};

use crate::App;
use crate::model::{DISK_FULL, Model, View, age, bytes, disk_pct};
use yagura_core::proxmox::rrd::Point;

const RED: Style = Style::new().fg(Color::Red);
const SELECTED: Style = Style::new().bg(Color::Blue).add_modifier(Modifier::BOLD);
const DIM: Style = Style::new().fg(Color::DarkGray);

pub fn draw(f: &mut Frame, app: &App) {
    // About a fifth of the height for graphs above the guests (the host's, or the entered guest's), when the screen is tall enough to spare it.
    let tall = f.area().height >= 30;
    let table = app.view == View::Guests;
    let inside_h = if table { 7 } else { 0 };
    let strip_h = if tall && (table || matches!(app.view, View::Graph(_))) { (f.area().height / 5).clamp(6, 12) } else { 0 };
    let [head, strip_area, body, detail, foot] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(strip_h),
        Constraint::Min(5),
        Constraint::Length(inside_h),
        Constraint::Length(1),
    ])
    .areas(f.area());
    header(f, app, head);
    if strip_h > 0 {
        strip(f, app, strip_area);
    }
    match app.view {
        View::Guests => guests(f, app, body),
        View::Procs => procs(f, app, body),
        View::Host => host_graphs(f, app, body),
        View::Graph(id) => guest_procs(f, app, id, body),
    }
    if table {
        inside(f, app, detail);
    }
    footer(f, app, foot);
    if app.help {
        help(f);
    }
}

/// The key list and what the marks mean, over everything else.
fn help(f: &mut Frame) {
    const LINES: [&str; 26] = [
        "KEYS",
        "  Tab        switch view: host / guests / this machine's processes",
        "  j k        move down / up      g G  top / bottom",
        "  /          find a guest by name or id (Enter keep, Esc clear)",
        "  s          sort: id, cpu, mem, swap, problems (worst first)",
        "  Enter      open a guest: graphs and the processes inside it",
        "  Esc        back from a guest",
        "  u          install the waiting updates in the guest (asks first)",
        "  o d r      start / shut down / reboot the guest (asks first)",
        "  q          quit",
        "",
        "MARKS",
        "  UPD 3      3 packages can be updated;  3*  the guest wants a reboot",
        "  UPD 3+1h   1 more package is held on purpose (apt-mark hold)",
        "  red SWAP   swap is nearly full: the guest lacks memory",
        "  red DISK   the root disk is 90% full: updates may fail",
        "  red NEW    a newer version of the app exists (NEW column)",
        "  gray row   the guest is stopped",
        "",
        "The screen only reads. Nothing changes until you press y.",
        "",
        "PROBLEMS sort puts first: swap full, disk full (2 each),",
        "  reboot wanted, updates waiting (1 each).",
        "",
        "Press any key to close.",
        "",
    ];
    let area = f.area();
    let w = 70.min(area.width);
    let h = (LINES.len() as u16 + 2).min(area.height);
    let r = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h);
    f.render_widget(ratatui::widgets::Clear, r);
    f.render_widget(Paragraph::new(LINES.iter().map(|l| Line::raw(*l)).collect::<Vec<_>>()).block(Block::bordered().title(" help ")), r);
}

fn pct(v: f32) -> String {
    format!("{v:5.1}%")
}

fn header(f: &mut Frame, app: &App, area: Rect) {
    let m = &app.model;
    if let View::Graph(id) = app.view
        && let Some(g) = m.guests(app.sort).into_iter().find(|g| g.id == id)
    {
        let swap = g.swap.map_or("-".to_string(), |s| s.max.map_or(bytes(s.used), |max| format!("{}/{}", bytes(s.used), bytes(max))));
        let l1 = Line::from(vec![
            Span::styled(format!(" {} {} ", g.id, g.name), Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(format!("cpu {} of {} cores", pct(g.cpu * 100.0), g.cpus)),
        ]);
        let l2 = Line::from(format!(" mem {}/{}  swap {swap}  disk {}/{}", bytes(g.mem), bytes(g.maxmem), bytes(g.disk), bytes(g.maxdisk)));
        let l3 = Line::styled(
            format!(
                " node {}  up {}  tags {}",
                g.node,
                if g.status == Status::Running { age(g.uptime) } else { "stopped".into() },
                if g.tags.is_empty() { "-".into() } else { g.tags.join(",") }
            ),
            DIM,
        );
        f.render_widget(Paragraph::new(vec![l1, l2, l3]), area);
        return;
    }
    let swap_red = m.mem.swap_total > 0 && m.mem.swap_used * 10 >= m.mem.swap_total * 9;
    let load = m.load.map_or(String::new(), |(a, b, c)| format!("  load {a:.2} {b:.2} {c:.2}"));
    let l1 = Line::from(vec![
        Span::styled(format!(" {} ", if m.host.is_empty() { "host" } else { &m.host }), Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(format!("cpu {}{load}", pct(m.cpu))),
    ]);
    let l2 = Line::from(vec![
        Span::raw(format!(" mem {}/{}  ", bytes(m.mem.used), bytes(m.mem.total))),
        Span::styled(format!("swap {}/{}", bytes(m.mem.swap_used), bytes(m.mem.swap_total)), if swap_red { RED } else { Style::new() }),
    ]);
    let nodes = m.cluster.nodes.iter().map(|n| format!("{} {}", n.name, if n.online { "online" } else { "offline" })).collect::<Vec<_>>();
    let l3 = Line::styled(format!(" pve: {}", if nodes.is_empty() { note(m.cluster.note) } else { nodes.join(", ") }), DIM);
    f.render_widget(Paragraph::new(vec![l1, l2, l3]), area);
}

fn note(n: Option<Note>) -> String {
    match n {
        None => "reading...".into(),
        Some(Note::OtherOs) => "not Linux".into(),
        Some(Note::NoPvesh) => "no pvesh: not a Proxmox host".into(),
        Some(Note::Failed) => "pvesh failed (root needed?)".into(),
        Some(Note::Unreadable) => "pvesh output unreadable".into(),
    }
}

fn guests(f: &mut Frame, app: &App, area: Rect) {
    let m = &app.model;
    let list = m.guests(app.sort);
    let rows = list.iter().map(|g| {
        let rate = m.rates.get(&g.id).copied().unwrap_or_default();
        let (swap, swap_style) = match g.swap {
            Some(s) => (
                s.max.map_or(bytes(s.used), |max| format!("{}/{}", bytes(s.used), bytes(max))),
                if s.near_limit() { RED.add_modifier(Modifier::BOLD) } else { Style::new() },
            ),
            None => ("-".into(), DIM),
        };
        let run = g.status == Status::Running;
        let (os, upd, upd_style) = look(m, g.id, run);
        let (app_name, ver, new, new_style) = app_cells(m, g.id);
        let (state, state_style) = match g.status {
            Status::Running => ("running", Style::new().fg(Color::Green)),
            Status::Stopped => ("stopped", DIM),
            Status::Other => ("other", Style::new().fg(Color::Yellow)),
        };
        let (disk, disk_style) = match disk_pct(g) {
            Some(p) => (format!("{p:3.0}%"), if p >= DISK_FULL { RED.add_modifier(Modifier::BOLD) } else { Style::new() }),
            None => (String::new(), DIM),
        };
        let base = if run { Style::new() } else { DIM };
        Row::new([
            Cell::from(g.id.to_string()),
            Cell::from(if g.kind == Kind::Lxc { "ct" } else { "vm" }),
            Cell::from(g.name.clone()),
            Cell::from(state).style(state_style),
            Cell::from(if run { pct(g.cpu * 100.0) } else { String::new() }),
            Cell::from(format!("{}/{}", bytes(g.mem), bytes(g.maxmem))),
            Cell::from(disk).style(disk_style),
            Cell::from(swap).style(swap_style),
            Cell::from(if run { format!("{}/{}", bytes(rate.rx as u64), bytes(rate.tx as u64)) } else { String::new() }),
            Cell::from(if run { age(g.uptime) } else { String::new() }),
            Cell::from(app_name),
            Cell::from(ver),
            Cell::from(new).style(new_style),
            Cell::from(os),
            Cell::from(upd).style(upd_style),
            Cell::from(g.tags.join(",")),
        ])
        .style(base)
    });
    let head = Row::new([
        "ID",
        "",
        "NAME",
        "STATE",
        "CPU",
        "MEM",
        "DISK",
        "SWAP",
        "NET in/out /s",
        "UP",
        "APP",
        "VER",
        "NEW",
        "OS",
        "UPD",
        "TAGS",
    ])
    .style(Style::new().add_modifier(Modifier::BOLD));
    let widths = [
        Constraint::Length(6),
        Constraint::Length(2),
        Constraint::Min(12),
        Constraint::Length(8),
        Constraint::Length(7),
        Constraint::Length(13),
        Constraint::Length(5),
        Constraint::Length(11),
        Constraint::Length(16),
        Constraint::Length(8),
        Constraint::Length(11),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Length(16),
        Constraint::Length(5),
        Constraint::Min(6),
    ];
    let (total, running, updates, reboot) = m.summary();
    let find = if app.filtering {
        format!("  find: {}_", m.filter)
    } else if m.filter.is_empty() {
        String::new()
    } else {
        format!("  find: {}", m.filter)
    };
    let title = format!(
        " guests: {total}, {running} running, {} stopped, {updates} with updates, {reboot} need reboot  (sort: {}){find} ",
        total - running,
        app.sort.label()
    );
    let t = Table::new(rows, widths).header(head).block(Block::new().borders(Borders::TOP).title(title)).row_highlight_style(SELECTED);
    let mut st = TableState::default().with_selected(Some(app.selected));
    f.render_stateful_widget(t, area, &mut st);
}

/// The OS and the upgradable count of a guest for the table: blank until it
/// has been looked into, `*` after the count when it wants a reboot.
fn look(m: &crate::model::Model, id: u32, running: bool) -> (String, String, Style) {
    if !running {
        return (String::new(), String::new(), Style::new());
    }
    match m.inside.get(&id) {
        None => (if m.looking.contains(&id) { "...".into() } else { String::new() }, String::new(), DIM),
        Some(i) if i.no_agent => ("no agent".into(), String::new(), DIM),
        Some(i) => {
            let n = i.upgradable.map_or("-".into(), |n| n.to_string());
            let n = if i.held > 0 { format!("{n}+{}h", i.held) } else { n };
            let (n, style) = if i.reboot_required { (format!("{n}*"), RED) } else { (n, Style::new()) };
            (short_os(i.os.as_deref().unwrap_or("")), n, style)
        }
    }
}

/// The app, its version and the latest one; red when it is behind.
fn app_cells(m: &crate::model::Model, id: u32) -> (String, String, String, Style) {
    let Some((name, ver, latest)) = m.app(id) else { return Default::default() };
    let behind = latest.is_some_and(|l| apps::is_older(ver, l));
    (
        name.to_string(),
        ver.to_string(),
        latest.unwrap_or("").to_string(),
        if behind { RED.add_modifier(Modifier::BOLD) } else { Style::new() },
    )
}

/// `Debian GNU/Linux 12 (bookworm)` as `Debian 12`.
pub fn short_os(name: &str) -> String {
    let name = name.replace(" GNU/Linux", "");
    name.split(" (").next().unwrap_or("").trim().to_string()
}

fn procs(f: &mut Frame, app: &App, area: Rect) {
    let list = app.model.procs_sorted();
    let rows = list.iter().map(|p| {
        Row::new([
            Cell::from(p.pid.to_string()),
            Cell::from(p.user.clone().unwrap_or_default()),
            Cell::from(format!("{:6.1}", p.cpu)),
            Cell::from(bytes(p.mem)),
            Cell::from(p.name.clone()),
        ])
    });
    let head = Row::new(["PID", "USER", "CPU%", "MEM", "NAME"]).style(Style::new().add_modifier(Modifier::BOLD));
    let widths = [Constraint::Length(8), Constraint::Length(12), Constraint::Length(7), Constraint::Length(8), Constraint::Min(10)];
    let t = Table::new(rows, widths)
        .header(head)
        .block(Block::new().borders(Borders::TOP).title(" processes (this host) "))
        .row_highlight_style(SELECTED);
    let mut st = TableState::default().with_selected(Some(app.selected));
    f.render_stateful_widget(t, area, &mut st);
}

/// The processes inside the entered guest.
fn guest_procs(f: &mut Frame, app: &App, id: u32, area: Rect) {
    let m = &app.model;
    let name = m.cluster.guests.iter().find(|g| g.id == id).map_or(String::new(), |g| g.name.clone());
    let title = format!(" processes in {id} {name} ");
    let list = match m.guest_procs.get(&id) {
        None => {
            f.render_widget(Paragraph::new(Line::styled(" reading...", DIM)).block(Block::new().borders(Borders::TOP).title(title)), area);
            return;
        }
        Some(None) => {
            let why = " no process list (needs a running Linux guest; a VM needs the guest agent)";
            f.render_widget(Paragraph::new(Line::styled(why, DIM)).block(Block::new().borders(Borders::TOP).title(title)), area);
            return;
        }
        Some(Some(l)) => l,
    };
    let rows = list.iter().map(|p| {
        Row::new([
            Cell::from(p.pid.to_string()),
            Cell::from(p.user.clone()),
            Cell::from(format!("{:6.1}", p.cpu)),
            Cell::from(bytes(p.mem)),
            Cell::from(p.name.clone()),
        ])
    });
    let head = Row::new(["PID", "USER", "CPU%", "MEM", "COMMAND"]).style(Style::new().add_modifier(Modifier::BOLD));
    let widths = [Constraint::Length(8), Constraint::Length(16), Constraint::Length(7), Constraint::Length(8), Constraint::Min(10)];
    let t = Table::new(rows, widths).header(head).block(Block::new().borders(Borders::TOP).title(title)).row_highlight_style(SELECTED);
    let mut st = TableState::default().with_selected(Some(app.selected));
    f.render_stateful_widget(t, area, &mut st);
}

/// When a stopped guest last started, for the detail pane.
fn last_run(at: Option<u64>) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    match at {
        Some(t) => format!("stopped; last started {} ago (task log)", age(now.saturating_sub(t))),
        None => "stopped; no start in the task log".into(),
    }
}

/// The container under the cursor, as `pct exec` found it.
fn inside(f: &mut Frame, app: &App, area: Rect) {
    let m = &app.model;
    let Some(g) = m.guests(app.sort).get(app.selected).copied() else { return };
    let mut lines = vec![];
    if m.looking.contains(&g.id) {
        lines.push(Line::styled("looking inside...", DIM));
    } else if let Some(i) = m.inside.get(&g.id) {
        if i.no_agent {
            lines.push(Line::styled("no answer from the QEMU guest agent (is it installed and enabled?)", DIM));
        }
        lines.push(Line::raw(format!("os: {}", i.os.as_deref().unwrap_or("?"))));
        let n = |v: Option<u32>| v.map_or("?".into(), |v| v.to_string());
        let stale = i.lists_age.map_or(String::new(), |a| format!(" (apt lists {} old)", age(a)));
        let held = if i.held > 0 { format!("   held: {}", i.held) } else { String::new() };
        lines.push(Line::raw(format!("packages: {}   upgradable: {}{held}{stale}", n(i.packages), n(i.upgradable))));
        if i.reboot_required {
            lines.push(Line::styled("reboot required", RED));
        }
    } else if g.status == Status::Stopped {
        lines.push(Line::styled(last_run(m.last_start.get(&g.id).copied().flatten()), DIM));
    } else {
        lines.push(Line::styled("Enter: look inside (pct exec / qm guest exec)", DIM));
    }
    let title = format!(" {} {} ", g.id, g.name);
    f.render_widget(Paragraph::new(lines).block(Block::new().borders(Borders::TOP).title(title)), area);
}

fn footer(f: &mut Frame, app: &App, area: Rect) {
    if let Some(a) = &app.confirm {
        let note = if a.note.is_empty() { String::new() } else { format!(" - {}", a.note) };
        let q = format!(" {} {} {}{note} (y/n) ", a.power.verb(), a.id, a.name);
        f.render_widget(Paragraph::new(q).style(RED.add_modifier(Modifier::BOLD)), area);
        return;
    }
    if app.filtering {
        f.render_widget(Paragraph::new(" type a name or id  Enter keep  Esc clear").style(DIM), area);
        return;
    }
    let keys = match app.view {
        View::Host => " Tab guests  ? help  q quit",
        View::Guests => " Tab view  j/k move  / find  s sort  Enter graphs  u upgrade  o start  d shutdown  r reboot  ? help  q quit",
        View::Procs => " Tab host  j/k move  ? help  q quit",
        View::Graph(_) => " Esc back  j/k move  u upgrade  ? help  q quit",
    };
    let mut text = keys.to_string();
    if let Some((id, p, since)) = app.busy.first() {
        text = format!("{text}   {id}: {}... {}s{}", p.verb(), since.elapsed().as_secs(), if app.busy.len() > 1 { " (+more)" } else { "" });
    }
    if !app.status.is_empty() {
        text = format!("{text}   {}", app.status);
    }
    f.render_widget(Paragraph::new(text).style(DIM), area);
}

/// Bars made of ASCII only (see the top of this file).
const BARS: bar::Set = bar::Set {
    full: "#",
    seven_eighths: "#",
    three_quarters: "#",
    five_eighths: "+",
    half: "+",
    three_eighths: ":",
    one_quarter: ":",
    one_eighth: ".",
    empty: " ",
};

/// One graph: a title with the latest value, and the values oldest first.
struct Chart {
    title: String,
    values: Vec<f64>,
    /// The value a full bar stands for; 0 means the largest value seen.
    max: f64,
    /// How far back the leftmost column reaches, shown under the graph.
    span: &'static str,
    percent: bool,
}

fn percent(label: &str, values: Vec<f64>) -> Chart {
    let last = values.last().copied().unwrap_or(0.0);
    Chart { title: format!(" {label} {last:.1}% "), values, max: 100.0, span: "1h", percent: true }
}

fn rate(label: &str, values: Vec<f64>) -> Chart {
    let last = values.last().copied().unwrap_or(0.0);
    Chart { title: format!(" {label} {}/s ", bytes(last as u64)), values, max: 0.0, span: "1h", percent: false }
}

fn gpu_chart(values: &[f32]) -> Chart {
    let mut c = percent("gpu", values.iter().map(|v| *v as f64).collect());
    c.span = "5m";
    c
}

fn no_gpu() -> Chart {
    Chart { title: " gpu: none found ".into(), values: vec![], max: 100.0, span: "5m", percent: true }
}

fn ratio(a: f64, b: f64) -> f64 {
    if b > 0.0 { a / b * 100.0 } else { 0.0 }
}

fn series(pts: &[Point], f: impl Fn(&Point) -> f64) -> Vec<f64> {
    pts.iter().map(f).collect()
}

/// The charts into a grid, two to a row where the screen is wide enough.
fn grid(f: &mut Frame, area: Rect, charts: &[Chart]) {
    let cols = if area.width >= 80 { 2 } else { 1 };
    let rows = charts.len().div_ceil(cols).max(1);
    let row_areas = Layout::vertical(vec![Constraint::Ratio(1, rows as u32); rows]).split(area);
    for (r, row) in charts.chunks(cols).enumerate() {
        let cells = Layout::horizontal(vec![Constraint::Ratio(1, cols as u32); cols]).split(row_areas[r]);
        for (c, ch) in row.iter().enumerate() {
            chart(f, cells[c], ch);
        }
    }
}

/// One boxed graph.
fn chart(f: &mut Frame, area: Rect, ch: &Chart) {
    let max = if ch.percent {
        // A full bar is the peak rounded up, so a quiet graph still has a shape; the scale is printed on the box.
        let peak = ch.values.iter().copied().fold(0.0, f64::max);
        [10.0, 20.0, 25.0, 50.0, 100.0].into_iter().find(|n| peak <= *n).unwrap_or(100.0)
    } else if ch.max > 0.0 {
        ch.max
    } else {
        ch.values.iter().copied().fold(1.0, f64::max)
    };
    let scale = if ch.percent { format!(" full = {max:.0}% ") } else { format!(" full = {}/s ", bytes(max as u64)) };
    let mut block = Block::new().borders(Borders::ALL).title(ch.title.clone());
    if ch.title.chars().count() + scale.chars().count() + 2 <= area.width as usize {
        block = block.title(Line::from(scale).right_aligned());
    }
    if !ch.values.is_empty() {
        block = block.title_bottom(Line::from(format!(" -{} ", ch.span)).left_aligned()).title_bottom(Line::from(" now ").right_aligned());
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    // Every column is the average of its share of the series, so the whole span always fits the box.
    let (w, n) = (inner.width as usize, ch.values.len());
    let data: Vec<u64> = if n == 0 || w == 0 {
        Vec::new()
    } else {
        (0..w)
            .map(|c| {
                let a = (c * n / w).min(n - 1);
                let b = ((c + 1) * n / w).clamp(a + 1, n);
                let s = &ch.values[a..b];
                let v = s.iter().sum::<f64>() / s.len() as f64;
                (v / max * 1000.0).clamp(0.0, 1000.0) as u64
            })
            .collect()
    };
    f.render_widget(Sparkline::default().data(&data).max(1000).bar_set(BARS).style(Style::new().fg(Color::Cyan)), inner);
}

/// The entered guest's last hour in the same strip: cpu, memory, network, disk.
fn guest_strip(f: &mut Frame, app: &App, id: u32, area: Rect) {
    let m: &Model = &app.model;
    let name = m.cluster.guests.iter().find(|g| g.id == id).map_or(String::new(), |g| g.name.clone());
    let Some(p) = m.guest_rrd.get(&id).filter(|p| !p.is_empty()) else {
        f.render_widget(Paragraph::new(Line::styled(format!(" {id} {name}: reading... (last hour)"), DIM)), area);
        return;
    };
    let mut charts =
        vec![percent(&format!("{id} {name} cpu"), series(p, |x| x.cpu * 100.0)), percent("mem", series(p, |x| ratio(x.mem, x.maxmem)))];
    if area.width >= 60 {
        charts.push(rate("net in", series(p, |x| x.netin)));
    }
    if area.width >= 100 {
        charts.push(rate("net out", series(p, |x| x.netout)));
    }
    if area.width >= 130 {
        charts.push(rate("disk read", series(p, |x| x.diskread)));
    }
    if area.width >= 160 {
        charts.push(rate("disk write", series(p, |x| x.diskwrite)));
    }
    let cells = Layout::horizontal(vec![Constraint::Ratio(1, charts.len() as u32); charts.len()]).split(area);
    for (c, ch) in charts.iter().enumerate() {
        chart(f, cells[c], ch);
    }
}

/// The host's last hour in one row of small graphs: cpu, memory, network and GPU.
fn strip(f: &mut Frame, app: &App, area: Rect) {
    if let View::Graph(id) = app.view {
        return guest_strip(f, app, id, area);
    }
    let m: &Model = &app.model;
    let p = &m.host_rrd;
    let mut charts = vec![percent("host cpu", series(p, |x| x.cpu * 100.0)), percent("host mem", series(p, |x| ratio(x.mem, x.maxmem)))];
    if area.width >= 60 {
        charts.push(rate("net in", series(p, |x| x.netin)));
    }
    if area.width >= 100 {
        charts.push(rate("net out", series(p, |x| x.netout)));
    }
    if area.width >= 130 {
        charts.push(if m.gpu.is_empty() { no_gpu() } else { gpu_chart(&m.gpu.iter().copied().collect::<Vec<f32>>()) });
    }
    if p.is_empty() {
        f.render_widget(Paragraph::new(Line::styled(" host graphs: reading... (last hour)", DIM)), area);
        return;
    }
    let cells = Layout::horizontal(vec![Constraint::Ratio(1, charts.len() as u32); charts.len()]).split(area);
    for (c, ch) in charts.iter().enumerate() {
        chart(f, cells[c], ch);
    }
}

/// The node's last hour, and the GPU since this screen started.
fn host_graphs(f: &mut Frame, app: &App, area: Rect) {
    let m: &Model = &app.model;
    let p = &m.host_rrd;
    let mut charts = vec![
        percent("cpu", series(p, |x| x.cpu * 100.0)),
        percent("mem", series(p, |x| ratio(x.mem, x.maxmem))),
        percent("iowait", series(p, |x| x.iowait * 100.0)),
        percent("root disk", series(p, |x| ratio(x.disk, x.maxdisk))),
        rate("net in", series(p, |x| x.netin)),
        rate("net out", series(p, |x| x.netout)),
    ];
    if m.gpu.is_empty() {
        charts.push(no_gpu());
    } else {
        charts.push(gpu_chart(&m.gpu.iter().copied().collect::<Vec<f32>>()));
    }
    if p.is_empty() {
        charts.truncate(6);
    }
    grid(f, area, &charts);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Model, Sort};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use yagura_core::proxmox::{Cluster, Guest, Swap};

    fn app(view: View) -> App {
        let mut model = Model::default();
        model.cluster = Cluster {
            guests: vec![Guest {
                id: 101,
                name: "web".into(),
                status: Status::Running,
                swap: Some(Swap { used: 95, max: Some(100) }),
                ..Guest::default()
            }],
            ..Cluster::default()
        };
        App {
            model,
            view,
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
        }
    }

    fn render(a: &App) -> Buffer {
        let mut t = Terminal::new(TestBackend::new(170, 29)).unwrap();
        t.draw(|f| draw(f, a)).unwrap();
        t.backend().buffer().clone()
    }

    fn text(b: &Buffer) -> String {
        b.content().iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn a_guest_near_its_swap_limit_is_drawn_in_red() {
        let b = render(&app(View::Guests));
        assert!(text(&b).contains("web"));
        let red = b.content().iter().any(|c| c.symbol() == "9" && c.fg == Color::Red);
        assert!(red, "the swap cell should be red");
    }

    #[test]
    fn a_pending_power_change_asks_in_the_footer() {
        let mut a = app(View::Guests);
        a.confirm = Some(crate::Ask {
            id: 101,
            name: "web".into(),
            kind: Kind::Lxc,
            power: yagura_core::proxmox::Power::Reboot,
            note: String::new(),
        });
        assert!(text(&render(&a)).contains("reboot 101 web (y/n)"));
    }

    #[test]
    fn the_table_has_a_state_column() {
        let mut term = Terminal::new(TestBackend::new(160, 30)).unwrap();
        term.draw(|f| draw(f, &app(View::Guests))).unwrap();
        let b = term.backend().buffer().clone();
        let t = text(&b);
        assert!(t.contains("STATE") && t.contains("running"));
        assert!(b.content().iter().any(|c| c.symbol() == "r" && c.fg == Color::Green));
    }

    #[test]
    fn a_pending_upgrade_asks_in_the_footer() {
        let mut a = app(View::Guests);
        a.confirm = Some(crate::Ask {
            id: 101,
            name: "web".into(),
            kind: Kind::Lxc,
            power: yagura_core::proxmox::Power::Upgrade,
            note: "installs 5 updates, 1.2G free".into(),
        });
        assert!(text(&render(&a)).contains("upgrade 101 web - installs 5 updates, 1.2G free (y/n)"));
    }

    #[test]
    fn the_help_lists_the_keys_and_the_marks() {
        let mut a = app(View::Guests);
        a.help = true;
        let t = text(&render(&a));
        assert!(t.contains("KEYS") && t.contains("MARKS") && t.contains("Press any key"));
    }

    #[test]
    fn the_title_counts_guests_and_the_upgrade_question_says_what_it_does() {
        let mut a = app(View::Guests);
        a.confirm = Some(crate::Ask {
            id: 101,
            name: "web".into(),
            kind: Kind::Lxc,
            power: yagura_core::proxmox::Power::Shutdown,
            note: "stops it; connected users are cut off".into(),
        });
        let t = text(&render(&a));
        assert!(t.contains("connected users are cut off"));
        a.confirm = None;
        assert!(text(&render(&a)).contains("running"));
    }

    #[test]
    fn an_app_behind_its_latest_release_is_red() {
        let mut a = app(View::Guests);
        a.model.inside.insert(101, yagura_core::proxmox::Inside { apps: vec![("immich".into(), "3.1.0".into())], ..Default::default() });
        a.model.latest.insert("immich".into(), Some("v3.3.5".into()));
        let b = render(&a);
        assert!(text(&b).contains("immich"));
        assert!(b.content().iter().any(|c| c.symbol() == "3" && c.fg == Color::Red), "the NEW cell should be red");
        a.model.latest.insert("immich".into(), Some("v3.1.0".into()));
        assert!(!render(&a).content().iter().any(|c| c.symbol() == "3" && c.fg == Color::Red));
    }

    #[test]
    fn short_os_names_read_short() {
        assert_eq!(short_os("Debian GNU/Linux 12 (bookworm)"), "Debian 12");
        assert_eq!(short_os("Ubuntu 24.04.1 LTS"), "Ubuntu 24.04.1 LTS");
        assert_eq!(short_os(""), "");
    }

    #[test]
    fn scrolling_through_many_processes_does_not_panic() {
        let mut a = app(View::Procs);
        a.model.procs = (0..300)
            .map(|i| yagura_core::Proc {
                pid: i,
                name: format!("p\u{3042}\t{i}\u{1b}"),
                user: (i % 2 == 0).then(|| "root".into()),
                cpu: i as f32,
                ..Default::default()
            })
            .collect();
        let mut t = Terminal::new(TestBackend::new(170, 29)).unwrap();
        for s in (0..300).chain((0..300).rev()) {
            a.selected = s;
            t.draw(|f| draw(f, &a)).unwrap();
        }
    }

    #[test]
    fn the_graph_screens_draw_with_and_without_history() {
        use yagura_core::proxmox::rrd::Point;
        let pts: Vec<Point> = (0..70)
            .map(|i| Point {
                cpu: i as f64 / 70.0,
                mem: 5.0,
                maxmem: 10.0,
                disk: 1.0,
                maxdisk: 4.0,
                netin: i as f64 * 1000.0,
                ..Point::default()
            })
            .collect();
        for w in [20, 60, 100] {
            for v in [View::Host, View::Graph(101), View::Graph(999)] {
                let mut a = app(v);
                let mut t = Terminal::new(TestBackend::new(w, 24)).unwrap();
                t.draw(|f| draw(f, &a)).unwrap();
                a.model.host_rrd = pts.clone();
                a.model.guest_rrd.insert(101, pts.clone());
                a.model.push_gpu(Some(50.0));
                t.draw(|f| draw(f, &a)).unwrap();
            }
        }
        let mut a = app(View::Graph(101));
        a.model.guest_rrd.insert(101, pts);
        let mut t = Terminal::new(TestBackend::new(100, 40)).unwrap();
        t.draw(|f| draw(f, &a)).unwrap();
        let s = text(t.backend().buffer());
        assert!(s.contains("cpu 98.6%") && s.contains("mem 50.0%") && s.contains("net in 67K/s"), "{s}");
        assert!(s.contains('#'));
    }

    #[test]
    fn the_guests_screen_has_a_strip_of_host_graphs_on_a_tall_screen() {
        use yagura_core::proxmox::rrd::Point;
        let pts: Vec<Point> =
            (0..60).map(|i| Point { cpu: i as f64 / 60.0, mem: 5.0, maxmem: 10.0, netin: 1000.0, ..Point::default() }).collect();
        let mut a = app(View::Guests);
        a.model.host_rrd = pts;
        for (w, h) in [(80, 40), (140, 50), (140, 20)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            t.draw(|f| draw(f, &a)).unwrap();
            let s = text(t.backend().buffer());
            assert_eq!(s.contains("host cpu 98.3%"), h >= 30, "{w}x{h}");
            assert!(s.contains("web"), "{w}x{h}: the guest is still listed");
        }
        a.model.guest_rrd.insert(101, (0..60).map(|i| Point { cpu: i as f64 / 60.0, ..Point::default() }).collect());
        a.view = View::Graph(101);
        let mut t = Terminal::new(TestBackend::new(140, 50)).unwrap();
        t.draw(|f| draw(f, &a)).unwrap();
        let s = text(t.backend().buffer());
        assert!(s.contains("101 web cpu 98.3%") && s.contains("processes in 101 web"), "{s}");
    }

    #[test]
    fn a_guest_shows_its_processes() {
        use yagura_core::proxmox::GuestProc;
        let mut a = app(View::Graph(101));
        for h in [6, 20, 40] {
            let mut t = Terminal::new(TestBackend::new(100, h)).unwrap();
            t.draw(|f| draw(f, &a)).unwrap();
            assert!(text(t.backend().buffer()).contains("reading"), "{h}");
        }
        a.model.guest_procs.insert(101, None);
        assert!(text(&render(&a)).contains("no process list"));
        let p = GuestProc { pid: 7, user: "root".into(), cpu: 1.5, mem: 2048, name: "nginx -g daemon".into() };
        a.model.guest_procs.insert(101, Some(vec![p]));
        let s = text(&render(&a));
        assert!(s.contains("nginx") && s.contains("root"), "{s}");
    }

    #[test]
    fn both_views_draw_on_a_small_screen() {
        for v in [View::Guests, View::Procs, View::Host, View::Graph(101)] {
            let mut t = Terminal::new(TestBackend::new(20, 6)).unwrap();
            t.draw(|f| draw(f, &app(v))).unwrap();
        }
    }
}
