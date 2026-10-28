//! Drawing. Every frame paints the whole window from the app's state with
//! egui's painter, the way the mock's canvas did, and reports clicks, scroll
//! positions and divider drags back through [`Out`]; the app applies them
//! once the frame is drawn, so drawing only ever reads the state.

mod affinity;
mod confirm;
mod containers;
mod cpu;
mod devices;
mod disk;
pub mod fleet;
mod gpu;
mod graph;
mod help;
mod mem;
mod net;
mod overview;
mod procs;
mod sensors;
pub mod settings;
mod svc;
pub mod sys;
mod tiles;
mod zoomed;

use std::time::{SystemTime, UNIX_EPOCH};

use egui::{Align2, Color32, FontId, Id, Painter, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use ito_layout::Node;
use ito_layout::ui::{Look, dividers, from_egui, to_egui};

use crate::app::{App, Mode, Out, fmt_interval};
use crate::config::cmd::Act;
use crate::config::keys::render_seq;
use crate::config::theme::{self, ROW};
use crate::i18n::{tr, trf};
use crate::remote::Status;
use crate::screen::{Pane, Screen};
use crate::util::{fmt_bytes, fmt_uptime};
use crate::zoom::Zoom;

pub const FONT: f32 = 13.0;
const SMALL: f32 = 10.0;
const HEADER: f32 = 28.0;
const CRUMB: f32 = 24.0;
const STATUS: f32 = 24.0;
/// Between two panels, and around the body.
pub const GAP: f32 = 6.0;

pub fn font() -> FontId {
    FontId::monospace(FONT)
}

fn small_font() -> FontId {
    FontId::monospace(SMALL)
}

/// The painter with the width of one character of the monospace font, so
/// text can be cut to fit without asking egui to lay it out first.
#[derive(Clone)]
pub struct Pen {
    pub p: Painter,
    pub cw: f32,
}

impl Pen {
    pub fn text(&self, pos: Pos2, s: &str, color: Color32) -> Rect {
        self.p.text(pos, Align2::LEFT_CENTER, s, font(), color)
    }

    pub fn text_r(&self, pos: Pos2, s: &str, color: Color32) -> Rect {
        self.p.text(pos, Align2::RIGHT_CENTER, s, font(), color)
    }

    pub fn text_c(&self, pos: Pos2, s: &str, color: Color32) -> Rect {
        self.p.text(pos, Align2::CENTER_CENTER, s, font(), color)
    }

    /// The small font, for the thread numbers and clocks inside a tile.
    pub fn small(&self, pos: Pos2, align: Align2, s: &str, color: Color32) -> Rect {
        self.p.text(pos, align, s, small_font(), color)
    }

    /// `s` cut to `w` pixels, with `…` where it was cut.
    pub fn fit(&self, s: &str, w: f32) -> String {
        let room = (w / self.cw).floor().max(0.0) as usize;
        if crate::util::width(s) <= room {
            return s.to_string();
        }
        if room == 0 {
            return String::new();
        }
        // Whole characters up to one cell short, for the `…`.
        let mut out = String::new();
        let mut used = 0;
        for c in s.chars() {
            let n = crate::util::cells(c);
            if used + n > room - 1 {
                break;
            }
            used += n;
            out.push(c);
        }
        out.push('…');
        out
    }

    pub fn width(&self, s: &str) -> f32 {
        crate::util::width(s) as f32 * self.cw
    }

    /// The same pen drawing only inside `r`.
    pub fn clipped(&self, r: Rect) -> Pen {
        Pen { p: self.p.with_clip_rect(r.intersect(self.p.clip_rect())), cw: self.cw }
    }

    /// A key and its value on one row: the key dim in a column `kw` wide,
    /// the value cut to fit.
    pub fn kv(&self, r: Rect, kw: f32, key: &str, val: &str) {
        let y = r.center().y;
        self.text(pos2(r.left() + 4.0, y), &self.fit(key, kw - 6.0), theme::dim());
        self.text(pos2(r.left() + kw, y), &self.fit(val, r.width() - kw - 4.0), theme::fg());
    }

    /// A label on the left and a value on the right of a row; the label is
    /// cut where the value begins.
    pub fn pair(&self, r: Rect, left: &str, lc: Color32, right: &str, rc: Color32) {
        let y = r.center().y;
        let rr = self.text_r(pos2(r.right() - 6.0, y), right, rc);
        self.text(pos2(r.left() + 6.0, y), &self.fit(left, rr.left() - r.left() - 6.0 - self.cw), lc);
    }

    /// A small header over a block of a panel.
    pub fn section(&self, r: Rect, s: &str) {
        self.text(pos2(r.left() + 4.0, r.center().y), &self.fit(s, r.width()), theme::cyan().gamma_multiply(0.8));
    }
}

/// What one screen draws with: the frame's painter, the app, and where to
/// report what was clicked.
pub struct Cx<'a> {
    pub ui: &'a Ui,
    pub pen: Pen,
    pub out: &'a mut Out,
    pub app: &'a App,
}

