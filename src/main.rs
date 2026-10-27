//! yagura: a keyboard-driven system monitor, the sibling of kura and
//! tsumugi. The window and the input; the state is in `app.rs`, the
//! drawing in `ui/`, the sampling in `collect/`.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod affinity;
mod alert;
mod app;
mod bugreport;
use yagura_core as collect;

/// The sampler's wake-up call for a window: repaint it.
fn wake_of(ctx: Option<&egui::Context>) -> collect::Wake {
    ctx.cloned().map(|c| std::sync::Arc::new(move || c.request_repaint()) as std::sync::Arc<dyn Fn() + Send + Sync>)
}
mod config;
mod envreport;
mod export;
mod fonts;
mod i18n;
mod icon;
mod kill;
mod metrics;
mod model;
mod persist;
mod prefs;
mod procinfo;
mod remote;
mod renice;
mod screen;
mod service;
mod stacks;
mod svcctl;
mod tree;
mod ui;
mod util;
mod zoom;

use std::path::PathBuf;
use std::time::Duration;

use app::{App, Mode};
use config::cmd::Act;
use config::keys::{Key, from_egui};
use config::{keymap::Keymap, settings, theme};
use ito_common::window::{Saved, Seen, Tracker};
use ito_keyscript::{Do, Runner};

/// The window's icon, in pixels square.
const ICON_PX: u32 = 128;

const USAGE: &str = "\
yagura — a keyboard-driven system monitor

usage: yagura [--keys SCRIPT]
       yagura --agent [--log FILE]
       yagura --snapshot
       yagura --log-metrics FILE [--every SECONDS]
       yagura --install-service | --uninstall-service
       yagura --key
       yagura env [--out FILE]

  --keys SCRIPT  press these keys once the window is up, as kura's --keys
                 (`<Wait:ms>`, `<Shot:name>`, `<State:name>`, `<Click:x,y>`,
                 `<Drag:x,y-x,y>`, `<Quit>`); YAGURA_KEYS_DONE names the
                 file the final state is written to
  --agent        no window: serve this machine's summary to the fleet screen
                 of other machines, on 127.0.0.1:7313 only (`[agent] port`);
                 reach it through `ssh -L 7313:localhost:7313 HOST` or
                 `tailscale serve --bg --tcp 7313 tcp://localhost:7313`.
                 View only unless `[agent] allow` says otherwise. With
                 `[agent] listen = \"0.0.0.0:7314\"` it also takes viewers
                 whose keys `[[agent.viewer]]` lists, directly, over Noise;
                 with `[agent] metrics = \"127.0.0.1:9314\"` it also serves
                 `/metrics` for Prometheus (plain HTTP)
  --log FILE     with --agent: append what the agent says to FILE as well
                 (Windows has no console to say it to when started at logon)
  --snapshot     no window: read everything once and print it as JSON (the
                 system, CPU, memory, processes, GPUs, services, network,
                 disks, sensors, battery, hardware and devices)
  --log-metrics FILE
                 no window: append a CSV row of the main numbers (CPU,
                 load, memory, network, disk, GPU, hottest temperature,
                 battery, process count) to FILE every --every seconds
                 (default 5) until stopped with Ctrl+C
  --every SECONDS
                 the gap between rows of --log-metrics
  --install-service
                 start the agent at logon from now on, and now: a systemd
                 user unit on Linux, a LaunchAgent on macOS, the user's Run
                 key on Windows. Needs no administrator
  --uninstall-service
                 stop that agent and remove what --install-service added
  --key          print this machine's public key (made the first time) and
                 what to add to the other machine's config.toml for a direct
                 connection
  env            print what a bug report needs: the version, the OS, which
                 config files were read, the settings, which helper tools
                 are on the PATH and which variables are set. --out FILE
                 writes it there as UTF-8 instead
  --version      print the version
  --help         print this

Settings: Ctrl+, (Cmd+, on macOS) opens the settings screen. The files are
<config dir>/uchmk/yagura/config.toml ([[host]] lists the fleet) and keymap.toml;
the language, the scale, the theme and the clock are shared by every uchmk app in
<config dir>/uchmk/common.toml, and themes of your own go in <config dir>/uchmk/themes/.

