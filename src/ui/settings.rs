//! The settings screen (Ctrl+,), laid out as tsumugi's: `ito-prefs`
//! draws the nav and the cards, and the rows common to every uchmk app (the
//! language, the clock, the theme) write common.toml. yagura's own rows
//! write config.toml; what a row asks for is gathered while the page is
//! drawn and done once it is.

use egui::{Align2, FontId, Rect, Sense, Ui, pos2, vec2};
use ito_common::{Clock, CommonChange, themes_dir};
use ito_prefs::{Words, button, field, row, section, select, sep, status, switch};

use crate::app::{App, INTERVALS, fmt_interval};
use crate::config::edit::Edit;
use crate::config::settings::{ALLOW, BACKENDS, Host};
use crate::config::theme::{self, ramp};
use crate::i18n::{current_language, tr, trf};
use crate::remote::{self, proto};

const GENERAL: usize = 0;
const THEME: usize = 1;
const FLEET: usize = 2;
const KEYS: usize = 3;
const NOTIFY: usize = 4;
const ADVANCED: usize = 5;
const ABOUT: usize = 6;

/// The keymap's layers, and each one's name on the Keys page.
const LAYERS: [(&str, &str); 6] = [
    ("main", "settings.layer_main"),
    ("input", "settings.layer_input"),
    ("help", "settings.layer_help"),
    ("confirm", "settings.layer_confirm"),
    ("zoom", "settings.layer_zoom"),
    ("settings", "settings.layer_settings"),
];

/// What the page asked for.
#[derive(Default)]
struct Asked {
    common: Vec<CommonChange>,
    edits: Vec<Edit>,
    interval: Option<f32>,
    copy: Option<String>,
    /// The bug-report button was pressed.
    bug: bool,
    /// "Forget the remembered place" was pressed.
    forget: bool,
    /// The Startup switch was clicked: the state it now asks for.
    startup: Option<bool>,
    advanced: Vec<ito_prefs::Advanced>,
    backup: Vec<ito_prefs::Backup>,
    /// The Startup card was shown: ask the OS once.
    ask_startup: bool,
    /// The fleet page was shown: read this machine's key.
    key: bool,
    /// A "Send a test" of the notifications page was pressed.
    test: Option<ito_prefs::Test>,
    toast: Option<String>,
}

/// The rows on each page, for the search.
fn index() -> Vec<(usize, &'static str)> {
    let w = Words::of(current_language());
    let mut v = vec![
        (GENERAL, w.language),
        (GENERAL, w.scale),
        (GENERAL, tr("settings.interval")),
        (GENERAL, tr("settings.updates")),
        (GENERAL, tr("settings.check_updates")),
        (GENERAL, w.clock),
        (GENERAL, w.show_time),
        (GENERAL, w.time_format),
        (GENERAL, w.show_date),
        (GENERAL, w.date_format),
        (GENERAL, w.weekday),
        (THEME, w.mode),
        (THEME, w.themes),
        (THEME, w.preview),
    ];
    for k in [
        "settings.agent",
        "settings.allow_kill",
        "settings.allow_service",
        "settings.port",
        "settings.hosts",
        "settings.add_host",
        "settings.setup",
        "settings.copy_agent",
        "settings.copy_ssh",
        "settings.copy_tailscale",
        "settings.copy_host",
        "settings.copy_config",
        "settings.direct",
        "settings.key",
        "settings.copy_viewer",
        "settings.copy_listen",
        "settings.copy_keyed_host",
    ] {
        v.push((FLEET, tr(k)));
    }
    v.extend(ito_prefs::keys_index(ito_prefs::KeyWords::of(current_language())).into_iter().map(|t| (KEYS, t)));
    v.extend(LAYERS.iter().map(|(_, k)| (KEYS, tr(k))));
    v.extend(ito_prefs::notify_index(w).into_iter().map(|t| (NOTIFY, t)));
    let m = ito_prefs::MoreWords::of(current_language());
    v.extend(ito_prefs::window_index(m).into_iter().map(|t| (GENERAL, t)));
    v.extend(ito_prefs::WINDOW_KEYS.into_iter().map(|t| (GENERAL, t)));
    v.extend(ito_prefs::startup_index(m).into_iter().map(|t| (GENERAL, t)));
    v.extend(ito_prefs::advanced_index(m).into_iter().map(|t| (ADVANCED, t)));
    v.extend(ito_prefs::backup_index(m).into_iter().map(|t| (ADVANCED, t)));
    v.extend(ito_prefs::about_index(m, w).into_iter().map(|t| (ABOUT, t)));
    for k in ["settings.backend", "settings.graphics", "settings.warnings"] {
        v.push((ADVANCED, tr(k)));
    }
    // The names in the files, so a search finds them in any language.
    v.extend(CONFIG_NAMES);
    v
}