impl Cx<'_> {
    pub fn focused(&self, p: Pane) -> bool {
        self.app.focus() == p
    }

    pub fn sel(&self, p: Pane) -> usize {
        self.app.view().sel[p.index()]
    }

    /// A row's background (the selection's colour when it is selected) and
    /// its click.
    pub fn row_bg(&mut self, p: Pane, i: usize, r: Rect) {
        if i == self.sel(p) {
            self.pen.p.rect_filled(r, 3.0, if self.focused(p) { theme::sel() } else { theme::sel_unfocused() });
        }
        let resp = self.ui.interact(r, Id::new(("row", self.app.screen, p.index(), i)), Sense::click());
        if resp.clicked() {
            self.out.click = Some((p, i, resp.double_clicked()));
        }
    }

    /// A graph in `r` that opens full size when it is double clicked.
    pub fn zoomable(&mut self, r: Rect, id: usize, zoom: Zoom) {
        // The series names the graph: `id` alone repeats across the
        // interfaces, drives and GPUs a screen lists.
        let key = ("zoom", self.app.screen, id, format!("{:?}", zoom.series));
        if self.ui.interact(r, Id::new(key), Sense::click()).double_clicked() {
            self.out.zoom = Some(zoom);
        }
    }

    /// The panels `default` stacks in column `p`, laid over `r` as the mouse
    /// left them: each panel's room, by its number. Their dividers are
    /// taken after the screen is drawn.
    pub fn stack(&mut self, p: Pane, r: Rect, default: Node<u8>) -> Vec<Rect> {
        let node = self.app.stack(p, default);
        let mut out = vec![Rect::NOTHING; node.leaves().iter().map(|&i| i as usize + 1).max().unwrap_or(0)];
        if r.width() < 1.0 || r.height() < 1.0 {
            return out;
        }
        for (i, part) in node.layout(from_egui(r), GAP) {
            out[i as usize] = to_egui(part);
        }
        self.out.stacks.push((p, node, r));
        out
    }

    /// `count` rows in `body`, scrolled so the selection shows; `row`
    /// draws one after its background.
    pub fn scroll_list(&mut self, p: Pane, body: Rect, count: usize, mut row: impl FnMut(&mut Cx, Rect, usize)) {
        let n = ((body.height() / ROW).floor() as usize).max(1);
        let sel = self.sel(p);
        let mut scroll = self.app.view().scroll[p.index()];
        if sel < scroll {
            scroll = sel;
        }
        if sel >= scroll + n {
            scroll = sel + 1 - n;
        }
        scroll = scroll.min(count.saturating_sub(n));
        self.out.visible[p.index()] = n;
        self.out.scroll[p.index()] = Some(scroll);
        for i in scroll..count.min(scroll + n) {
            let r = Rect::from_min_size(pos2(body.left(), body.top() + (i - scroll) as f32 * ROW), vec2(body.width(), ROW));
            self.row_bg(p, i, r);
            row(self, r, i);
        }
    }
}