Environment: UCHMK_CONFIG_DIR moves the whole uchmk folder, YAGURA_CONFIG_HOME
yagura's own folder and YAGURA_STATE_HOME the folder of state.toml (the column
widths).";

struct Yagura {
    app: App,
    script: Option<Runner>,
    /// What the script asked for in `raw_input_hook`, done in the frame.
    todo: Vec<Do>,
    /// The one question to GitHub about a newer release, until it answers.
    update: Option<std::sync::mpsc::Receiver<ito_common::update::Update>>,
    /// Writes the window's place to `window.toml` once it has stayed put, and
    /// when the window closes (#58); `None` folder (or `--keys`) remembers
    /// nothing.
    place: Option<Tracker>,
    /// The "always on top" the window was last told (common.toml's `[window]`).
    on_top: bool,
}

impl Yagura {
    fn write_state(&self, path: PathBuf, tail: &str) {
        let text = format!("{}{tail}", self.app.report());
        if let Err(e) = std::fs::write(&path, text) {
            err(&format!("yagura --keys: {}: {e}\n", path.display()));
        }
    }

    /// The settings screen's keys, from its layer of the keymap: those
    /// taken out of egui's input, the rest left to the controls.
    fn settings_keys(&mut self, ctx: &egui::Context) -> ito_prefs::Keys {
        let mut keys = ito_prefs::Keys::default();
        // The Keys page is taking keys: every one is its, Esc included.
        if self.app.prefs.keys_edit.busy() {
            return keys;
        }
        // A control had the keys: Esc is its (egui lets go of the control).
        let busy = self.app.prefs.state.held || ctx.memory(|m| m.focused().is_some());
        let events = ctx.input(|i| i.events.clone());
        let mut used = Vec::new();
        for (n, e) in events.iter().enumerate() {
            let egui::Event::Key { key, pressed: true, modifiers, .. } = e else { continue };
            let Some(k) = from_egui(*key, modifiers) else { continue };
            match self.app.settings_act(k) {
                Some(Act::Escape) if busy => continue,
                Some(Act::Escape | Act::Settings) => keys.close = true,
                Some(Act::Page(d)) if d < 0 => keys.back = true,
                Some(Act::Page(_)) => keys.next = true,
                Some(Act::Search) => keys.find = true,
                Some(Act::Scale(to)) => self.app.scale(to),
                _ => continue,
            }
            used.push(n);
        }
        if !used.is_empty() {
            ctx.input_mut(|i| {
                let mut n = 0;
                i.events.retain(|_| {
                    n += 1;
                    !used.contains(&(n - 1))
                });
            });
        }
        keys
    }

    fn keys(&mut self, ctx: &egui::Context) {
        let events = ctx.input(|i| i.events.clone());
        for e in events {
            match e {
                egui::Event::Key { key, pressed: true, modifiers, .. } => {
                    if let Some(k) = from_egui(key, &modifiers) {
                        self.app.feed(k);
                    }
                }
                egui::Event::Text(t) => {
                    for c in t.chars() {
                        self.app.feed(Key::char(c));
                    }
                }
                _ => {}
            }
        }
    }

    fn script(&mut self, ctx: &egui::Context) {
        let Some(script) = &mut self.script else { return };
        if let Some(err) = script.shot(ctx) {
            self.app.toast(err);
        }
        for d in std::mem::take(&mut self.todo) {
            let Some(script) = &self.script else { return };
            match d {
                Do::State(name) => self.write_state(script.dir.join(format!("{name}.txt")), ""),
                Do::Quit => self.app.quit = true,
                // The settings page writes common.toml; the watcher applies it.
                Do::Theme(name) => self.app.prefs.common_change(vec![ito_common::CommonChange {
                    table: None,
                    key: "theme",
                    value: ito_common::quote(&name),
                }]),
                Do::Lang(code) => self.app.prefs.common_change(vec![ito_common::CommonChange {
                    table: None,
                    key: "language",
                    value: ito_common::quote(&code),
                }]),
                // yagura defines no readings of its own.
                Do::Read { .. } => {}
                Do::Done => {
                    if let Some(done) = script.done.clone() {
                        self.write_state(done, "keys: done\n");
                    }
                }
            }
        }
        if let Some(due) = self.script.as_ref().and_then(Runner::due) {
            ctx.request_repaint_after(due);
        }
    }
}