/// Each page's keys in config.toml and common.toml.
const CONFIG_NAMES: [(usize, &str); 27] = [
    (GENERAL, "language"),
    (GENERAL, "scale"),
    (GENERAL, "interval"),
    (GENERAL, "[clock]"),
    (THEME, "theme"),
    (THEME, "dark_theme"),
    (THEME, "light_theme"),
    (FLEET, "[agent]"),
    (FLEET, "allow"),
    (FLEET, "port"),
    (FLEET, "listen"),
    (FLEET, "[[agent.viewer]]"),
    (FLEET, "[[host]]"),
    (FLEET, "addr"),
    (FLEET, "key"),
    (FLEET, "every"),
    (KEYS, "keymap.toml"),
    (KEYS, "prepend_keymap"),
    (NOTIFY, "[notify]"),
    (NOTIFY, "system"),
    (NOTIFY, "sound_name"),
    (NOTIFY, "focus_mode"),
    (NOTIFY, "[[notify.webhooks]]"),
    (NOTIFY, "url"),
    (NOTIFY, "format"),
    (NOTIFY, "states"),
    (ADVANCED, "backend"),
];

pub fn show(ui: &mut Ui, app: &mut App, keys: ito_prefs::Keys) {
    let lang = current_language();
    let w = Words::of(lang);
    let mw = ito_prefs::MoreWords::of(lang);
    let startup_now = app.prefs.startup;
    let state_dir = app.prefs.state_dir.is_some();
    let backup_file = ito_common::home_dir().unwrap_or_default().join("yagura-settings.toml");
    let pages = [
        (tr("settings.general"), tr("settings.general_lead")),
        (tr("settings.theme"), tr("settings.theme_lead")),
        (tr("settings.fleet"), tr("settings.fleet_lead")),
        (tr("settings.keys"), tr("settings.keys_lead")),
        (w.notify, tr("settings.notify_lead")),
        (tr("settings.advanced"), tr("settings.advanced_lead")),
        ito_prefs::Page::About.words(mw),
    ];
    let index = index();
    let nav = ito_prefs::Nav { pages: &pages, index: &index, words: w, file: "config.toml" };
    let mut asked = Asked::default();
    let interval = app.interval;
    let warnings = app.prefs.warnings();
    let layers = app.keymap().layers();
    let kw = ito_prefs::KeyWords::of(lang);
    let mut key_edits = Vec::new();
    let prefs = &mut app.prefs;
    let (settings, common, themes, base) = (&prefs.settings, &prefs.common, &prefs.themes, prefs.base.as_deref());
    let drafts = &mut prefs.drafts;
    let keys_edit = &mut prefs.keys_edit;
    let key = &prefs.key;
    let config = base.map(crate::config::edit::config_path);
    let out = ito_prefs::show(ui, ito_theme::colors(), &mut prefs.state, &nav, keys, |ui, page, l| match page {
        GENERAL => {
            section(ui, l, tr("settings.display"), |ui| {
                let own = settings.app_language.as_deref().filter(|l| *l != "auto");
                let note = match own {
                    Some(code) => trf("settings.language_own", &[code]),
                    None => tr("settings.language_note").into(),
                };
                let now = common.language.as_deref().unwrap_or("auto");
                asked.common.extend(ito_prefs::language_row(ui, l, w, now, &note));
                sep(ui, l);
                asked.common.extend(ito_prefs::scale_row(ui, l, w, common.scale_or_one(), tr("settings.scale_note")));
                sep(ui, l);
                let secs = |s: f32| trf("settings.seconds", &[&fmt_interval(s)]);
                let mut options: Vec<(f32, String)> = INTERVALS.iter().map(|&s| (s, secs(s))).collect();
                if !INTERVALS.contains(&interval) {
                    options.push((interval, secs(interval)));
                }
                let options: Vec<(u32, &str)> = options.iter().map(|(s, t)| (s.to_bits(), t.as_str())).collect();
                let picked = row(ui, l, tr("settings.interval"), tr("settings.interval_note"), |ui| {
                    select(ui, "interval", interval.to_bits(), &options)
                });
                if let Some(bits) = picked {
                    asked.interval = Some(f32::from_bits(bits));
                }
            });
            section(ui, l, tr("settings.updates"), |ui| {
                let on = settings.check_updates;
                if row(ui, l, tr("settings.check_updates"), tr("settings.check_updates_note"), |ui| switch(ui, l, on)) {
                    asked.edits.push(Edit::CheckUpdates(!on));
                }
            });
            asked.common.extend(ito_prefs::clock_card(ui, l, w, &common.clock_or(None)));
            let out = ito_prefs::window_card(ui, l, mw, &common.window_or_default());
            asked.common.extend(out.changes);
            asked.forget = out.forget;
            asked.ask_startup = true;
            asked.startup = ito_prefs::startup_card(ui, l, mw, startup_now);
        }
        THEME => {
            let (t, d, li) = common.theme_choice(None);
            let note = match base {
                Some(b) => trf("settings.theme_note", &[&themes_dir(b).display().to_string()]),
                None => tr("settings.no_folder").into(),
            };
            asked.common.extend(ito_prefs::theme_page(ui, l, w, themes, (&t, &d, &li), &note, preview));
        }
        FLEET => {
            section(ui, l, tr("settings.agent"), |ui| {
                for (k, (key, note)) in ALLOW
                    .iter()
                    .zip([("settings.allow_kill", "settings.allow_kill_note"), ("settings.allow_service", "settings.allow_service_note")])
                    .enumerate()
                {
                    if k > 0 {
                        sep(ui, l);
                    }
                    let on = settings.allow.iter().any(|a| a == key);
                    if row(ui, l, tr(note.0), tr(note.1), |ui| switch(ui, l, on)) {
                        let allow = ALLOW
                            .iter()
                            .filter(|a| if **a == *key { !on } else { settings.allow.iter().any(|b| b == *a) })
                            .map(|a| a.to_string())
                            .collect();
                        asked.edits.push(Edit::Allow(allow));
                    }
                }
                sep(ui, l);
                let port = settings.port.to_string();
                let typed = row(ui, l, tr("settings.port"), tr("settings.port_note"), |ui| {
                    field(ui, drafts, "port", &port, &proto::PORT.to_string(), 90.0)
                });
                if let Some(t) = typed {
                    match t.trim().parse::<u16>() {
                        Ok(p) if p > 0 => asked.edits.push(Edit::Port(p)),
                        _ => asked.toast = Some(tr("settings.port_bad").into()),
                    }
                }
            });
            section(ui, l, tr("settings.hosts"), |ui| {
                let hosts = &settings.hosts;
                for (i, h) in hosts.iter().enumerate() {
                    let label = trf("settings.host_n", &[&(i + 1).to_string()]);
                    let (mut gone, mut name, mut addr) = (false, None, None);
                    row(ui, l, &label, "", |ui| {
                        gone = button(ui, l, tr("settings.delete"));
                        addr = field(ui, drafts, &format!("host-addr-{i}"), &h.addr, "host:port", 220.0);
                        name = field(ui, drafts, &format!("host-name-{i}"), &h.name, "name", 120.0);
                    });
                    let mut list = hosts.clone();
                    if gone {
                        list.remove(i);
                    } else if let Some(n) = name {
                        list[i].name = if n.trim().is_empty() { list[i].addr.clone() } else { n.trim().to_string() };
                    } else if let Some(a) = addr {
                        if a.trim().is_empty() {
                            asked.toast = Some(tr("settings.addr_empty").into());
                            continue;
                        }
                        list[i].addr = a.trim().to_string();
                    } else {
                        sep(ui, l);
                        continue;
                    }
                    asked.edits.push(Edit::Hosts(list));
                    sep(ui, l);
                }
                if row(ui, l, tr("settings.add_host"), tr("settings.add_host_note"), |ui| button(ui, l, tr("settings.add"))) {
                    let mut list = hosts.clone();
                    let n = list.len() + 1;
                    list.push(Host { name: format!("host{n}"), addr: format!("localhost:{}", settings.port), ..Host::default() });
                    asked.edits.push(Edit::Hosts(list));
                }
            });
            section(ui, l, tr("settings.setup"), |ui| {
                let mut lines = vec![
                    ("settings.copy_agent", remote::AGENT_LINE.to_string()),
                    ("settings.copy_ssh", remote::ssh_line(settings.port)),
                    ("settings.copy_tailscale", remote::tailscale_line(settings.port)),
                    ("settings.copy_host", remote::host_sample(settings.port)),
                ];
                if let Some(c) = &config {
                    lines.push(("settings.copy_config", c.display().to_string()));
                }
                for (k, (key, text)) in lines.into_iter().enumerate() {
                    if k > 0 {
                        sep(ui, l);
                    }
                    // The note is the line itself, on one line.
                    let shown = text.trim_end().replace('\n', "  ");
                    if row(ui, l, tr(key), &shown, |ui| button(ui, l, tr("settings.copy"))) {
                        asked.copy = Some(text);
                    }
                }
            });
            asked.key = true;
            section(ui, l, tr("settings.direct"), |ui| match key {
                Some(Ok((public, host))) => {
                    let lines = [
                        ("settings.key", public.clone()),
                        ("settings.copy_viewer", remote::noise::viewer_snippet(host, public)),
                        ("settings.copy_listen", remote::noise::listen_snippet()),
                        ("settings.copy_keyed_host", remote::noise::host_snippet(host, public)),
                    ];
                    for (k, (key, text)) in lines.into_iter().enumerate() {
                        if k > 0 {
                            sep(ui, l);
                        }
                        let shown = text.trim_end().replace('\n', "  ");
                        if row(ui, l, tr(key), &shown, |ui| button(ui, l, tr("settings.copy"))) {
                            asked.copy = Some(text);
                        }
                    }
                }
                Some(Err(e)) => {
                    row(ui, l, tr("settings.key"), "", |_| ());
                    status(ui, e, l.c.err);
                }
                None => {
                    row(ui, l, tr("settings.key"), tr("settings.key_reading"), |_| ());
                }
            });
        }
        KEYS => {
            let layer_name = |n: &str| LAYERS.iter().find(|(id, _)| *id == n).map_or_else(|| n.to_owned(), |(_, k)| tr(k).to_owned());
            let tr = |d: &str| crate::i18n::tr_desc(d).to_owned();
            let table = ito_prefs::KeyTable {
                now: &layers,
                built_in: crate::config::keymap::Keymap::built_in_layers(),
                noop: "noop",
                own: "keymap.toml",
                layer_name: &layer_name,
                tr: &tr,
                extra: &|_, _| Vec::new(),
                unbound: &[],
                sup: false,
            };
            key_edits.extend(ito_prefs::keys_page(ui, l, kw, &table, keys_edit).unwrap_or_default());
        }
        NOTIFY => {
            let was = &settings.notify;
            let mut n = was.page();
            let ends = [ito_prefs::End { name: tr("settings.notify_alert"), color: l.c.err, sound: tr("settings.notify_alert_sound") }];
            asked.test = ito_prefs::notify_page(ui, l, w, &ends, &mut n, &crate::alert::Notify::default().page(), drafts);
            let keys = was.edits(&n);
            if !keys.is_empty() {
                asked.edits.push(Edit::Notify(keys));
            }
            if let Some(places) = was.webhooks_of(&n) {
                asked.edits.push(Edit::Webhooks(places));
            }
        }
        ADVANCED => {
            section(ui, l, tr("settings.graphics"), |ui| {
                let now = settings.backend.as_deref().unwrap_or("auto");
                let options: Vec<(&str, &str)> = BACKENDS.iter().map(|b| (*b, *b)).collect();
                if let Some(b) = row(ui, l, tr("settings.backend"), tr("settings.backend_note"), |ui| select(ui, "backend", now, &options))
                {
                    asked.edits.push(Edit::Backend(b.to_string()));
                }
            });
            asked.advanced = ito_prefs::advanced_card(ui, l, mw, state_dir);
            asked.backup = ito_prefs::backup_card(ui, l, mw, &backup_file);
            section(ui, l, tr("settings.warnings"), |ui| {
                if warnings.is_empty() {
                    row(ui, l, tr("settings.no_warnings"), "", |_| ());
                }
                for (k, warn) in warnings.iter().enumerate() {
                    if k > 0 {
                        sep(ui, l);
                    }
                    ui.add_space(8.0);
                    status(ui, warn, l.c.err);
                    ui.add_space(8.0);
                }
            });
        }
        ABOUT => {
            let os = os_line();
            let about =
                ito_prefs::About { version: env!("CARGO_PKG_VERSION"), commit: None, system: os, license: Some("MIT OR Apache-2.0") };
            if ito_prefs::about_page(ui, l, mw, w, "yagura", &about).contains(&ito_prefs::AboutAction::Report) {
                asked.bug = true;
            }
        }
        _ => {}
    });

    // Keys taken on the Keys page stop with it: left to another page by a
    // click, they would keep Esc and Ctrl+Tab from the screen.
    if app.prefs.state.page != KEYS {
        app.prefs.keys_edit.capture = None;
        app.prefs.keys_edit.listening = false;
    }
    app.prefs.keymap_edit(key_edits);
    if out.close {
        app.close_settings();
    }
    if out.open_file {
        app.prefs.open(true);
    }
    for a in asked.advanced {
        match a {
            ito_prefs::Advanced::OpenConfigDir => app.prefs.open(false),
            ito_prefs::Advanced::OpenStateDir => app.prefs.open_state(),
            ito_prefs::Advanced::Reset => app.prefs.reset(),
        }
    }
    for b in asked.backup {
        app.prefs.backup(b);
    }
    if asked.forget {
        app.prefs.forget_place();
        app.toast(tr("toast.place_forgotten"));
    }
    if asked.ask_startup {
        app.prefs.ask_startup();
    }
    if let Some(on) = asked.startup {
        app.prefs.set_startup(on);
    }
    if asked.bug {
        app.bug_report();
    }
    if asked.key {
        app.prefs.ask_key();
    }
    app.prefs.common_change(asked.common);
    for e in asked.edits {
        app.prefs.config_edit(e);
    }
    if let Some(s) = asked.interval {
        // At once, and kept in config.toml for the next start.
        app.set_interval(s);
        app.prefs.config_edit(Edit::Interval(s));
    }
    let (title, body) = (tr("settings.notify_test_title"), tr("settings.notify_test_body"));
    match asked.test {
        Some(ito_prefs::Test::All) => crate::alert::tell(&app.prefs.settings.notify, title, body),
        Some(ito_prefs::Test::Hook(k)) => {
            if let Some(p) = app.prefs.settings.notify.webhooks.get(k) {
                crate::alert::tell_one(p, title, body);
            }
        }
        None => {}
    }
    if let Some(t) = asked.toast {
        app.toast(t);
    }
    if let Some(text) = asked.copy {
        app.copy(text);
    }
    draw_toast(ui, app);
}