/// Whether a panel laid out in `r` has room for its frame, its title and a
/// little of its body; smaller ones (a stack's divider dragged nearly to
/// its end) are left out.
pub fn shown(r: Rect) -> bool {
    r.is_finite() && r.width() >= 40.0 && r.height() >= 40.0
}

/// A row of `ROW` at `y` across `body`.
pub fn row_at(body: Rect, y: f32) -> Rect {
    Rect::from_min_size(pos2(body.left(), y), vec2(body.width(), ROW))
}

/// Key and value rows from `y` down, as many as fit in `body`; returns the
/// `y` under the last one.
pub fn kv_rows(pen: &Pen, body: Rect, mut y: f32, rows: &[(&str, String)]) -> f32 {
    // The keys' column fits the longest key, up to half the row, or more when the values are short.
    let longest = rows.iter().map(|(k, _)| pen.width(k)).fold(0.0, f32::max);
    let widest = rows.iter().map(|(_, v)| pen.width(v)).fold(0.0, f32::max);
    let cap = (body.width() * 0.5).max(body.width() - widest - pen.cw * 2.0).max(pen.cw * 6.0);
    let kw = (longest + pen.cw * 2.0).clamp(pen.cw * 6.0, cap);
    for (k, v) in rows {
        if y + ROW > body.bottom() + 0.5 {
            break;
        }
        pen.kv(row_at(body, y), kw, k, v);
        y += ROW;
    }
    y
}

/// A panel's frame and title; returns the room inside it.
pub fn panel(pen: &Pen, r: Rect, title: &str, info: &str, focused: bool) -> Rect {
    pen.p.rect_filled(r, 6.0, theme::bg_alt());
    pen.p.rect_stroke(
        r,
        6.0,
        Stroke::new(1.0, if focused { theme::cyan().gamma_multiply(0.45) } else { theme::line() }),
        StrokeKind::Inside,
    );
    let y = r.top() + 13.0;
    let t = pen.text(pos2(r.left() + 10.0, y), &pen.fit(title, r.width() - 20.0), if focused { theme::cyan() } else { theme::dim() });
    if !info.is_empty() {
        let room = r.right() - 10.0 - (t.right() + 12.0);
        pen.text_r(pos2(r.right() - 10.0, y), &pen.fit(info, room), theme::dim());
    }
    Rect::from_min_max(pos2(r.left() + 6.0, r.top() + 26.0), pos2(r.right() - 6.0, r.bottom() - 6.0))
}