impl Yagura {
    /// The window's own bar: the name, the palette box, the bell and the
    /// caption buttons. The bell opens the list of toasts that went by.
    fn title_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        let unread = ito_notices::unread(&self.app.notices);
        let bell = i18n::trf("title.bell", &[&unread.to_string()]);
        let spec = ito_titlebar::Spec {
            own: !cfg!(target_os = "macos"),
            left: if cfg!(target_os = "macos") { 76.0 } else { 0.0 },
            maximized,
            unread,
            any_error: ito_notices::any_error(&self.app.notices),
            centre: true,
            right_min: 0.0,
            words: ito_titlebar::Words {
                search: i18n::tr("title.search"),
                search_key: "Ctrl+Shift+P",
                bell: &bell,
                minimize: i18n::tr("title.minimize"),
                maximize: i18n::tr("title.maximize"),
                restore: i18n::tr("title.restore"),
                close: i18n::tr("title.close"),
            },
        };
        let out = ito_titlebar::show(
            ui,
            &spec,
            |ui, rect| {
                let galley = ui.painter().layout_no_wrap("yagura".into(), egui::FontId::proportional(13.0), theme::fg());
                let x = rect.left() + 12.0;
                let right = x + galley.size().x;
                ui.painter().galley(egui::pos2(x, rect.center().y - galley.size().y / 2.0), galley, egui::Color32::WHITE);
                right
            },
            |_, _| {},
        );
        if let Some(r) = out.bell_rect {
            self.app.bell_at = r;
        }
        if out.bell {
            self.app.bell_open = !self.app.bell_open;
        }
        if out.search && self.app.mode != Mode::Settings {
            self.app.run(Act::Palette);
        }
        if let Some(op) = out.window {
            ito_titlebar::apply(&ctx, op, maximized);
        }
    }

    /// The list under the bell, while it is open.
    fn notice_list(&mut self, ctx: &egui::Context) {
        if !self.app.bell_open {
            return;
        }
        let title = i18n::trf("notices.title", &[&ito_notices::unread(&self.app.notices).to_string()]);
        let words = ito_notices::Words {
            title: &title,
            mark_read: i18n::tr("notices.mark_read"),
            empty: i18n::tr("notices.empty"),
            ago: i18n::tr("notices.ago"),
            kinds: [
                i18n::tr("notices.ok"),
                i18n::tr("notices.info"),
                i18n::tr("notices.warn"),
                i18n::tr("notices.err"),
                i18n::tr("notices.wait"),
            ],
        };
        match ito_notices::show(ctx, self.app.bell_at, &self.app.notices, &words) {
            Some(ito_notices::Action::Open(_, id)) => {
                self.app.notices.iter_mut().filter(|n| n.id == id).for_each(|n| n.read = true);
                self.app.bell_open = false;
            }
            Some(ito_notices::Action::ReadAll) => self.app.notices.iter_mut().for_each(|n| n.read = true),
            Some(ito_notices::Action::Close) => self.app.bell_open = false,
            None => {}
        }
    }
}