/// The OS line of the bug report, read once (it can touch the registry).
fn os_line() -> &'static str {
    static OS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    OS.get_or_init(ito_bugreport::os_line)
}

/// The toast over the foot of the page: there is no status bar here.
fn draw_toast(ui: &Ui, app: &App) {
    let Some((t, left)) = app.shown_toast() else { return };
    let c = ito_theme::colors();
    let font = FontId::proportional(12.5);
    let area = ui.available_rect_before_wrap();
    let galley = ui.painter().layout_no_wrap(t.to_string(), font, c.strong());
    let size = galley.size() + vec2(28.0, 14.0);
    let r = Rect::from_center_size(pos2(area.center().x + 120.0, area.bottom() - 20.0 - size.y / 2.0), size);
    let p = ui.painter();
    p.rect_filled(r, 8.0, c.panel);
    p.rect_stroke(r, 8.0, egui::Stroke::new(1.0, c.wait), egui::StrokeKind::Inside);
    p.galley(r.center() - galley.size() / 2.0, galley, c.strong());
    ui.ctx().request_repaint_after(left);
}

/// yagura in the theme in force: a row of core tiles, a few processes
/// with one picked, and the status bar with its clock.
fn preview(ui: &mut Ui) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width().min(420.0), 236.0), Sense::hover());
    let p = ui.painter_at(r);
    let mono = FontId::monospace(12.0);
    p.rect_filled(r, 8.0, theme::bg());
    p.rect_stroke(r, 8.0, egui::Stroke::new(1.0, theme::line()), egui::StrokeKind::Inside);
    let inner = r.shrink(12.0);
    p.text(inner.left_top(), Align2::LEFT_TOP, tr("tab.cpu"), mono.clone(), theme::cyan());
    // The tiles: one load each, from cool to hot.
    let loads = [6.0, 18.0, 34.0, 52.0, 71.0, 88.0, 97.0, 45.0];
    let tw = (inner.width() - 7.0 * 6.0) / 8.0;
    for (k, pct) in loads.iter().enumerate() {
        let t = Rect::from_min_size(pos2(inner.left() + k as f32 * (tw + 6.0), inner.top() + 22.0), vec2(tw, 40.0));
        p.rect_filled(t, 4.0, theme::bg_alt());
        let fill = Rect::from_min_max(pos2(t.left(), t.bottom() - t.height() * pct / 100.0), t.max);
        p.rect_filled(fill, 4.0, ramp(*pct).gamma_multiply(0.55));
        p.text(t.center(), Align2::CENTER_CENTER, format!("{pct:.0}"), mono.clone(), theme::fg());
    }
    // The processes, the second one picked.
    let rows = [
        ("yagura", "0.8", theme::fg()),
        ("tsumugi", "3.1", theme::fg()),
        ("cargo", "54.0", theme::yellow()),
        ("rustc", "98.2", theme::red()),
    ];
    for (k, (name, cpu, color)) in rows.iter().enumerate() {
        let y = inner.top() + 76.0 + k as f32 * 23.0;
        let line = Rect::from_min_size(pos2(inner.left(), y), vec2(inner.width(), 23.0));
        let fg = if k == 1 {
            p.rect_filled(line, 3.0, theme::sel());
            theme::strong()
        } else {
            *color
        };
        p.text(pos2(line.left() + 6.0, line.center().y), Align2::LEFT_CENTER, *name, mono.clone(), fg);
        p.text(
            pos2(line.right() - 6.0, line.center().y),
            Align2::RIGHT_CENTER,
            format!("{cpu}%"),
            mono.clone(),
            if k == 1 { fg } else { theme::dim() },
        );
    }
    // The status bar, with the badge and the clock.
    let bar = Rect::from_min_max(pos2(r.left() + 1.0, r.bottom() - 26.0), pos2(r.right() - 1.0, r.bottom() - 1.0));
    p.rect_filled(bar, egui::CornerRadius { nw: 0, ne: 0, sw: 7, se: 7 }, theme::bar());
    let badge = Rect::from_min_size(pos2(bar.left() + 8.0, bar.center().y - 9.0), vec2(64.0, 18.0));
    p.rect_filled(badge, 9.0, theme::blue());
    p.text(badge.center(), Align2::CENTER_CENTER, tr("mode.normal"), mono.clone(), theme::bg());
    let now = chrono::Local::now().naive_local();
    let clock = Clock::default().text(&now, current_language());
    p.text(pos2(bar.right() - 10.0, bar.center().y), Align2::RIGHT_CENTER, clock, mono, theme::strong());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_in_the_files_finds_its_page_in_any_language() {
        let pages = |q: &str| -> Vec<usize> {
            let mut v: Vec<usize> = CONFIG_NAMES.iter().filter(|(_, w)| w.contains(q)).map(|(p, _)| *p).collect();
            v.dedup();
            v
        };
        for (q, page) in [
            ("language", GENERAL),
            ("scale", GENERAL),
            ("interval", GENERAL),
            ("clock", GENERAL),
            ("theme", THEME),
            ("allow", FLEET),
            ("port", FLEET),
            ("listen", FLEET),
            ("viewer", FLEET),
            ("host", FLEET),
            ("addr", FLEET),
            ("keymap", KEYS),
            ("prepend", KEYS),
            ("every", FLEET),
            ("focus_mode", NOTIFY),
            ("webhook", NOTIFY),
            ("webhooks", NOTIFY),
            ("format", NOTIFY),
            ("backend", ADVANCED),
        ] {
            assert_eq!(pages(q), [page], "{q}");
        }
        // "key" is both a [[host]] field and part of keymap.toml.
        assert_eq!(pages("key"), [FLEET, KEYS]);
    }

    /// The Keys page takes the keys pressed while a key is being set (Esc
    /// and Enter included, which would close the window), and writes only the
    /// key set to yagura's keymap.toml once Enter sets it.
    #[test]
    fn the_keys_page_takes_a_key_and_writes_it_on_enter() {
        use crate::screen::Screen;
        let dir = crate::util::test_dir("settings-keys");
        let (km, _) = crate::config::keymap::Keymap::load(None);
        let (_tx, rx) = crossbeam_channel::unbounded();
        let (ctl_tx, _ctl_rx) = crossbeam_channel::unbounded();
        let layouts = Screen::ALL.iter().map(|&s| (s, s.default_layout())).collect();
        let mut a = App::new(km, rx, ctl_tx, layouts, crate::persist::Saver::new(None), 1.0);
        a.prefs.base = Some(dir.clone());
        a.prefs.state.page = KEYS;
        let ctx = egui::Context::default();
        let mut time = 0.0;
        let mut frame = |a: &mut App, events: Vec<egui::Event>| {
            time += 0.1;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1200.0, 800.0))),
                time: Some(time),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| show(ui, a, ito_prefs::Keys::default()));
            });
            out.textures_delta.clear();
        };
        let key = |key, modifiers, pressed| egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers };
        let mut press = |a: &mut App, k, m| {
            frame(a, vec![key(k, m, true)]);
            frame(a, vec![key(k, m, false)]);
        };
        let start = |a: &mut App| {
            a.prefs.keys_edit.capture =
                Some(ito_prefs::Capture { layer: "main".into(), run: "quit".into(), keys: Vec::new(), replace: None });
        };
        let file = crate::prefs::keymap_path(&dir);
        // Esc gives up the key.
        start(&mut a);
        press(&mut a, egui::Key::Escape, egui::Modifiers::NONE);
        assert!(a.prefs.keys_edit.capture.is_none(), "Esc gave the key up");
        // Ctrl+Shift+Enter is a key; nothing is written before Enter.
        start(&mut a);
        let cs = egui::Modifiers { ctrl: true, shift: true, command: true, ..Default::default() };
        press(&mut a, egui::Key::Enter, cs);
        assert!(!file.exists(), "a key pressed is not written yet");
        assert_eq!(a.prefs.keys_edit.capture.as_ref().map(|c| c.keys.len()), Some(1), "{:?}", a.prefs.keys_edit.capture);
        press(&mut a, egui::Key::Enter, egui::Modifiers::NONE);
        let mut text = String::new();
        for _ in 0..500 {
            text = std::fs::read_to_string(&file).unwrap_or_default();
            if text.contains("quit") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(text.contains("quit"), "keymap.toml was not written within 5 s");
        assert!(text.contains("<C-S-Enter>") && text.contains(r#"run = "quit""#) && text.contains("prepend_keymap"), "{text}");
        assert!(!text.contains("noop") && !text.contains("overview"), "only the key set is written: {text}");
        // Taking keys, then gone to another page: the keys stop being taken.
        start(&mut a);
        a.prefs.keys_edit.listening = true;
        a.prefs.state.page = NOTIFY;
        frame(&mut a, Vec::new());
        assert!(!a.prefs.keys_edit.busy(), "another page takes no keys for the Keys page");
        // Taking keys, then the settings closed and opened again: no key is taken.
        a.prefs.state.page = KEYS;
        start(&mut a);
        a.close_settings();
        a.open_settings();
        assert!(!a.prefs.keys_edit.busy(), "the settings opened again take no keys for the Keys page");
    }
}
