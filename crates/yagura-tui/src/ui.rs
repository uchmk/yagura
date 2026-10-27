//! Drawing. Colours are the terminal's own: red for what needs a look (swap
//! near its limit, a failed read), a blue background for the selection (not
//! reverse video: where the terminal's default colours coincide, the text of
//! a reversed row vanishes). Only
//! ASCII symbols: some terminals draw "ambiguous width" ones (arrows, `…`)
//! two columns wide, which leaves stale cells behind when the view changes.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use yagura_core::proxmox::{Kind, Note, Status};

use crate::App;
use crate::model::{View, age, bytes};

const RED: Style = Style::new().fg(Color::Red);
const SELECTED: Style = Style::new().bg(Color::Blue).add_modifier(Modifier::BOLD);
const DIM: Style = Style::new().fg(Color::DarkGray);

pub fn draw(f: &mut Frame, app: &App) {
    let inside_h = if app.view == View::Guests { 7 } else { 0 };
    let [head, body, detail, foot] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(5), Constraint::Length(inside_h), Constraint::Length(1)]).areas(f.area());
    header(f, app, head);
    match app.view {
        View::Guests => guests(f, app, body),
        View::Procs => procs(f, app, body),
    }
    if app.view == View::Guests {
        inside(f, app, detail);
    }
    footer(f, app, foot);
}

fn pct(v: f32) -> String {
    format!("{v:5.1}%")
}

fn header(f: &mut Frame, app: &App, area: Rect) {
    let m = &app.model;
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
        let base = if run { Style::new() } else { DIM };
        Row::new([
            Cell::from(g.id.to_string()),
            Cell::from(if g.kind == Kind::Lxc { "ct" } else { "vm" }),
            Cell::from(g.name.clone()),
            Cell::from(if run { pct(g.cpu * 100.0) } else { String::new() }),
            Cell::from(format!("{}/{}", bytes(g.mem), bytes(g.maxmem))),
            Cell::from(swap).style(swap_style),
            Cell::from(if run { format!("{}/{}", bytes(rate.rx as u64), bytes(rate.tx as u64)) } else { String::new() }),
            Cell::from(if run { age(g.uptime) } else { "stopped".into() }),
            Cell::from(os),
            Cell::from(upd).style(upd_style),
            Cell::from(g.tags.join(",")),
        ])
        .style(base)
    });
    let head = Row::new(["ID", "", "NAME", "CPU", "MEM", "SWAP", "NET in/out /s", "UP", "OS", "UPD", "TAGS"])
        .style(Style::new().add_modifier(Modifier::BOLD));
    let widths = [
        Constraint::Length(6),
        Constraint::Length(2),
        Constraint::Min(12),
        Constraint::Length(7),
        Constraint::Length(13),
        Constraint::Length(11),
        Constraint::Length(16),
        Constraint::Length(8),
        Constraint::Length(16),
        Constraint::Length(5),
        Constraint::Min(6),
    ];
    let title = format!(" guests (sort: {}) ", app.sort.label());
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
            let (n, style) = if i.reboot_required { (format!("{n}*"), RED) } else { (n, Style::new()) };
            (short_os(i.os.as_deref().unwrap_or("")), n, style)
        }
    }
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
        lines.push(Line::raw(format!("packages: {}   upgradable: {}{stale}", n(i.packages), n(i.upgradable))));
        if i.reboot_required {
            lines.push(Line::styled("reboot required", RED));
        }
    } else {
        lines.push(Line::styled("Enter: look inside (pct exec / qm guest exec)", DIM));
    }
    let title = format!(" {} {} ", g.id, g.name);
    f.render_widget(Paragraph::new(lines).block(Block::new().borders(Borders::TOP).title(title)), area);
}

fn footer(f: &mut Frame, app: &App, area: Rect) {
    if let Some(a) = &app.confirm {
        let q = format!(" {} {} {} (y/n) ", a.power.verb(), a.id, a.name);
        f.render_widget(Paragraph::new(q).style(RED.add_modifier(Modifier::BOLD)), area);
        return;
    }
    let keys = " Tab view  j/k move  s sort  Enter inside  o start  d shutdown  r reboot  q quit";
    let text = if app.status.is_empty() { keys.to_string() } else { format!("{keys}   {}", app.status) };
    f.render_widget(Paragraph::new(text).style(DIM), area);
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
        App { model, view, sort: Sort::Id, selected: 0, status: String::new(), confirm: None }
    }

    fn render(a: &App) -> Buffer {
        let mut t = Terminal::new(TestBackend::new(100, 20)).unwrap();
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
        a.confirm = Some(crate::Ask { id: 101, name: "web".into(), kind: Kind::Lxc, power: yagura_core::proxmox::Power::Reboot });
        assert!(text(&render(&a)).contains("reboot 101 web (y/n)"));
    }

    #[test]
    fn short_os_names_read_short() {
        assert_eq!(short_os("Debian GNU/Linux 12 (bookworm)"), "Debian 12");
        assert_eq!(short_os("Ubuntu 24.04.1 LTS"), "Ubuntu 24.04.1 LTS");
        assert_eq!(short_os(""), "");
    }

    #[test]
    fn both_views_draw_on_a_small_screen() {
        for v in [View::Guests, View::Procs] {
            let mut t = Terminal::new(TestBackend::new(20, 6)).unwrap();
            t.draw(|f| draw(f, &app(v))).unwrap();
        }
    }
}