/// A rounded pill with text in it, `w` wide; the badge in the status bar
/// and the active tab.
fn pill(pen: &Pen, r: Rect, s: &str, bg: Color32, fg: Color32) {
    pen.p.rect_filled(r, 4.0, bg);
    pen.text_c(r.center(), s, fg);
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Draw the window and report what the pointer did.
pub fn frame(ui: &Ui, app: &App) -> Out {
    let mut out = Out::default();
    let full = ui.available_rect_before_wrap();
    let cw = ui.ctx().fonts_mut(|f| f.glyph_width(&font(), 'M'));
    let pen = Pen { p: ui.painter_at(full), cw };
    pen.p.rect_filled(full, 0.0, theme::bg());

    let header = Rect::from_min_size(full.min, vec2(full.width(), HEADER));
    draw_header(ui, &pen, app, header, &mut out);
    let crumb = Rect::from_min_size(pos2(full.left(), header.bottom()), vec2(full.width(), CRUMB));
    draw_crumb(&pen, app, crumb);
    let status = Rect::from_min_max(pos2(full.left(), full.bottom() - STATUS), full.max);
    let area = Rect::from_min_max(pos2(full.left() + GAP, crumb.bottom()), pos2(full.right() - GAP, status.top() - GAP));

    match &app.model.st {
        _ if area.width() < 40.0 || area.height() < 40.0 => {}
        None => {
            let body = panel(&pen, area, tr(app.screen.label_key()), "", false);
            pen.text(pos2(body.left() + 4.0, body.top() + ROW / 2.0), tr("misc.reading"), theme::dim());
        }
        Some(_) if !app.screen.built() => {
            let body = panel(&pen, area, tr(app.screen.label_key()), "", false);
            pen.text(pos2(body.left() + 4.0, body.top() + ROW / 2.0), tr("misc.not_yet"), theme::dim());
        }
        Some(_) if app.fleet => fleet::draw(&mut Cx { ui, pen: pen.clone(), out: &mut out, app }, area),
        Some(_) => {
            let mut rects = [Rect::NOTHING; 3];
            for (p, r) in app.layout().layout(from_egui(area), GAP) {
                rects[p.index()] = to_egui(r);
            }
            {
                let mut cx = Cx { ui, pen: pen.clone(), out: &mut out, app };
                match app.screen {
                    Screen::Ovw => overview::draw(&mut cx, rects),
                    Screen::Cpu => cpu::draw(&mut cx, rects),
                    Screen::Mem => mem::draw(&mut cx, rects),
                    Screen::Gpu => gpu::draw(&mut cx, rects),
                    Screen::Procs => procs::draw(&mut cx, rects),
                    Screen::Svc => svc::draw(&mut cx, rects),
                    Screen::Net => net::draw(&mut cx, rects),
                    Screen::Disk => disk::draw(&mut cx, rects),
                    Screen::Sensors => sensors::draw(&mut cx, rects),
                    Screen::Sys => sys::draw(&mut cx, rects),
                    Screen::Devices => devices::draw(&mut cx, rects),
                    Screen::Containers => containers::draw(&mut cx, rects),
                }
            }
            for (i, r) in rects.into_iter().enumerate() {
                out.pane_rects[i] = (r.width() > 0.0).then_some(r);
            }
            // After the rows, so a divider takes the pointer where both are;
            // the columns' last, over the panels' where they meet.
            if !matches!(app.mode, Mode::Help | Mode::Confirm | Mode::Zoom | Mode::Palette | Mode::Affinity) {
                let look = Look { gap: GAP, grab: 10.0, line: theme::cyan() };
                for (p, node, r) in std::mem::take(&mut out.stacks) {
                    if let Some(m) = dividers(ui, Id::new(("stack", app.screen, p.index())), &node, from_egui(r), look) {
                        out.stack_moved = Some((p, m));
                    }
                }
                out.moved = dividers(ui, Id::new(("div", app.screen)), app.layout(), from_egui(area), look);
            }
        }
    }

    draw_status(ui, &pen, app, status);
    if app.mode == Mode::Palette {
        out.click = None;
        out.tab = None;
        out.fleet_click = None;
        out.fleet_tab = false;
    }
    if app.mode == Mode::Help {
        help::draw(&pen, app, full, &mut out);
        out.click = None;
        out.tab = None;
        out.fleet_click = None;
        out.fleet_tab = false;
    }
    if app.mode == Mode::Confirm {
        confirm::draw(&pen, app, full);
        out.click = None;
        out.tab = None;
        out.fleet_click = None;
        out.fleet_tab = false;
    }
    if app.mode == Mode::Affinity {
        affinity::draw(ui, &pen, app, full, &mut out);
        out.click = None;
        out.tab = None;
        out.zoom = None;
        out.copy = None;
        out.fleet_click = None;
        out.fleet_tab = false;
    }
    if app.mode == Mode::Zoom {
        zoomed::draw(&pen, app, full);
        out.click = None;
        out.tab = None;
        out.zoom = None;
        out.fleet_click = None;
        out.fleet_tab = false;
    }
    out
}

fn draw_header(ui: &Ui, pen: &Pen, app: &App, r: Rect, out: &mut Out) {
    pen.p.rect_filled(r, 0.0, theme::bar());
    let y = r.center().y;
    let mut x = r.left() + 8.0;
    for s in Screen::ALL {
        let label = tr(s.label_key());
        let w = pen.width(label) + pen.cw * 2.0 + 14.0;
        let tab = Rect::from_center_size(pos2(x + w / 2.0, y), vec2(w, 20.0));
        if tab.right() > r.right() - 8.0 {
            break;
        }
        let digit = s.digit().to_string();
        if s == app.screen && !app.fleet {
            pill(pen, tab, &format!("{digit} {label}"), theme::blue(), theme::bg());
        } else {
            let (dc, lc) = if s.built() { (theme::cyan(), theme::fg()) } else { (theme::dim(), theme::dim()) };
            let d = pen.text(pos2(tab.left() + 7.0, y), &digit, dc);
            pen.text(pos2(d.right() + pen.cw, y), label, lc);
        }
        if ui.interact(tab, Id::new(("tab", s)), Sense::click()).clicked() {
            out.tab = Some(s);
        }
        x += w + 2.0;
    }
    // The fleet after the screens, by its letter.
    let label = tr("tab.fleet");
    let key =
        crate::config::keymap::live(&app.keymap().main).find(|b| b.run == [Act::Fleet]).map(|b| render_seq(&b.on)).unwrap_or_default();
    let w = pen.width(label) + pen.width(&key) + pen.cw + 14.0;
    let tab = Rect::from_center_size(pos2(x + 6.0 + w / 2.0, y), vec2(w, 20.0));
    if tab.right() <= r.right() - 8.0 {
        if app.fleet {
            pill(pen, tab, &format!("{key} {label}"), theme::magenta(), theme::bg());
        } else {
            let d = pen.text(pos2(tab.left() + 7.0, y), &key, theme::magenta());
            pen.text(pos2(d.right() + pen.cw, y), label, theme::fg());
        }
        if ui.interact(tab, Id::new("tab-fleet"), Sense::click()).clicked() {
            out.fleet_tab = true;
        }
        x = tab.right() + 2.0;
    }
    if let Some(st) = &app.model.st {
        let up = trf("header.up", &[&fmt_uptime(now_secs().saturating_sub(st.boot_time))]);
        let info = format!("{} · {} · {up}", st.host, st.os);
        let room = r.right() - 10.0 - x - 16.0;
        if room > pen.cw * 8.0 {
            pen.text_r(pos2(r.right() - 10.0, y), &pen.fit(&info, room), theme::dim());
        }
    }
}

fn draw_crumb(pen: &Pen, app: &App, r: Rect) {
    let Some(st) = &app.model.st else { return };
    let m = &app.model;
    let detail = match app.screen {
        Screen::Ovw => format!("{} · {}", st.host, st.arch),
        Screen::Cpu => format!("{} · {}", st.brand, st.topo.summary()),
        Screen::Mem => trf("crumb.mem", &[&fmt_bytes(m.mem.total), &fmt_bytes(m.mem.swap_total)]),
        Screen::Gpu if !m.gpu_read => tr("misc.reading").to_string(),
        Screen::Gpu => match app.selected_gpu() {
            Some(d) => format!("{} · {}", gpu::summary(&m.gpus), gpu::name(d)),
            None => gpu::summary(&m.gpus),
        },
        Screen::Procs => {
            let threads = m.total_threads.map_or_else(|| "—".to_string(), |t| t.to_string());
            trf("crumb.procs", &[&m.procs.len().to_string(), &threads])
        }
        Screen::Svc if !m.svc_read => tr("misc.reading").to_string(),
        Screen::Svc => {
            // The kind on the list (services or drivers), as the left column counts it.
            use crate::collect::svc::State;
            let n = |s: State| app.svc_kind().filter(|v| v.state == s).count().to_string();
            let kind = tr(if app.svc_drivers { "svc.drivers" } else { "svc.services" });
            trf("crumb.svc", &[kind, &app.svc_kind().count().to_string(), &n(State::Running), &n(State::Failed), m.services.source])
        }
        Screen::Net if !m.nic_read => tr("misc.reading").to_string(),
        Screen::Net => {
            let up = m.nics.iter().filter(|n| n.up).count().to_string();
            let socks = if m.conn_read { m.conns.list.len().to_string() } else { "—".to_string() };
            let source = if m.conn_read { m.conns.source } else { "—" };
            trf("crumb.net", &[&m.nics.len().to_string(), &up, &socks, source])
        }
        Screen::Disk if !m.disk_read => tr("misc.reading").to_string(),
        Screen::Disk => {
            let d = &m.disks;
            let (rd, wr) = d.rates();
            let r = |b: f64| fmt_bytes(b as u64);
            trf("crumb.disk", &[&d.drives.len().to_string(), &d.volumes.len().to_string(), &r(rd), &r(wr)])
        }
        Screen::Sensors if !m.sensor_read => tr("misc.reading").to_string(),
        Screen::Sensors => {
            use crate::collect::sensors::Kind;
            let s = &m.sensors;
            let hottest = s.hottest().map_or_else(|| "—".to_string(), |x| x.kind.show(x.value));
            let fans = (s.count(Kind::Fan) + s.count(Kind::Duty)).to_string();
            trf("crumb.sensors", &[&s.list.len().to_string(), &hottest, &fans])
        }
        Screen::Sys => match &m.hw {
            None => tr("misc.reading").to_string(),
            Some(hw) => {
                let mem = match hw.installed().or((m.mem.total > 0).then_some(m.mem.total)) {
                    Some(b) if !hw.unified && hw.slots_total() > 0 => {
                        trf("crumb.sys_slots", &[&fmt_bytes(b), &hw.used().to_string(), &hw.slots_total().to_string()])
                    }
                    Some(b) => fmt_bytes(b),
                    None => "—".to_string(),
                };
                let source = if hw.source.is_empty() { "—" } else { hw.source.as_str() };
                trf("crumb.sys", &[&sys::machine(app), &mem, source])
            }
        },
        Screen::Devices => match &m.devices {
            None => tr("misc.reading").to_string(),
            Some(d) => {
                let problems = d.list.iter().filter(|x| x.status.is_problem()).count().to_string();
                let usb = d.list.iter().filter(|x| matches!(x.bus, crate::collect::devices::Bus::Usb { .. })).count().to_string();
                trf("crumb.devices", &[&d.list.len().to_string(), &app.dev_classes.len().to_string(), &usb, &problems])
            }
        },
        Screen::Containers => match &m.containers {
            None => tr("misc.reading").to_string(),
            Some(c) => {
                let running = c.list.iter().filter(|x| x.state == crate::collect::containers::State::Running).count();
                let runtime = c.runtime.map_or("—", |r| tr(r.key()));
                trf("crumb.containers", &[&c.list.len().to_string(), &running.to_string(), runtime])
            }
        },
    };
    let s = if app.fleet {
        let now = std::time::Instant::now();
        let up = app.peers.iter().filter(|p| matches!(p.status(now), Status::Ok | Status::Slow)).count() + 1;
        let detail = trf("crumb.fleet", &[&(app.peers.len() + 1).to_string(), &up.to_string()]);
        format!("{} › {detail}", tr("tab.fleet"))
    } else {
        format!("{} › {detail}", tr(app.screen.label_key()))
    };
    let mut x = r.left() + 12.0;
    // Another machine's screens start with its name.
    if let Some(p) = app.away_peer().filter(|_| !app.fleet) {
        let room = (r.right() - 12.0 - x) / 2.0;
        x = pen.text(pos2(x, r.center().y), &pen.fit(&p.name, room), theme::cyan()).right();
        x = pen.text(pos2(x, r.center().y), " › ", theme::dim()).right();
    }
    pen.text(pos2(x, r.center().y), &pen.fit(&s, r.right() - 12.0 - x), theme::dim());
}

fn draw_status(ui: &Ui, pen: &Pen, app: &App, r: Rect) {
    pen.p.rect_filled(r, 0.0, theme::bar());
    let y = r.center().y;
    let (badge, color) = match app.mode {
        Mode::Normal if app.fleet => match app.selected_peer() {
            Some(p) if p.status(std::time::Instant::now()) == Status::Down => (tr("mode.stale"), theme::red()),
            _ => (tr("mode.fleet"), theme::magenta()),
        },
        Mode::Normal => (tr("mode.normal"), theme::blue()),
        Mode::Filter => (tr("mode.filter"), theme::yellow()),
        Mode::Help => (tr("mode.help"), theme::magenta()),
        Mode::Palette => (tr("mode.palette"), theme::magenta()),
        Mode::Confirm => (tr("mode.confirm"), theme::red()),
        Mode::Settings => (tr("mode.settings"), theme::cyan()),
        Mode::Zoom => (tr("mode.zoom"), theme::green()),
        Mode::Affinity => (tr("mode.affinity"), theme::cyan()),
    };
    let bw = pen.width(badge) + 16.0;
    let b = Rect::from_center_size(pos2(r.left() + 6.0 + bw / 2.0, y), vec2(bw, 18.0));
    pill(pen, b, badge, color, theme::bg());
    let mut x = b.right() + 10.0;
    // Whose screens these are, red once the connection is lost.
    let away = app.away.as_ref().zip(app.away_peer());
    if let Some((a, p)) = away {
        let (s, c) = if a.lost { (trf("status.lost", &[&p.name]), theme::red()) } else { (p.name.clone(), theme::cyan()) };
        let s = pen.fit(&s, (r.width() / 4.0).max(pen.cw * 6.0));
        let w = pen.width(&s) + 16.0;
        let pr = Rect::from_center_size(pos2(x + w / 2.0, y), vec2(w, 18.0));
        pill(pen, pr, &s, c, theme::bg());
        x = pr.right() + 10.0;
    }

    // The right side first, so the left knows its room.
    let m = &app.model;
    let mut right = trf("status.summary", &[&format!("{:.0}", m.total), &format!("{:.0}", m.mem_pct())]);
    if app.fleet {
        right += &format!("  {}/{}", app.fleet_sel + 1, app.peers.len() + 1);
    } else if app.screen.built() {
        let p = app.focus();
        let n = app.count(app.screen, p);
        if n > 0 {
            right += &format!("  {}/{n}", app.view().sel[p.index()] + 1);
        }
    }
    // The clock at the right end (common.toml's [clock]), as tsumugi's.
    let mut end = r.right() - 10.0;
    let clock = app.prefs.common.clock_or(None);
    if clock.show {
        let now = chrono::Local::now().naive_local();
        let lang = crate::i18n::current_language();
        let cr = pen.text_r(pos2(end, y), &clock.text(&now, lang), theme::strong());
        let full = ito_common::Clock::full(&now, lang);
        ui.interact(cr, Id::new("status-clock"), Sense::hover()).on_hover_text(full);
        ui.ctx().request_repaint_after(ito_common::Clock::next_minute(&now));
        end = cr.left() - 16.0;
    }
    let rr = pen.text_r(pos2(end, y), &right, theme::dim());
    let room = rr.left() - 16.0 - x;

    if app.mode == Mode::Filter {
        pen.text(pos2(x, y), &pen.fit(&format!("/{}▏", app.filter()), room), theme::fg());
    } else if let Some((t, left)) = app.shown_toast() {
        pen.text(pos2(x, y), &pen.fit(t, room), theme::yellow());
        ui.ctx().request_repaint_after(left);
    } else {
        // Another machine samples at its own pace.
        let (interval, paused) = match away {
            Some((_, p)) => (p.interval().as_secs_f32(), false),
            None => (app.interval, app.paused),
        };
        let state = tr(if paused { "status.paused" } else { "status.sampling" });
        let mut s = format!("{} · {state}", trf("status.interval", &[&fmt_interval(interval)]));
        if !app.filter().is_empty() {
            s += &format!(" · /{}", app.filter());
        }
        pen.text(pos2(x, y), &pen.fit(&s, room), if paused { theme::yellow() } else { theme::dim() });
    }
}