impl eframe::App for Yagura {
    fn on_exit(&mut self) {
        let keeps = ito_prefs::keeps_place(&self.app.prefs.common.window_or_default());
        if let (Some(place), Some(ctx)) = (&mut self.place, &self.app.wake)
            && keeps
        {
            place.close(Seen::of(ctx));
        }
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        if let Some(script) = &mut self.script
            && let Some(d) = script.input(ctx, raw)
        {
            self.todo.push(d);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let keys = if self.app.mode == Mode::Settings { self.settings_keys(&ctx) } else { ito_prefs::Keys::default() };
        if self.app.mode != Mode::Settings {
            self.keys(&ctx);
        }
        self.script(&ctx);
        self.app.pump();
        if let Some(rx) = &self.update
            && let Ok(found) = rx.try_recv()
        {
            self.update = None;
            if let ito_common::update::Update::Newer(v) = found {
                self.app.toast(i18n::trf("toast.update", &[&v, env!("CARGO_PKG_VERSION")]));
            }
        }
        // common.toml's theme, or the OS's light or dark under `system`.
        self.app.prefs.apply_theme(&ctx);
        egui::Panel::top("titlebar")
            .exact_size(ito_titlebar::HEIGHT)
            .frame(egui::Frame::NONE.fill(theme::bg()))
            .show(ui, |ui| self.title_bar(ui));
        self.notice_list(&ctx);
        if self.app.mode == Mode::Settings {
            ui::settings::show(ui, &mut self.app, keys);
        } else {
            let out = ui::frame(ui, &self.app);
            self.app.apply_out(out, &ctx);
        }
        if self.app.mode == Mode::Palette {
            self.app.show_palette(&ctx);
        }
        if let Some(text) = self.app.copy.take() {
            ctx.copy_text(text);
        }
        if self.app.quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // One more frame to take the toast away; nothing else needs a frame
        // until the sampler sends one.
        if let Some(left) = self.app.toast_left() {
            ctx.request_repaint_after(left);
        }
        if let Some(left) = self.app.probe_agent() {
            ctx.request_repaint_after(left);
        }
        // Last, so the edges of the frameless window win over what is under them.
        if !cfg!(target_os = "macos") {
            ito_titlebar::resize_edges(&ctx);
        }
        let window = self.app.prefs.common.window_or_default();
        if window.always_on_top != self.on_top {
            self.on_top = window.always_on_top;
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(if self.on_top {
                egui::WindowLevel::AlwaysOnTop
            } else {
                egui::WindowLevel::Normal
            }));
        }
        // `remember_place = false` neither saves the place nor restores it.
        if let Some(place) = self.place.as_mut().filter(|_| ito_prefs::keeps_place(&window)) {
            place.follow(&ctx);
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::bg().to_normalized_gamma_f32()
    }
}

/// Writes an answer (`--version`, `env`, `--snapshot`) to standard output:
/// ito's `ito_console::out` (v0.66.0, shared with every uchmk app). A file or
/// a pipe gets the text as it is, a console gets it in UTF-16 whatever its
/// code page, and nobody reading lets it go: `$v = & yagura.exe --version`
/// hands it a pipe PowerShell has already closed, where `println!` panicked.
/// No newline is added.
fn out(text: &str) {
    ito_console::out(text);
}

/// [`out`] on standard error, for warnings, refusals and the agent's log.
fn err(text: &str) {
    ito_console::err(text);
}

/// A release build on Windows has no console of its own: borrow the one it
/// was started from. [`out`] and [`err`] would join it themselves; this is
/// for the programs it starts (`reg` for `--install-service`), so they write
/// there too rather than open a console of their own.
#[cfg(windows)]
fn attach_console() {
    use ::windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    // SAFETY: no arguments to get wrong; failing (no parent console) is fine.
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

#[cfg(not(windows))]
fn attach_console() {}

/// This machine's name, for `--key`'s examples.
fn main() -> eframe::Result {
    collect::set_tr(|k| i18n::tr(k).to_string());
    let mut args = std::env::args().skip(1).peekable();
    if args.peek().is_some() {
        attach_console();
    }
    let mut script = None;
    let mut agent = false;
    let mut log = None;
    let mut service = None;
    let mut show_key = false;
    let mut snapshot = false;
    let mut metrics: Option<PathBuf> = None;
    let mut every = 5.0_f64;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--version" | "-V" => {
                out(&format!("yagura {}\n", env!("CARGO_PKG_VERSION")));
                return Ok(());
            }
            "--help" | "-h" => {
                out(&format!("{USAGE}\n"));
                return Ok(());
            }
            "env" | "--env" => {
                let to = match (args.next().as_deref(), args.next()) {
                    (None, _) => None,
                    (Some("--out"), Some(p)) => Some(PathBuf::from(p)),
                    _ => {
                        err("yagura: env takes only `--out FILE`\n");
                        std::process::exit(2);
                    }
                };
                let text = envreport::text();
                match to {
                    None => out(&text),
                    Some(path) => match std::fs::write(&path, &text) {
                        Ok(()) => err(&format!("yagura: wrote {}\n", std::path::absolute(&path).unwrap_or(path).display())),
                        Err(e) => {
                            err(&format!("yagura: {}: {e}\n", path.display()));
                            std::process::exit(1);
                        }
                    },
                }
                return Ok(());
            }
            "--agent" => agent = true,
            "--log" => {
                let Some(p) = args.next() else {
                    err("yagura: --log needs a file\n");
                    std::process::exit(2);
                };
                log = Some(PathBuf::from(p));
            }
            "--snapshot" => snapshot = true,
            "--log-metrics" => {
                let Some(p) = args.next() else {
                    err("yagura: --log-metrics needs a file\n");
                    std::process::exit(2);
                };
                metrics = Some(PathBuf::from(p));
            }
            "--every" => match args.next().and_then(|v| v.parse::<f64>().ok()).filter(|v| *v >= 1.0 && v.is_finite()) {
                Some(v) => every = v,
                None => {
                    err("yagura: --every needs a number of seconds, 1 or more\n");
                    std::process::exit(2);
                }
            },
            "--install-service" => service = Some(true),
            "--key" => show_key = true,
            "--uninstall-service" => service = Some(false),
            "--keys" => {
                let Some(s) = args.next() else {
                    err("yagura: --keys needs a script\n");
                    std::process::exit(2);
                };
                match ito_keyscript::parse(&s) {
                    Ok(steps) => script = Some(steps),
                    Err(e) => {
                        err(&format!("yagura --keys: {e}\n"));
                        std::process::exit(2);
                    }
                }
            }
            other => {
                err(&format!("yagura: unknown argument `{other}`\n\n{USAGE}\n"));
                std::process::exit(2);
            }
        }
    }

    let s = settings::load(None);
    if let Some(install) = service {
        for w in &s.warnings {
            err(&format!("yagura: {w}\n"));
        }
        let done = if install { service::install(s.port) } else { service::uninstall(s.port) };
        if let Err(e) = done {
            err(&format!("yagura: {e}\n"));
            std::process::exit(1);
        }
        return Ok(());
    }
    if snapshot {
        match export::snapshot() {
            Ok(json) => out(&format!("{json}\n")),
            Err(e) => {
                err(&format!("yagura --snapshot: {e}\n"));
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    if let Some(path) = &metrics {
        if let Err(e) = export::log_metrics(path, Duration::from_secs_f64(every)) {
            err(&format!("yagura --log-metrics: {e}\n"));
            std::process::exit(1);
        }
        return Ok(());
    }
    if show_key {
        let Some(dir) = &s.dir else {
            err("yagura --key: this machine has no config folder to keep a key in\n");
            std::process::exit(1);
        };
        match remote::noise::load_or_make(dir) {
            Ok(keys) => out(&remote::noise::howto(&keys.public, &remote::noise::key_path(dir), &remote::noise::host_name())),
            Err(e) => {
                err(&format!("yagura --key: {e}\n"));
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    if agent {
        if let Some(path) = &log
            && let Err(e) = remote::agent::log_to(path)
        {
            err(&format!("yagura --log: {}: {e}\n", path.display()));
            std::process::exit(1);
        }
        for w in &s.warnings {
            err(&format!("yagura: {w}\n"));
        }
        let direct = s.listen.as_ref().map(|listen| {
            let keys = s.dir.as_deref().ok_or_else(|| "this machine has no config folder to keep a key in".to_string());
            match keys.and_then(|d| remote::noise::load_or_make(d).map_err(|e| e.to_string())) {
                Ok(keys) => remote::agent::Direct { listen: listen.clone(), keys, viewers: s.viewers.clone() },
                Err(e) => {
                    err(&format!("yagura --agent: [agent] listen: {e}\n"));
                    std::process::exit(1);
                }
            }
        });
        let cfg = remote::agent::Config { allow: s.allow.clone(), interval: s.interval, direct, metrics: s.metrics.clone() };
        if let Err(e) = remote::agent::run(s.port, cfg) {
            err(&format!("yagura --agent: 127.0.0.1:{}: {e}\n", s.port));
            std::process::exit(1);
        }
        return Ok(());
    }
    i18n::set_language(&s.language);
    let (km, km_warn) = Keymap::load(s.keymap.as_deref());
    let (state_dir, state_note) = persist::dir(s.dir.as_deref());
    let (layouts, stacks, layout_warn) = persist::load(state_dir.as_deref());
    let (wgpu_options, gpu_warn) = ito_gpu::options(s.backend.as_deref());
    let warnings: Vec<String> = s
        .warnings
        .iter()
        .chain(&km_warn)
        .chain(&layout_warn)
        .cloned()
        .chain(gpu_warn.map(|w| format!("config.toml: {w}")))
        .chain(state_note)
        .collect();
    for w in &warnings {
        err(&format!("yagura: {w}\n"));
    }
    // Where the window was when it last closed, unless a script is driving
    // it (its screenshots want the size they ask for). ito's `Saved` reads
    // it: a place on none of the screens there are now (one that was
    // unplugged) is not put back, and a file from before the zoom was kept
    // is read at common.toml's `scale`.
    let window =
        settings::base_dir().and_then(|b| ito_common::Common::read(&b).ok()).map(|(c, _)| c).unwrap_or_default().window_or_default();
    let saved = Saved::load(state_dir.clone().filter(|_| script.is_none() && ito_prefs::keeps_place(&window)));
    let persist_dir = state_dir.clone();
    let saver = persist::Saver::new(state_dir);
    let interval = s.interval;
    let check_updates = s.check_updates;
    let hosts = s.hosts.clone();
    let done = std::env::var_os("YAGURA_KEYS_DONE").filter(|v| !v.is_empty()).map(PathBuf::from);

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("yagura")
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([480.0, 320.0])
        // The title bar, Alt+Tab, the taskbar and the Dock; build.rs bakes
        // the same drawing into yagura.exe for Explorer.
        .with_icon(std::sync::Arc::new(egui::IconData { rgba: icon::rgba(ICON_PX), width: ICON_PX, height: ICON_PX }));
    // Made at the place without the zoom; on Windows `put` (in the creator
    // below, while the window is hidden) moves it to the pixels it was
    // closed at, so a second screen at another scale gets it back (#58).
    viewport = saved.viewport(viewport);
    // The bar is yagura's own (the Mac keeps its traffic lights over it).
    viewport = ito_titlebar::frameless(viewport);
    if window.always_on_top {
        viewport = viewport.with_always_on_top();
    };
    let options = eframe::NativeOptions { viewport, wgpu_options, ..Default::default() };
    eframe::run_native(
        "yagura",
        options,
        Box::new(move |cc| {
            saved.put(cc);
            cc.egui_ctx.set_visuals(theme::visuals());
            fonts::load_in_background(cc.egui_ctx.clone());
            // Tell the other uchmk apps where this exe is; best-effort, off the UI thread.
            std::thread::spawn(|| _ = ito_common::apps::register("yagura", env!("CARGO_PKG_VERSION")));
            let (rx, ctl) = collect::spawn(crate::wake_of(Some(&cc.egui_ctx)), Duration::from_secs_f32(interval));
            let mut app = App::new(km, rx, ctl, layouts, saver, interval);
            app.stacks = stacks;
            app.wake = Some(cc.egui_ctx.clone());
            app.prefs = prefs::Prefs::load(settings::base_dir(), s);
            app.prefs.state_dir = persist_dir.clone();
            app.prefs.keymap_warn = km_warn;
            app.prefs.watch(&cc.egui_ctx);
            app.prefs.apply_theme(&cc.egui_ctx);
            // The scale is common.toml's, stepped by the keymap's scale keys,
            // so egui's own Ctrl+= / Ctrl+- stay out of it.
            cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
            cc.egui_ctx.set_zoom_factor(app.prefs.common.scale_or_one());
            if !hosts.is_empty() {
                let peers = hosts.iter().map(remote::Peer::new).collect();
                let fleet = remote::client::spawn(&hosts, app.prefs.settings.dir.clone(), Some(cc.egui_ctx.clone()));
                app.set_fleet(peers, fleet);
            }
            // One line in the status bar; the whole list went to stderr.
            if let Some(first) = warnings.first() {
                let more = if warnings.len() > 1 { format!(" (+{})", warnings.len() - 1) } else { String::new() };
                app.toast(format!("{first}{more}"));
            }
            // One question to GitHub per run; never under `--keys`.
            let update = (check_updates && script.is_none()).then(|| {
                let ctx = cc.egui_ctx.clone();
                ito_common::update::check("uchmk/yagura", env!("CARGO_PKG_VERSION"), move || ctx.request_repaint())
            });
            Ok(Box::new(Yagura {
                app,
                script: script.map(|steps| Runner::new(steps, done)),
                todo: Vec::new(),
                update,
                place: saved.tracker(),
                on_top: window.always_on_top,
            }))
        }),
    )
}
