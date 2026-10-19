//! The app's state and what each [`Act`] does. Drawing is in `ui/`; this
//! side owns the selection, the layouts and the sampler's channels.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use ito_layout::Node;

use crate::collect::{Ctl, Msg, Proc, containers, devices, disk, gpu, hw, net, sensors, svc};
use crate::config::cmd::{Act, Goto, Step};
use crate::config::keymap::{Keymap, Match, resolve};
use crate::config::keys::{Code, Key};
use crate::config::theme;
use crate::i18n::{tr, trf};
use crate::kill::{self, Outcome, Target};
use crate::model::{Model, Range};
use crate::persist::{Saver, Stacks};
use crate::prefs::{Prefs, Reload};
use crate::remote::client::{self, Event, ToHost, Why};
use crate::remote::proto::{self, Done, Hello, Refusal, Summary, Want};
use crate::remote::{self, Link, Peer};
use crate::screen::{Pane, Screen};
use crate::stacks;
use crate::svcctl::{self, Action};
use crate::zoom::Zoom;

/// The sampling intervals `+` and `-` step through, in seconds.
pub const INTERVALS: [f32; 6] = [0.5, 1.0, 2.0, 3.0, 5.0, 10.0];
/// How long a toast stays in the status bar.
pub const TOAST: Duration = Duration::from_secs(3);

/// How long a toast stays: longer for a long line, so there is time to read it.
pub fn toast_time(text: &str) -> Duration {
    let extra = crate::util::width(text).saturating_sub(40) as u64;
    TOAST + Duration::from_millis((extra * 60).min(5000))
}
/// How far a resize key moves a divider, as a share of the room.
const NUDGE: f32 = 0.03;
/// How often sys looks for this machine's agent while it shows.
/// How old the selected process's priority and open files get before they
/// are read again.
const PROC_INFO_EVERY: Duration = Duration::from_secs(5);

const PROBE_EVERY: Duration = Duration::from_secs(5);
/// Rows the overview lists in each of its two top lists.
pub const OVW_TOP: usize = 5;

/// A class in devices' left column.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DevClass {
    pub name: String,
    pub total: usize,
    pub shown: usize,
    pub problem: bool,
}

/// The lists a frame's readings changed.
#[derive(Default)]
struct Dirty {
    svc: bool,
    net: bool,
    dev: bool,
    ct: bool,
}

/// Whether a device matches devices' filter (lower case).
fn dev_matches(d: &devices::Device, f: &str) -> bool {
    f.is_empty()
        || [d.name.as_str(), d.vendor.as_str(), d.id.as_str(), d.driver.as_str()].iter().any(|s| s.to_lowercase().contains(f))
        || d.ids().is_some_and(|i| i.contains(f))
}

/// Which containers the containers screen's left column lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CtScope {
    All,
    Running,
    Paused,
    Stopped,
}

impl CtScope {
    pub const ALL: [CtScope; 4] = [CtScope::All, CtScope::Running, CtScope::Paused, CtScope::Stopped];

    pub fn key(self) -> &'static str {
        match self {
            CtScope::All => "ct.scope.all",
            CtScope::Running => "ct.scope.running",
            CtScope::Paused => "ct.scope.paused",
            CtScope::Stopped => "ct.scope.stopped",
        }
    }

    pub fn has(self, s: containers::State) -> bool {
        use containers::State::*;
        match self {
            CtScope::All => true,
            CtScope::Running => s == Running,
            CtScope::Paused => s == Paused,
            CtScope::Stopped => !s.live(),
        }
    }
}

/// Whether a container matches the containers filter (lower case).
fn ct_matches(c: &containers::Container, f: &str) -> bool {
    f.is_empty() || [c.name.as_str(), c.image.as_str(), c.id.as_str()].iter().any(|s| s.to_lowercase().contains(f))
}

/// A row of the overview's left column.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Gauge {
    Cpu,
    Mem,
    Swap,
    /// A GPU or NPU, by its index in the model's list.
    Gpu(usize),
    /// A drive, by its index in the model's list.
    Disk(usize),
}
const TOP_CPU: usize = 10;
const TOP_MEM: usize = 15;

/// What the confirm question asks.
#[derive(Clone, Debug, PartialEq)]
pub enum Ask {
    Kill(Target),
    Svc(svcctl::Target),
    Ct(containers::act::Target),
}

impl Ask {
    /// The process's, the service's or the container's name.
    pub fn name(&self) -> &str {
        match self {
            Ask::Kill(t) => &t.name,
            Ask::Svc(t) => &t.name,
            Ask::Ct(t) => &t.name,
        }
    }

    /// The `[agent] allow` word that lets another machine do it.
    pub fn word(&self) -> &'static str {
        match self {
            Ask::Kill(_) => "kill",
            Ask::Svc(_) => "service",
            Ask::Ct(_) => "container",
        }
    }

    /// The request for another machine's agent; none for a container,
    /// which is moved on this machine only.
    fn act(&self) -> Option<proto::Act> {
        match self {
            Ask::Kill(t) => Some(proto::Act::Kill { pid: t.pid, start: t.start }),
            Ask::Svc(t) => Some(proto::Act::Service { action: t.action, name: t.name.clone(), driver: t.driver }),
            Ask::Ct(_) => None,
        }
    }

    /// The toast while it runs.
    fn starting(&self) -> String {
        match self {
            Ask::Kill(t) => trf("toast.ending", &[&t.name, &t.pid.to_string()]),
            Ask::Svc(t) => {
                let key = match t.action {
                    Action::Start => "toast.svc_starting",
                    Action::Stop => "toast.svc_stopping",
                    Action::Restart => "toast.svc_restarting",
                };
                trf(key, &[&t.name])
            }
            Ask::Ct(t) => {
                let key = match t.action {
                    Action::Start => "toast.ct_starting",
                    Action::Stop => "toast.ct_stopping",
                    Action::Restart => "toast.ct_restarting",
                };
                trf(key, &[&t.name])
            }
        }
    }
}

/// A request sent to another machine's agent, until it answers.
#[derive(Clone, Debug, PartialEq)]
pub struct Asked {
    pub id: u64,
    /// The machine, by its index in `peers`.
    pub peer: usize,
    pub ask: Ask,
}

/// The kind of OS a machine runs, for the words that differ by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Windows,
    Mac,
    Other,
}

impl Family {
    pub fn here() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Mac
        } else {
            Self::Other
        }
    }

    /// From a machine's OS as its hello says it (`Windows 11 Pro`,
    /// `macOS 15 Sequoia`, `Linux 24.04 Ubuntu`).
    pub fn of(os: &str) -> Self {
        let os = os.to_lowercase();
        if os.contains("windows") {
            Self::Windows
        } else if os.contains("mac") || os.contains("darwin") {
            Self::Mac
        } else {
            Self::Other
        }
    }
}

/// The toast once a process's priority was stepped, or not.
pub fn renice_toast(t: &Target, o: crate::renice::Outcome) -> String {
    use crate::renice::Outcome;
    let (name, pid) = (t.name.as_str(), t.pid.to_string());
    match o {
        Outcome::Set(p) => trf("toast.prio_set", &[name, &pid, &p.label()]),
        Outcome::Limit(p) => trf("toast.prio_limit", &[name, &pid, &p.label()]),
        Outcome::Gone => trf("toast.gone", &[name, &pid]),
        Outcome::Unknown => trf("toast.prio_unknown", &[name, &pid]),
        Outcome::Failed(e) => trf("toast.prio_failed", &[name, &pid, &e]),
    }
}

/// The toast once a process's CPUs were chosen, or not.
pub fn aff_toast(t: &Target, o: crate::affinity::Outcome) -> String {
    use crate::affinity::Outcome;
    let (name, pid) = (t.name.as_str(), t.pid.to_string());
    match o {
        Outcome::Set(cpus) => trf("toast.aff_set", &[name, &pid, &cpus]),
        Outcome::Gone => trf("toast.gone", &[name, &pid]),
        Outcome::Failed(e) => trf("toast.aff_failed", &[name, &pid, &e]),
    }
}

/// The CPUs being chosen for a process (`A` in procs).
#[derive(Clone, Debug, PartialEq)]
pub struct AffEdit {
    pub t: Target,
    /// As typed: `0-3,8`.
    pub text: String,
    /// The CPUs there are to choose from, once read; until then the line
    /// takes nothing but Esc.
    pub ids: Option<Vec<usize>>,
    /// The CPUs it may run on now, once read and if the OS would say.
    pub now: Option<String>,
}

/// The toast once a process has been ended, or not.
pub fn kill_toast(t: &Target, o: Outcome) -> String {
    let (name, pid) = (t.name.as_str(), t.pid.to_string());
    match o {
        Outcome::Ended => trf("toast.ended", &[name, &pid]),
        Outcome::Killed => trf("toast.killed", &[name, &pid]),
        Outcome::Gone => trf("toast.gone", &[name, &pid]),
        Outcome::Failed(e) => trf("toast.kill_failed", &[name, &pid, &e]),
    }
}

/// The toast once a service has moved, or not, on a machine of `family`.
pub fn svc_toast(t: &svcctl::Target, o: svcctl::Outcome, family: Family) -> String {
    let n = t.name.as_str();
    match (o, t.action) {
        (svcctl::Outcome::Done, Action::Start) => trf("toast.svc_started", &[n]),
        (svcctl::Outcome::Done, Action::Stop) => trf("toast.svc_stopped", &[n]),
        (svcctl::Outcome::Done, Action::Restart) => trf("toast.svc_restarted", &[n]),
        (svcctl::Outcome::Denied, _) if family == Family::Windows => trf("toast.svc_denied_win", &[n]),
        (svcctl::Outcome::Denied, _) if family == Family::Mac => trf("toast.svc_denied_mac", &[n]),
        (svcctl::Outcome::Denied, _) => trf("toast.svc_denied_linux", &[n]),
        (svcctl::Outcome::Failed(e), Action::Start) => trf("toast.svc_start_failed", &[n, &e]),
        (svcctl::Outcome::Failed(e), Action::Stop) => trf("toast.svc_stop_failed", &[n, &e]),
        (svcctl::Outcome::Failed(e), Action::Restart) => trf("toast.svc_restart_failed", &[n, &e]),
    }
}

/// The toast once a container has moved, or not.
pub fn ct_toast(t: &containers::act::Target, o: containers::act::Outcome) -> String {
    use containers::act::Outcome as O;
    let n = t.name.as_str();
    match (o, t.action) {
        (O::Done, Action::Start) => trf("toast.ct_started", &[n]),
        (O::Done, Action::Stop) => trf("toast.ct_stopped", &[n]),
        (O::Done, Action::Restart) => trf("toast.ct_restarted", &[n]),
        (O::Already, Action::Start) => trf("toast.ct_already_running", &[n]),
        (O::Already, _) => trf("toast.ct_not_running", &[n]),
        (O::Gone, _) => trf("toast.ct_gone", &[n]),
        (O::Denied, _) => trf("toast.ct_denied", &[n]),
        (O::NoEngine, _) => trf("toast.ct_no_engine", &[n]),
        (O::Slow, _) => trf("toast.ct_slow", &[n]),
        (O::Unclear, _) => trf("toast.ct_unclear", &[n]),
        (O::Failed(e), Action::Start) => trf("toast.ct_start_failed", &[n, &e]),
        (O::Failed(e), Action::Stop) => trf("toast.ct_stop_failed", &[n, &e]),
        (O::Failed(e), Action::Restart) => trf("toast.ct_restart_failed", &[n, &e]),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Filter,
    Help,
    /// The command palette over the screen.
    Palette,
    /// The question before a process is ended.
    Confirm,
    /// The settings screen (`Ctrl+,`), across the whole window.
    Settings,
    /// A graph opened full size.
    Zoom,
    /// The CPUs a process may run on, being chosen.
    Affinity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortCol {
    Pid,
    Name,
    Cpu,
    Mem,
    Thr,
    User,
}

impl SortCol {
    pub const ALL: [SortCol; 6] = [Self::Pid, Self::Name, Self::Cpu, Self::Mem, Self::Thr, Self::User];

    pub fn label(self) -> &'static str {
        match self {
            Self::Pid => tr("procs.col.pid"),
            Self::Name => tr("procs.col.name"),
            Self::Cpu => tr("procs.col.cpu"),
            Self::Mem => tr("procs.col.mem"),
            Self::Thr => tr("procs.col.thr"),
            Self::User => tr("procs.col.user"),
        }
    }

    fn next(self) -> Self {
        let i = Self::ALL.iter().position(|&c| c == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }
}

/// Inside another machine's screens: what is kept of this one's meanwhile.
pub struct Away {
    /// The machine, by its index in `peers`.
    pub peer: usize,
    /// This machine's model, still fed by its sampler.
    pub home: Model,
    home_views: HashMap<Screen, View>,
    home_me: Option<String>,
    /// The connection dropped while inside; the last readings stay.
    pub lost: bool,
}

/// Where each screen's cursor is: the focused column and a row per column.
#[derive(Clone, Copy, Debug, Default)]
pub struct View {
    pub focus: usize,
    pub sel: [usize; 3],
    pub scroll: [usize; 3],
}

/// What a frame's drawing reports back, applied once drawing is done.
#[derive(Default)]
pub struct Out {
    /// A row clicked: its column, its index, and whether it was the second
    /// click of a double click.
    pub click: Option<(Pane, usize, bool)>,
    pub scroll: [Option<usize>; 3],
    pub visible: [usize; 3],
    /// Cells a value has on a row of the sys screen's left column.
    pub sys_room: Option<usize>,
    pub pane_rects: [Option<egui::Rect>; 3],
    pub tab: Option<Screen>,
    /// A graph double-clicked: what to open full size.
    pub zoom: Option<Zoom>,
    /// A divider dragged, released or double-clicked.
    pub moved: Option<ito_layout::ui::Moved<Pane>>,
    /// The panels stacked in a column this frame: the column, their layout
    /// and the room it was laid over, for their dividers.
    pub stacks: Vec<(Pane, Node<u8>, egui::Rect)>,
    /// A divider between stacked panels dragged, released or double-clicked.
    pub stack_moved: Option<(Pane, ito_layout::ui::Moved<u8>)>,
    /// How far the help can scroll, so a key past the end stays there.
    pub help_max: Option<usize>,
    /// The fleet tab clicked.
    pub fleet_tab: bool,
    /// Cards in a row of the fleet, for the up and down keys.
    pub fleet_cols: Option<usize>,
    /// A fleet card clicked (0 is this machine), and whether twice.
    pub fleet_click: Option<(usize, bool)>,
    /// A line of the fleet's hint clicked: what it copies.
    pub copy: Option<String>,
    /// A CPU box clicked in the affinity editor.
    pub cpu_click: Option<usize>,
}

pub struct App {
    km: Keymap,
    pub model: Model,
    rx: Receiver<Msg>,
    ctl: Sender<Ctl>,
    pub screen: Screen,
    pub layouts: HashMap<Screen, Node<Pane>>,
    /// The layout while a divider is being dragged; kept on release.
    pub drag: Option<Node<Pane>>,
    /// The panels of a column where the mouse moved their dividers.
    pub stacks: Stacks,
    /// The panels of a column while a divider between them is dragged.
    pub stack_drag: Option<(Screen, Pane, Node<u8>)>,
    views: HashMap<Screen, View>,
    pub visible: [usize; 3],
    /// Cells a value had on the sys screen's left column, last drawn; longer
    /// ones go on under their key, so the rows to select follow it.
    pub sys_room: usize,
    pub mode: Mode,
    pending: Vec<Key>,
    /// The `/` text of procs, svc and net, each its own.
    filters: HashMap<Screen, String>,
    pub sort: SortCol,
    proc_pid: Option<u32>,
    /// The process list as procs shows it: indices into `model.procs`.
    pub rows: Vec<usize>,
    /// procs as a tree (`t`) rather than a flat list.
    pub tree: bool,
    /// The branch lines in front of each row's name, while `tree` is on.
    pub tree_lines: Vec<String>,
    pub top_cpu: Vec<usize>,
    pub top_mem: Vec<usize>,
    /// The service list as svc shows it: indices into `model.services.list`.
    pub svc_rows: Vec<usize>,
    /// The service selected, kept while the list is read again.
    svc_name: Option<String>,
    /// svc shows Windows' drivers instead of its services (`u`).
    pub svc_drivers: bool,
    /// The socket list as net shows it: indices into `model.conns.list`.
    pub net_rows: Vec<usize>,
    /// The socket selected, kept while the list is read again.
    net_key: Option<ConnKey>,
    /// net lists only the sockets waiting for others (`u`).
    pub net_listen: bool,
    /// The classes devices' left column lists, after "all".
    pub dev_classes: Vec<DevClass>,
    /// The class selected, kept while the list is read again.
    dev_class: Option<String>,
    /// The device list as devices shows it: indices into `model.devices`.
    pub dev_rows: Vec<usize>,
    /// The branch lines in front of each device, while `dev_tree` is on.
    pub dev_lines: Vec<String>,
    /// devices as the tree of what hangs off what (`t`).
    pub dev_tree: bool,
    /// The device selected, kept while the list is read again.
    dev_id: Option<String>,
    /// The container list as containers shows it: indices into
    /// `model.containers`.
    pub ct_rows: Vec<usize>,
    /// The container selected, kept while the list is read again.
    ct_id: Option<String>,
    /// net lists only the sockets on this interface (`Enter` on it, `Esc`
    /// drops it); it follows the selection while set.
    pub net_nic: Option<String>,
    me: Option<String>,
    dirty: bool,
    pub interval: f32,
    pub paused: bool,
    pub toast: Option<(String, Instant)>,
    /// Every toast, kept for the bell's list (the newest last).
    pub notices: Vec<ito_notices::Notice<()>>,
    pub bell_open: bool,
    /// Where the title bar's bell is, so the list opens under it.
    pub bell_at: egui::Rect,
    pub help_scroll: usize,
    pub palette: ito_palette::View,
    /// What the confirm question is about.
    pub confirm: Option<Ask>,
    /// The graph open full size, in [`Mode::Zoom`].
    pub zoom: Option<Zoom>,
    /// The CPUs being chosen, in [`Mode::Affinity`].
    pub affinity: Option<AffEdit>,
    /// The range the last zoom looked back over; the next starts there.
    zoom_range: Range,
    /// What the `[[alert]]` rules have seen of this machine.
    alerts: crate::alert::Tracker,
    /// What the procs detail knows of the selected process beyond the list,
    /// and when it was asked; read on a thread of its own, one at a time.
    proc_info: Option<(u32, crate::procinfo::Info, Instant)>,
    proc_info_busy: bool,
    proc_info_tx: Sender<(u32, crate::procinfo::Info)>,
    proc_info_rx: Receiver<(u32, crate::procinfo::Info)>,
    kill_tx: Sender<(Target, Outcome)>,
    renice_tx: Sender<(Target, crate::renice::Outcome)>,
    renice_rx: Receiver<(Target, crate::renice::Outcome)>,
    aff_tx: Sender<(Target, crate::affinity::Outcome)>,
    aff_rx: Receiver<(Target, crate::affinity::Outcome)>,
    look_tx: Sender<(u32, crate::affinity::Look)>,
    look_rx: Receiver<(u32, crate::affinity::Look)>,
    kill_rx: Receiver<(Target, Outcome)>,
    svc_tx: Sender<(svcctl::Target, svcctl::Outcome)>,
    svc_rx: Receiver<(svcctl::Target, svcctl::Outcome)>,
    ct_tx: Sender<(containers::act::Target, containers::act::Outcome)>,
    ct_rx: Receiver<(containers::act::Target, containers::act::Outcome)>,
    /// Containers asked to move whose answer has not come back, by key.
    ct_busy: HashSet<String>,
    /// Repainted when a process has been ended or a service or container
    /// has moved, so the toast shows at once.
    pub wake: Option<egui::Context>,
    saver: Saver,
    pub quit: bool,
    wheel_acc: f32,
    /// The fleet shows over the screen (`F`).
    pub fleet: bool,
    /// The `[[host]]` machines; card 0 is this one, card `i` is `peers[i - 1]`.
    pub peers: Vec<Peer>,
    fleet_rx: Option<Receiver<(usize, Event)>>,
    /// What tells each host's watcher what to send and what to ask, by its
    /// index in `peers`.
    fleet_tx: Vec<Sender<ToHost>>,
    /// Requests sent to other machines' agents and not yet answered.
    pub asked: Vec<Asked>,
    next_act: u64,
    /// What each host was last told to send.
    sent_remote: Vec<Want>,
    /// Whether this machine's sampler was last told to read services,
    /// sockets and devices.
    sent_local: (bool, bool, bool, bool),
    /// Inside another machine's screens (`Enter` on its card, `[` / `]`).
    pub away: Option<Away>,
    /// The card selected.
    pub fleet_sel: usize,
    /// Cards in a row, as last drawn.
    pub fleet_cols: usize,
    /// The settings files and the settings screen's state.
    pub prefs: Prefs,
    /// Words to put on the clipboard this frame.
    pub copy: Option<String>,
    /// This machine's agent, for sys.
    pub probe: Probe,
}

/// What sys last found on this machine's agent port, looked at on a thread
/// every [`PROBE_EVERY`] while it shows.
pub struct Probe {
    /// `None` until the first look comes back.
    pub last: Option<Result<(Hello, Summary), Why>>,
    tx: Sender<Result<(Hello, Summary), Why>>,
    rx: Receiver<Result<(Hello, Summary), Why>>,
    busy: bool,
    next: Option<Instant>,
}

impl Default for Probe {
    fn default() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self { last: None, tx, rx, busy: false, next: None }
    }
}

impl App {
    pub fn new(km: Keymap, rx: Receiver<Msg>, ctl: Sender<Ctl>, layouts: HashMap<Screen, Node<Pane>>, saver: Saver, interval: f32) -> Self {
        let (kill_tx, kill_rx) = crossbeam_channel::unbounded();
        let (renice_tx, renice_rx) = crossbeam_channel::unbounded();
        let (aff_tx, aff_rx) = crossbeam_channel::unbounded();
        let (look_tx, look_rx) = crossbeam_channel::unbounded();
        let (svc_tx, svc_rx) = crossbeam_channel::unbounded();
        let (ct_tx, ct_rx) = crossbeam_channel::unbounded();
        let (proc_info_tx, proc_info_rx) = crossbeam_channel::unbounded();
        Self {
            km,
            model: Model::default(),
            rx,
            ctl,
            screen: Screen::Ovw,
            layouts,
            drag: None,
            stacks: Stacks::new(),
            stack_drag: None,
            views: new_views(),
            visible: [1; 3],
            sys_room: 0,
            mode: Mode::Normal,
            pending: Vec::new(),
            filters: HashMap::new(),
            sort: SortCol::Cpu,
            proc_pid: None,
            rows: Vec::new(),
            tree: false,
            tree_lines: Vec::new(),
            top_cpu: Vec::new(),
            top_mem: Vec::new(),
            svc_rows: Vec::new(),
            svc_name: None,
            svc_drivers: false,
            net_rows: Vec::new(),
            net_key: None,
            net_listen: false,
            dev_classes: Vec::new(),
            dev_class: None,
            dev_rows: Vec::new(),
            dev_lines: Vec::new(),
            dev_tree: false,
            dev_id: None,
            ct_rows: Vec::new(),
            ct_id: None,
            net_nic: None,
            me: None,
            dirty: true,
            interval,
            paused: false,
            toast: None,
            notices: Vec::new(),
            bell_open: false,
            bell_at: egui::Rect::NOTHING,
            help_scroll: 0,
            palette: ito_palette::View::new(),
            confirm: None,
            zoom: None,
            affinity: None,
            zoom_range: Range::default(),
            alerts: crate::alert::Tracker::default(),
            proc_info: None,
            proc_info_busy: false,
            proc_info_tx,
            proc_info_rx,
            kill_tx,
            renice_tx,
            renice_rx,
            aff_tx,
            aff_rx,
            look_tx,
            look_rx,
            kill_rx,
            svc_tx,
            svc_rx,
            ct_tx,
            ct_rx,
            ct_busy: HashSet::new(),
            wake: None,
            saver,
            quit: false,
            wheel_acc: 0.0,
            fleet: false,
            peers: Vec::new(),
            fleet_rx: None,
            fleet_tx: Vec::new(),
            asked: Vec::new(),
            next_act: 0,
            sent_remote: Vec::new(),
            sent_local: (false, false, false, false),
            away: None,
            fleet_sel: 0,
            fleet_cols: 1,
            prefs: Prefs::empty(),
            copy: None,
            probe: Probe::default(),
        }
    }

    /// The `[[host]]` machines, their watchers' events and what tells each
    /// watcher what to send.
    pub fn set_fleet(&mut self, peers: Vec<Peer>, (rx, tx): (Receiver<(usize, Event)>, Vec<Sender<ToHost>>)) {
        self.sent_remote = vec![Want::default(); peers.len()];
        self.peers = peers;
        self.fleet_rx = Some(rx);
        self.fleet_tx = tx;
    }

    /// This machine's model, also while inside another's.
    pub fn home(&self) -> &Model {
        self.away.as_ref().map_or(&self.model, |a| &a.home)
    }

    /// The machine whose screens show, unless it is this one.
    pub fn away_peer(&self) -> Option<&Peer> {
        self.away.as_ref().and_then(|a| self.peers.get(a.peer))
    }

    /// The kind of OS of the machine whose screens show.
    pub fn family(&self) -> Family {
        match self.away_peer() {
            Some(p) => Family::of(p.hello.as_ref().map_or("", |h| h.os.as_str())),
            None => Family::here(),
        }
    }

    /// The fleet card selected, unless it is this machine's.
    pub fn selected_peer(&self) -> Option<&Peer> {
        self.fleet_sel.checked_sub(1).and_then(|i| self.peers.get(i))
    }

    pub fn keymap(&self) -> &Keymap {
        &self.km
    }

    pub fn view(&self) -> View {
        self.views.get(&self.screen).copied().unwrap_or_default()
    }

    pub fn view_of(&self, s: Screen) -> View {
        self.views.get(&s).copied().unwrap_or_default()
    }

    fn view_mut(&mut self) -> &mut View {
        self.views.entry(self.screen).or_default()
    }

    pub fn focus(&self) -> Pane {
        Pane::ALL[self.view().focus.min(2)]
    }

    pub fn layout(&self) -> &Node<Pane> {
        self.drag.as_ref().unwrap_or(&self.layouts[&self.screen])
    }

    /// The panels to stack in column `pane` of this screen: as they are
    /// being dragged, else as the mouse left them, else `default`; a kept
    /// layout only while the screen stacks the same panels the same way.
    pub fn stack(&self, pane: Pane, default: Node<u8>) -> Node<u8> {
        if let Some((s, p, n)) = &self.stack_drag
            && (*s, *p) == (self.screen, pane)
            && stacks::same_shape(n, &default)
        {
            return n.clone();
        }
        match self.stacks.get(&(self.screen, pane)) {
            Some(n) if stacks::same_shape(n, &default) => n.clone(),
            _ => default,
        }
    }

    fn save_layouts(&self) {
        self.saver.save(&self.layouts, &self.stacks);
    }

    pub fn toast(&mut self, s: impl Into<String>) {
        let text: String = s.into();
        let id = self.notices.last().map_or(1, |n| n.id + 1);
        self.notices.push(ito_notices::Notice {
            id,
            at: std::time::SystemTime::now(),
            kind: ito_notices::Kind::Info,
            title: text.clone(),
            note: None,
            go: None,
            read: false,
        });
        // The list is for glancing back, not for keeping everything.
        if self.notices.len() > 50 {
            self.notices.remove(0);
        }
        self.toast = Some((text, Instant::now()));
    }

    /// Put `text` on the clipboard (in the frame) and say so.
    pub fn copy(&mut self, text: String) {
        self.toast(trf("toast.copied", &[text.lines().next().unwrap_or("")]));
        self.copy = Some(text);
    }

    /// Sample every `s` seconds from now on.
    pub fn set_interval(&mut self, s: f32) {
        self.interval = s;
        let _ = self.ctl.send(Ctl::Interval(Duration::from_secs_f32(s)));
    }

    /// One step of the scale every uchmk app shares: shown at once, written
    /// to common.toml on a thread (the other apps follow it from there).
    pub fn scale(&mut self, to: ito_common::ScaleTo) {
        let next = ito_common::scale_step(self.prefs.common.scale_or_one(), to);
        self.prefs.common.scale = Some(next);
        self.apply_scale();
        self.prefs.common_change(vec![ito_common::scale_change(next)]);
        self.toast(ito_common::scale_text(next, crate::i18n::current_language()));
    }

    /// Put common.toml's scale on the window.
    fn apply_scale(&self) {
        if let Some(ctx) = &self.wake {
            let s = self.prefs.common.scale_or_one();
            if ctx.zoom_factor() != s {
                ctx.set_zoom_factor(s);
            }
        }
    }

    /// What the palette offers: the main layer's bindings that are commands
    /// rather than movement, one line each, with the keys that do the same.
    fn palette_entries(&self) -> Vec<ito_palette::Entry<Act>> {
        let mut out: Vec<ito_palette::Entry<Act>> = Vec::new();
        for b in crate::config::keymap::live(&self.km.main) {
            let [act] = b.run[..] else { continue };
            if matches!(
                act,
                Act::Arrow(_)
                    | Act::Focus(_)
                    | Act::Resize(_)
                    | Act::Enter
                    | Act::Escape
                    | Act::Palette
                    | Act::Backspace
                    | Act::Confirm
                    | Act::Page(_)
                    | Act::Noop
            ) {
                continue;
            }
            let keys = crate::config::keys::render_seq(&b.on);
            match out.iter_mut().find(|e| e.pick == act) {
                Some(e) => {
                    e.key.push_str(" / ");
                    e.key.push_str(&keys);
                }
                None => out.push(ito_palette::Entry::new(crate::i18n::tr_desc(&b.desc), keys, act)),
            }
        }
        out
    }

    /// Draw the palette and do what it answers; called every frame it is open.
    pub fn show_palette(&mut self, ctx: &egui::Context) {
        let entries = self.palette_entries();
        let style = ito_palette::Style {
            panel: theme::bg_alt(),
            border: theme::line(),
            text: theme::fg(),
            dim: theme::dim(),
            selected_bg: theme::sel(),
            selected_text: theme::strong(),
            hit: theme::yellow(),
            accent: theme::green(),
        };
        let words = ito_palette::Words { hint: tr("palette.hint"), none: tr("palette.none"), header: "", footer: "" };
        match ito_palette::show(ctx, &mut self.palette, &entries, &style, &words) {
            None => {}
            // No extra keys are registered (`View::keys` stays empty), so this never fires.
            Some(ito_palette::Answer::Key(..)) => {}
            Some(ito_palette::Answer::Close) => self.mode = Mode::Normal,
            Some(ito_palette::Answer::Pick(act)) => {
                self.mode = Mode::Normal;
                self.run(act);
            }
        }
    }

    pub fn open_settings(&mut self) {
        self.mode = Mode::Settings;
        self.pending.clear();
        self.give_up_key();
    }

    /// Opens the pre-filled bug report (F12, and the button on the Advanced page).
    pub fn bug_report(&mut self) {
        self.prefs.open_url(crate::bugreport::url(&self.prefs.settings, self.prefs.common.scale_or_one()));
        self.toast(tr("toast.bugreport"));
    }

    pub fn close_settings(&mut self) {
        self.mode = Mode::Normal;
        self.give_up_key();
    }

    /// Stop taking a key for the Keys page, or the settings come back still
    /// taking keys (Esc included).
    fn give_up_key(&mut self) {
        self.prefs.keys_edit.capture = None;
        self.prefs.keys_edit.listening = false;
    }

    /// What a key does on the settings screen, from its layer of the keymap.
    pub fn settings_act(&self, k: Key) -> Option<Act> {
        match resolve(&self.km.settings, &[k]) {
            Match::Exact(b) => b.run.first().copied(),
            _ => None,
        }
    }

    /// The language as config.toml, common.toml and the OS pick it now.
    fn relanguage(&self) {
        let os = ito_i18n::os_language();
        let app = self.prefs.settings.app_language.as_deref();
        crate::i18n::set_language(&crate::i18n::resolve_language(app, self.prefs.common.language.as_deref(), os.as_deref()));
    }

    /// Fold in a settings file read again.
    fn reload(&mut self, r: Reload) {
        match r {
            Reload::Common(Ok((c, w))) => {
                self.prefs.common = c;
                self.prefs.common_warn = w;
                self.relanguage();
                self.apply_scale();
            }
            Reload::Common(Err(e)) => {
                self.toast(e.clone());
                self.prefs.common_warn = vec![e];
            }
            Reload::Themes(themes, w) => {
                self.prefs.themes = themes;
                self.prefs.theme_warn = w;
            }
            Reload::Keys(s, said) => {
                self.reload(Reload::Settings(s));
                self.toast(said);
            }
            Reload::Settings(s) => {
                let old = std::mem::replace(&mut self.prefs.settings, *s);
                if self.prefs.settings.keymap != old.keymap {
                    // keymap.toml changed (the Keys page, or by hand): the
                    // keys, the help and the hints follow at once.
                    let (km, warn) = Keymap::load(self.prefs.settings.keymap.as_deref());
                    self.km = km;
                    self.prefs.keymap_warn = warn;
                }
                let interval = self.prefs.settings.interval;
                if interval != old.interval {
                    self.set_interval(interval);
                }
                if self.prefs.settings.hosts != old.hosts {
                    // The machine inside may be gone or another now.
                    self.go_home();
                    let hosts = self.prefs.settings.hosts.clone();
                    let fleet = remote::client::spawn(&hosts, self.prefs.settings.dir.clone(), self.wake.clone());
                    self.set_fleet(hosts.iter().map(Peer::new).collect(), fleet);
                    self.fleet_sel = self.fleet_sel.min(self.peers.len());
                }
                self.relanguage();
            }
            Reload::Key(k) => self.prefs.key = Some(k),
            Reload::Startup(on) => self.prefs.startup = Some(on),
            Reload::Said(s) => self.toast(&s),
            Reload::Failed(e) => self.toast(trf("toast.save_failed", &[&e])),
        }
    }

    /// Fold in what the sampler sent and bring the lists up to date.
    pub fn pump(&mut self) {
        let mut dirty = Dirty::default();
        while let Ok(m) = self.rx.try_recv() {
            match &mut self.away {
                // Kept up to date for the way back and this machine's card.
                Some(a) => a.home.apply(m),
                None => self.take(m, &mut dirty),
            }
        }
        if let Some(rx) = &self.fleet_rx {
            let events: Vec<(usize, Event)> = rx.try_iter().collect();
            for (i, e) in events {
                self.peer_event(i, e, &mut dirty);
            }
        }
        if dirty.svc {
            self.refresh_svc();
        }
        if dirty.net && self.model.conn_read {
            self.refresh_net();
        }
        if dirty.dev {
            self.refresh_dev();
        }
        if dirty.ct {
            self.refresh_ct();
        }
        while let Ok((t, o)) = self.kill_rx.try_recv() {
            self.toast(kill_toast(&t, o));
        }
        while let Ok((t, o)) = self.renice_rx.try_recv() {
            self.toast(renice_toast(&t, o));
            // Read what the detail shows again, now.
            self.proc_info = None;
        }
        while let Ok((t, o)) = self.aff_rx.try_recv() {
            self.toast(aff_toast(&t, o));
            self.proc_info = None;
        }
        while let Ok((pid, look)) = self.look_rx.try_recv() {
            self.aff_looked(pid, look);
        }
        while let Ok((t, o)) = self.svc_rx.try_recv() {
            self.toast(svc_toast(&t, o, Family::here()));
            // Read the list again now rather than at the next interval;
            // not while inside another machine, whose agent does that.
            if self.screen == Screen::Svc && self.away.is_none() {
                let _ = self.ctl.send(Ctl::Services(true));
            }
        }
        while let Ok((t, o)) = self.ct_rx.try_recv() {
            self.ct_busy.remove(t.key());
            self.toast(ct_toast(&t, o));
            if self.screen == Screen::Containers && self.away.is_none() {
                let _ = self.ctl.send(Ctl::Containers(true));
            }
        }
        while let Ok(r) = self.prefs.rx.try_recv() {
            self.reload(r);
        }
        if self.dirty {
            self.dirty = false;
            self.refresh_rows();
        }
        self.check_alerts();
        self.check_proc_info();
        self.clamp_all();
        // Dropped once it has had its time, or the frame after it would ask
        // for another frame forever.
        if self.toast.is_some() && self.toast_left().is_none() {
            self.toast = None;
        }
    }

    /// The selected process's priority, affinity and open files, read again
    /// when it is another process or [`PROC_INFO_EVERY`] old; only for this
    /// machine's processes while the procs screen shows.
    fn check_proc_info(&mut self) {
        while let Ok((pid, info)) = self.proc_info_rx.try_recv() {
            self.proc_info_busy = false;
            self.proc_info = Some((pid, info, Instant::now()));
        }
        if self.screen != Screen::Procs || self.away.is_some() || self.proc_info_busy {
            return;
        }
        let Some(pid) = self.selected_proc().map(|p| p.pid) else { return };
        let due = self.proc_info.as_ref().is_none_or(|(p, _, at)| *p != pid || at.elapsed() >= PROC_INFO_EVERY);
        if due {
            self.proc_info_busy = crate::procinfo::spawn(pid, self.proc_info_tx.clone(), self.wake.clone());
        }
    }

    /// The CPUs the process being given new ones may run on now, once read.
    pub fn affinity_now(&self) -> Option<&str> {
        self.affinity.as_ref()?.now.as_deref()
    }

    /// What the editor starts from, read: the CPUs there are, and the ones
    /// it has now on the line. One it has that is not there to choose (a CPU
    /// switched off) is left off the line, so Enter does not refuse it.
    pub(crate) fn aff_looked(&mut self, pid: u32, look: crate::affinity::Look) {
        let Some(e) = &mut self.affinity else { return };
        if e.t.pid != pid || e.ids.is_some() {
            return;
        }
        let have = look.now.as_deref().map(|n| crate::affinity::lenient(n, &look.ids)).unwrap_or_default();
        e.text = crate::procinfo::ranges(&have);
        e.now = look.now;
        e.ids = Some(look.ids);
    }

    /// What is known of the selected process beyond the list.
    pub fn selected_info(&self) -> Option<&crate::procinfo::Info> {
        let pid = self.selected_proc()?.pid;
        self.proc_info.as_ref().filter(|(p, ..)| *p == pid).map(|(_, i, _)| i)
    }

    /// The `[[alert]]` rules against this machine's readings (the home one
    /// while another machine's screens show): what fires is toasted here and
    /// told as `[notify]` asks.
    fn check_alerts(&mut self) {
        let rules = &self.prefs.settings.alerts;
        if rules.is_empty() && self.alerts.is_idle() {
            return;
        }
        let m = self.away.as_ref().map_or(&self.model, |a| &a.home);
        let fired = self.alerts.step(rules, Instant::now(), |rule| rule.read(m));
        let host = m.st.as_ref().map(|s| s.host.clone()).unwrap_or_default();
        for f in fired {
            let (title, body) = f.words(&host);
            self.toast(format!("{title}: {body}"));
            crate::alert::tell(&self.prefs.settings.notify, &title, &body);
        }
    }

    /// Fold a reading into the screens' model, noting the lists it changes
    /// (services, sockets, devices).
    fn take(&mut self, m: Msg, dirty: &mut Dirty) {
        match m {
            // The net filter matches process names too.
            Msg::Procs { .. } => (self.dirty, dirty.net) = (true, true),
            Msg::Svc(_) => dirty.svc = true,
            Msg::Conns(_) => dirty.net = true,
            Msg::Devices(_) => dirty.dev = true,
            Msg::Containers(_) => dirty.ct = true,
            // An interface's addresses may change under its narrowing.
            Msg::Nics(_) if self.net_nic.is_some() => dirty.net = true,
            _ => {}
        }
        self.model.apply(m);
    }

    /// News from a host's watcher: its card's, and its readings while its
    /// screens show.
    fn peer_event(&mut self, i: usize, e: Event, dirty: &mut Dirty) {
        let inside = self.away.as_ref().is_some_and(|a| a.peer == i);
        match e {
            Event::Msg(m) if inside => self.take(*m, dirty),
            Event::Msg(_) => {}
            Event::Done(id, d) => self.answered(i, id, d),
            e => {
                let (down, up) = (matches!(e, Event::Down(_)), matches!(e, Event::Up(_)));
                let Some(p) = self.peers.get_mut(i) else { return };
                p.apply(e);
                let name = p.name.clone();
                if let Some(a) = self.away.as_mut().filter(|_| inside) {
                    if down && !a.lost {
                        a.lost = true;
                        self.toast(trf("toast.remote_lost", &[&name]));
                    } else if up && a.lost {
                        // The watcher says again what it wants.
                        a.lost = false;
                        self.toast(trf("toast.machine", &[&name]));
                    }
                }
                // No answer comes over the next connection.
                let asked = self.asked.len();
                self.asked.retain(|a| !down || a.peer != i);
                if self.asked.len() < asked {
                    self.toast(trf("toast.remote_no_answer", &[&name]));
                }
            }
        }
    }

    /// The answer to request `id` sent to `peers[i]`.
    fn answered(&mut self, i: usize, id: u64, d: Done) {
        let Some(at) = self.asked.iter().position(|a| a.id == id && a.peer == i) else { return };
        let ask = self.asked.remove(at).ask;
        let Some(p) = self.peers.get(i) else { return };
        let name = p.name.clone();
        let family = Family::of(p.hello.as_ref().map_or("", |h| h.os.as_str()));
        let on = |s: String| trf("toast.on_machine", &[&name, &s]);
        let s = match (d, &ask) {
            (Done::Kill(o), Ask::Kill(t)) => on(kill_toast(t, o)),
            (Done::Svc(o), Ask::Svc(t)) => on(svc_toast(t, o, family)),
            (Done::Refused(Refusal::NotAllowed), _) => trf("toast.fleet_view_only", &[&name, ask.word()]),
            (Done::Refused(Refusal::Agent), _) => trf("toast.kill_agent", &[&name]),
            (Done::Refused(Refusal::NoService), _) => on(trf("toast.remote_no_service", &[ask.name()])),
            (Done::Refused(Refusal::Disabled(line)), _) => on(trf("toast.svc_disabled", &[ask.name(), &line])),
            // An answer of the other kind is not this request's.
            (Done::Kill(_) | Done::Svc(_), _) => return,
        };
        self.toast(s);
    }

    /// Send a confirmed question to the agent of the machine whose screens
    /// show.
    fn send_act(&mut self, ask: Ask) {
        let Some(i) = self.away.as_ref().map(|a| a.peer) else { return };
        let Some(p) = self.peers.get(i) else { return };
        let name = p.name.clone();
        let Some(act) = ask.act() else {
            self.toast(trf("toast.ct_remote", &[&name]));
            return;
        };
        self.next_act += 1;
        let id = self.next_act;
        let sent = p.link == Link::Up && self.fleet_tx.get(i).is_some_and(|tx| tx.send(ToHost::Act(id, act)).is_ok());
        if !sent {
            self.toast(trf("toast.fleet_down", &[&name]));
            return;
        }
        self.toast(trf("toast.on_machine", &[&name, &ask.starting()]));
        self.asked.push(Asked { id, peer: i, ask });
    }

    /// Show another machine's screens: card `card` of the fleet (0 is this
    /// one).
    pub fn enter_machine(&mut self, card: usize) {
        let Some(i) = card.checked_sub(1) else {
            self.go_home();
            return;
        };
        let Some(p) = self.peers.get(i) else { return };
        if p.link != Link::Up {
            self.toast(trf("toast.fleet_down", &[&p.name]));
            return;
        }
        let (name, user) = (p.name.clone(), p.hello.as_ref().map(|h| h.user.clone()).filter(|u| !u.is_empty()));
        let (home, home_views, home_me) = match self.away.take() {
            Some(a) => (a.home, a.home_views, a.home_me),
            None => (std::mem::take(&mut self.model), std::mem::replace(&mut self.views, new_views()), self.me.take()),
        };
        self.away = Some(Away { peer: i, home, home_views, home_me, lost: false });
        self.model = Model::default();
        self.views = new_views();
        self.me = user;
        self.fresh_lists();
        self.fleet = false;
        self.fleet_sel = card;
        self.sync_sources();
        self.toast(trf("toast.machine", &[&name]));
    }

    /// Back to this machine's screens, where they were left.
    pub fn go_home(&mut self) {
        let Some(a) = self.away.take() else { return };
        self.model = a.home;
        self.views = a.home_views;
        self.me = a.home_me;
        self.fresh_lists();
        self.fleet = false;
        self.fleet_sel = 0;
        self.sync_sources();
        self.toast(tr("toast.machine_home"));
    }

    /// The next (`d` > 0) or previous machine: this one, then the hosts that
    /// are up, round.
    fn cycle(&mut self, d: i32) {
        let cur = self.away.as_ref().map_or(0, |a| a.peer + 1);
        let stops: Vec<usize> =
            std::iter::once(0).chain((1..=self.peers.len()).filter(|&c| c == cur || self.peers[c - 1].link == Link::Up)).collect();
        if stops.len() < 2 {
            self.toast(tr("toast.machine_none"));
            return;
        }
        let at = stops.iter().position(|&c| c == cur).unwrap_or(0) as i32;
        let n = stops.len() as i32;
        self.enter_machine(stops[(at + d).rem_euclid(n) as usize]);
    }

    /// Lists, selections and the filter start over for another model.
    fn fresh_lists(&mut self) {
        self.filters.clear();
        if self.mode == Mode::Filter {
            self.mode = Mode::Normal;
        }
        self.proc_pid = None;
        self.svc_name = None;
        self.net_key = None;
        self.net_nic = None;
        self.dev_class = None;
        self.dev_id = None;
        self.ct_id = None;
        self.drag = None;
        self.stack_drag = None;
        self.pending.clear();
        self.rows.clear();
        self.tree_lines.clear();
        self.top_cpu.clear();
        self.top_mem.clear();
        self.svc_rows.clear();
        self.net_rows.clear();
        self.dev_rows.clear();
        self.dev_lines.clear();
        self.ct_rows.clear();
        self.dirty = true;
        self.refresh_svc();
        self.refresh_net();
        self.refresh_dev();
        self.refresh_ct();
    }

    /// Tell the samplers what the screen shown needs: services, sockets and
    /// devices only while their screen shows, and another machine's readings
    /// only while inside it.
    fn sync_sources(&mut self) {
        let (svc, net, dev, ct) =
            (self.screen == Screen::Svc, self.screen == Screen::Net, self.screen == Screen::Devices, self.screen == Screen::Containers);
        let away = self.away.as_ref().map(|a| a.peer);
        let local = if away.is_some() { (false, false, false, false) } else { (svc, net, dev, ct) };
        if local.0 != self.sent_local.0 {
            let _ = self.ctl.send(Ctl::Services(local.0));
        }
        if local.1 != self.sent_local.1 {
            let _ = self.ctl.send(Ctl::Net(local.1));
        }
        if local.2 != self.sent_local.2 {
            let _ = self.ctl.send(Ctl::Devices(local.2));
        }
        if local.3 != self.sent_local.3 {
            let _ = self.ctl.send(Ctl::Containers(local.3));
        }
        self.sent_local = local;
        for (i, tx) in self.fleet_tx.iter().enumerate() {
            let want = if away == Some(i) {
                Want { detail: true, services: svc, conns: net, devices: dev, containers: ct }
            } else {
                Want::default()
            };
            if self.sent_remote.get(i) != Some(&want) {
                let _ = tx.send(ToHost::Want(want));
                if let Some(w) = self.sent_remote.get_mut(i) {
                    *w = want;
                }
            }
        }
    }

    /// How long the toast has left in the status bar.
    /// While sys shows this machine: look for its agent when it is time,
    /// and say when the next look is due.
    pub fn probe_agent(&mut self) -> Option<Duration> {
        let p = &mut self.probe;
        while let Ok(r) = p.rx.try_recv() {
            (p.last, p.busy) = (Some(r), false);
        }
        if self.screen != Screen::Sys || self.away.is_some() || self.mode == Mode::Settings {
            return None;
        }
        let now = Instant::now();
        if !p.busy && p.next.is_none_or(|t| t <= now) {
            let (tx, wake, port) = (p.tx.clone(), self.wake.clone(), self.prefs.settings.port);
            let spawned = std::thread::Builder::new().name("yagura-probe".into()).spawn(move || {
                let _ = tx.send(client::probe(port));
                if let Some(w) = wake {
                    w.request_repaint();
                }
            });
            (p.busy, p.next) = (spawned.is_ok(), Some(now + PROBE_EVERY));
        }
        p.next.map(|t| t.saturating_duration_since(now))
    }

    pub fn toast_left(&self) -> Option<Duration> {
        self.shown_toast().map(|(_, left)| left)
    }

    /// The toast while it has time left, and how much.
    pub fn shown_toast(&self) -> Option<(&str, Duration)> {
        let (t, at) = self.toast.as_ref()?;
        let left = toast_time(t).saturating_sub(at.elapsed());
        (!left.is_zero()).then_some((t.as_str(), left))
    }

    pub fn proc_at(&self, i: usize) -> Option<&Proc> {
        self.model.procs.get(i)
    }

    /// The process the procs screen has selected.
    pub fn selected_proc(&self) -> Option<&Proc> {
        let sel = self.view_of(Screen::Procs).sel[1];
        self.rows.get(sel).and_then(|&i| self.proc_at(i))
    }

    /// The process under the cursor: procs' selection, or the row of a
    /// top list in the focused column.
    pub fn focused_proc(&self) -> Option<&Proc> {
        let sel = self.view().sel[self.focus().index()];
        let i = match (self.screen, self.focus()) {
            (Screen::Procs, _) => return self.selected_proc(),
            (Screen::Ovw, Pane::Right) => self.ovw_top(sel),
            (Screen::Cpu, Pane::Right) => self.top_cpu.get(sel).copied(),
            (Screen::Mem, Pane::Right) => self.top_mem.get(sel).copied(),
            (Screen::Gpu, Pane::Right) => {
                let pid = self.gpu_procs().get(sel).map(|g| g.pid);
                pid.and_then(|pid| self.model.procs.iter().position(|p| p.pid == pid))
            }
            _ => None,
        };
        i.and_then(|i| self.proc_at(i))
    }

    /// The device the gpu screen has selected.
    pub fn selected_gpu(&self) -> Option<&gpu::Device> {
        self.model.gpus.get(self.view_of(Screen::Gpu).sel[0])
    }

    /// The processes on the selected device.
    pub fn gpu_procs(&self) -> &[gpu::GpuProc] {
        self.selected_gpu().map_or(&[], |d| &d.procs)
    }

    /// The service the svc screen has selected.
    pub fn selected_svc(&self) -> Option<&svc::Service> {
        let sel = self.view_of(Screen::Svc).sel[1];
        self.svc_rows.get(sel).and_then(|&i| self.model.services.list.get(i))
    }

    /// The states svc's left column filters by, in its order.
    pub const SVC_SHOW: [Option<svc::State>; 4] = [None, Some(svc::State::Running), Some(svc::State::Stopped), Some(svc::State::Failed)];

    fn svc_matches(&self, s: &svc::Service, state: Option<svc::State>, f: &str) -> bool {
        s.driver == self.svc_drivers
            && state.is_none_or(|st| s.state == st)
            && (f.is_empty() || s.name.to_lowercase().contains(f) || s.display.to_lowercase().contains(f))
    }

    /// The services, or the drivers, svc shows: before the state and filter.
    pub fn svc_kind(&self) -> impl Iterator<Item = &svc::Service> {
        self.model.services.list.iter().filter(|s| s.driver == self.svc_drivers)
    }

    /// How many services a row of svc's left column would show.
    pub fn count_svc(&self, state: Option<svc::State>) -> usize {
        let f = self.filter_of(Screen::Svc);
        self.model.services.list.iter().filter(|s| self.svc_matches(s, state, &f)).count()
    }

    /// How many of the services, or the drivers, the filter keeps start this way.
    pub fn count_start(&self, start: svc::Start) -> usize {
        let f = self.filter_of(Screen::Svc);
        self.model.services.list.iter().filter(|s| s.start == start && self.svc_matches(s, None, &f)).count()
    }

    fn refresh_svc(&mut self) {
        let f = self.filter_of(Screen::Svc);
        let state = Self::SVC_SHOW[self.view_of(Screen::Svc).sel[0].min(3)];
        let list = &self.model.services.list;
        self.svc_rows = (0..list.len()).filter(|&i| self.svc_matches(&list[i], state, &f)).collect();
        let found = self.svc_name.as_ref().and_then(|n| self.svc_rows.iter().position(|&i| list[i].name == *n));
        if let Some(pos) = found {
            self.views.entry(Screen::Svc).or_default().sel[1] = pos;
        }
        self.clamp_all();
        self.svc_name = self.selected_svc().map(|s| s.name.clone());
    }

    /// The interface the net screen has selected.
    pub fn selected_nic(&self) -> Option<&net::Nic> {
        self.model.nics.get(self.view_of(Screen::Net).sel[0])
    }

    /// The socket the net screen has selected.
    pub fn selected_conn(&self) -> Option<&net::Conn> {
        let sel = self.view_of(Screen::Net).sel[1];
        self.net_rows.get(sel).and_then(|&i| self.model.conns.list.get(i))
    }

    /// The drive the disk screen has selected.
    pub fn selected_drive(&self) -> Option<&disk::Drive> {
        self.model.disks.drives.get(self.view_of(Screen::Disk).sel[0])
    }

    /// The volume the disk screen has selected.
    pub fn selected_volume(&self) -> Option<&disk::Volume> {
        self.model.disks.volumes.get(self.view_of(Screen::Disk).sel[1])
    }

    /// The chip the sensors screen has selected.
    pub fn selected_chip(&self) -> Option<sensors::Chip<'_>> {
        self.model.sensors.chips().into_iter().nth(self.view_of(Screen::Sensors).sel[0])
    }

    /// The sensor the sensors screen has selected.
    pub fn selected_sensor(&self) -> Option<&sensors::Sensor> {
        self.model.sensors.list.get(self.view_of(Screen::Sensors).sel[1])
    }

    /// The index of the chip the selected sensor belongs to.
    fn sensor_chip(&self) -> Option<usize> {
        let sel = self.view_of(Screen::Sensors).sel[Pane::Mid.index()];
        self.model.sensors.chips().iter().position(|c| (c.first..c.first + c.len).contains(&sel))
    }

    /// The memory slot the sys screen has selected.
    pub fn selected_slot(&self) -> Option<&hw::Slot> {
        self.model.hw.as_ref()?.slots.get(self.view_of(Screen::Sys).sel[1])
    }

    /// The name of the process with `pid`, if it is in the process list.
    pub fn proc_name(&self, pid: Option<u32>) -> Option<&str> {
        let pid = pid?;
        self.model.procs.iter().find(|p| p.pid == pid).map(|p| p.name.as_str())
    }

    fn conn_matches(&self, c: &net::Conn, f: &str) -> bool {
        if self.net_listen && !c.listening() {
            return false;
        }
        if let Some(name) = &self.net_nic
            && !net::on_nic(c, self.model.nics.iter().find(|n| &n.name == name))
        {
            return false;
        }
        if f.is_empty() {
            return true;
        }
        let local = c.local.to_string();
        let remote = c.remote.map(|r| r.to_string()).unwrap_or_default();
        let pid = c.pid.map(|p| p.to_string()).unwrap_or_default();
        let name = self.proc_name(c.pid).unwrap_or_default().to_lowercase();
        let state = tr(c.state.key()).to_lowercase();
        [c.proto.label(), &local, &remote, &pid, &name, &state].iter().any(|s| s.contains(f))
    }

    fn refresh_net(&mut self) {
        let f = self.filter_of(Screen::Net);
        let list = &self.model.conns.list;
        let rows: Vec<usize> = (0..list.len()).filter(|&i| self.conn_matches(&list[i], &f)).collect();
        let found = self.net_key.and_then(|k| rows.iter().position(|&i| conn_key(&list[i]) == k));
        self.net_rows = rows;
        if let Some(pos) = found {
            self.views.entry(Screen::Net).or_default().sel[1] = pos;
        }
        self.clamp_all();
        self.net_key = self.selected_conn().map(conn_key);
    }

    /// The devices read, or none yet.
    pub fn devices(&self) -> &[devices::Device] {
        self.model.devices.as_ref().map_or(&[], |d| &d.list)
    }

    /// The device the devices screen has selected.
    pub fn selected_dev(&self) -> Option<&devices::Device> {
        let sel = self.view_of(Screen::Devices).sel[1];
        self.dev_rows.get(sel).and_then(|&i| self.devices().get(i))
    }

    /// The class the devices screen's left column has selected; `None` is
    /// "all".
    pub fn selected_dev_class(&self) -> Option<&DevClass> {
        self.view_of(Screen::Devices).sel[0].checked_sub(1).and_then(|i| self.dev_classes.get(i))
    }

    fn refresh_dev(&mut self) {
        let f = self.filter_of(Screen::Devices);
        let list = self.model.devices.as_ref().map_or(&[][..], |d| &d.list);
        let mut classes: std::collections::BTreeMap<String, DevClass> = std::collections::BTreeMap::new();
        let mut class_of = Vec::with_capacity(list.len());
        for d in list {
            let name = d.class();
            let c = classes.entry(name.to_lowercase()).or_insert_with(|| DevClass { name: name.clone(), ..DevClass::default() });
            c.total += 1;
            c.shown += dev_matches(d, &f) as usize;
            c.problem |= d.status.is_problem();
            class_of.push(name);
        }
        self.dev_classes = classes.into_values().collect();
        // The same class stays selected while classes come and go.
        let at = match &self.dev_class {
            Some(n) => self.dev_classes.iter().position(|c| c.name == *n).map_or(0, |i| i + 1),
            None => 0,
        };
        self.views.entry(Screen::Devices).or_default().sel[0] = at;
        let class = at.checked_sub(1).map(|i| self.dev_classes[i].name.clone());
        let mut rows: Vec<usize> =
            (0..list.len()).filter(|&i| class.as_ref().is_none_or(|c| class_of[i] == *c) && dev_matches(&list[i], &f)).collect();
        // The tree keeps the names' order among siblings.
        self.dev_lines = if self.dev_tree {
            let t = devices::tree(list, &rows);
            rows = t.iter().map(|r| r.0).collect();
            t.into_iter().map(|r| r.1).collect()
        } else {
            Vec::new()
        };
        let found = self.dev_id.as_ref().and_then(|id| rows.iter().position(|&i| list[i].id == *id));
        self.dev_class = class;
        self.dev_rows = rows;
        if let Some(pos) = found {
            self.views.entry(Screen::Devices).or_default().sel[1] = pos;
        }
        self.clamp_all();
        self.dev_id = self.selected_dev().map(|d| d.id.clone());
    }

    /// The containers read, or none yet.
    pub fn containers(&self) -> &[containers::Container] {
        self.model.containers.as_ref().map_or(&[], |c| &c.list)
    }

    /// The container the containers screen has selected.
    pub fn selected_ct(&self) -> Option<&containers::Container> {
        let sel = self.view_of(Screen::Containers).sel[1];
        self.ct_rows.get(sel).and_then(|&i| self.containers().get(i))
    }

    /// The scope the containers screen's left column has selected.
    pub fn ct_scope(&self) -> CtScope {
        CtScope::ALL[self.view_of(Screen::Containers).sel[0].min(CtScope::ALL.len() - 1)]
    }

    fn refresh_ct(&mut self) {
        let f = self.filter_of(Screen::Containers);
        let scope = self.ct_scope();
        let list = self.model.containers.as_ref().map_or(&[][..], |c| &c.list);
        let rows: Vec<usize> = (0..list.len()).filter(|&i| scope.has(list[i].state) && ct_matches(&list[i], &f)).collect();
        let found = self.ct_id.as_ref().and_then(|id| rows.iter().position(|&i| list[i].id == *id));
        self.ct_rows = rows;
        if let Some(pos) = found {
            self.views.entry(Screen::Containers).or_default().sel[1] = pos;
        }
        self.clamp_all();
        self.ct_id = self.selected_ct().map(|c| c.id.clone());
    }

    /// The `/` text of this screen; empty where none is typed or the screen
    /// has no list to narrow.
    pub fn filter(&self) -> &str {
        self.filters.get(&self.screen).map_or("", String::as_str)
    }

    /// A screen's `/` text, lower-cased to match with.
    fn filter_of(&self, s: Screen) -> String {
        self.filters.get(&s).map_or_else(String::new, |f| f.to_lowercase())
    }

    /// Every list a filter narrows.
    fn refilter(&mut self) {
        self.refresh_rows();
        self.refresh_svc();
        self.refresh_net();
        self.refresh_dev();
        self.refresh_ct();
    }

    pub fn mine_only(&self) -> bool {
        self.view_of(Screen::Procs).sel[0] == 1
    }

    pub fn my_user(&self) -> Option<&str> {
        self.me.as_deref()
    }

    fn matches(&self, p: &Proc, mine: bool, f: &str) -> bool {
        if mine && (self.me.is_none() || p.user != self.me) {
            return false;
        }
        f.is_empty() || p.name.to_lowercase().contains(f) || p.pid.to_string().contains(f) || p.cmd.to_lowercase().contains(f)
    }

    /// The count of processes the procs list would show for a scope.
    pub fn count_scope(&self, mine: bool) -> usize {
        let f = self.filter_of(Screen::Procs);
        self.model.procs.iter().filter(|p| self.matches(p, mine, &f)).count()
    }

    fn refresh_rows(&mut self) {
        if self.me.is_none() {
            let me = std::process::id();
            self.me = self.model.procs.iter().find(|p| p.pid == me).and_then(|p| p.user.clone());
        }
        let f = self.filter_of(Screen::Procs);
        let mine = self.mine_only();
        let procs = &self.model.procs;
        let mut rows: Vec<usize> = (0..procs.len()).filter(|&i| self.matches(&procs[i], mine, &f)).collect();
        let key = |a: &Proc, b: &Proc| -> std::cmp::Ordering {
            let o = match self.sort {
                SortCol::Pid => a.pid.cmp(&b.pid),
                SortCol::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortCol::Cpu => b.cpu.total_cmp(&a.cpu),
                SortCol::Mem => b.mem.cmp(&a.mem),
                SortCol::Thr => b.threads.cmp(&a.threads),
                SortCol::User => a.user.cmp(&b.user),
            };
            o.then(a.pid.cmp(&b.pid))
        };
        rows.sort_by(|&a, &b| key(&procs[a], &procs[b]));
        // The tree keeps the sort among siblings.
        let lines = if self.tree {
            let t = crate::tree::order(procs, &rows);
            rows = t.iter().map(|r| r.0).collect();
            t.into_iter().map(|r| r.1).collect()
        } else {
            Vec::new()
        };
        let mut by_cpu: Vec<usize> = (0..procs.len()).collect();
        by_cpu.sort_by(|&a, &b| procs[b].cpu.total_cmp(&procs[a].cpu).then(procs[a].pid.cmp(&procs[b].pid)));
        by_cpu.truncate(TOP_CPU);
        let mut by_mem: Vec<usize> = (0..procs.len()).collect();
        by_mem.sort_by(|&a, &b| procs[b].mem.cmp(&procs[a].mem).then(procs[a].pid.cmp(&procs[b].pid)));
        by_mem.truncate(TOP_MEM);
        self.rows = rows;
        self.tree_lines = lines;
        self.top_cpu = by_cpu;
        self.top_mem = by_mem;

        // Keep the same process selected while the list reorders.
        let found = self.proc_pid.and_then(|pid| self.rows.iter().position(|&i| self.model.procs[i].pid == pid));
        let v = self.views.entry(Screen::Procs).or_default();
        if let Some(pos) = found {
            v.sel[1] = pos;
        }
        self.clamp_all();
        self.proc_pid = self.selected_proc().map(|p| p.pid);
    }

    /// Rows a column has to select, on a screen.
    pub fn count(&self, s: Screen, p: Pane) -> usize {
        let groups = self.model.st.as_ref().map_or(0, |st| st.topo.groups.len());
        let cores = self.model.st.as_ref().map_or(0, |st| st.topo.core_count());
        match (s, p) {
            (Screen::Ovw, Pane::Left) => 3 + self.model.gpus.len() + self.model.disks.drives.len(),
            (Screen::Ovw, Pane::Right) => self.top_cpu.len().min(OVW_TOP) + self.top_mem.len().min(OVW_TOP),
            (Screen::Cpu, Pane::Left) => 1 + groups,
            (Screen::Cpu, Pane::Mid) => cores,
            (Screen::Cpu, Pane::Right) => self.top_cpu.len(),
            (Screen::Mem, Pane::Left) => 3,
            (Screen::Mem, Pane::Right) => self.top_mem.len(),
            (Screen::Gpu, Pane::Left) => self.model.gpus.len(),
            (Screen::Gpu, Pane::Right) => self.gpu_procs().len(),
            (Screen::Procs, Pane::Left) => 2,
            (Screen::Procs, Pane::Mid) => self.rows.len(),
            (Screen::Svc, Pane::Left) => Self::SVC_SHOW.len(),
            (Screen::Svc, Pane::Mid) => self.svc_rows.len(),
            (Screen::Net, Pane::Left) => self.model.nics.len(),
            (Screen::Net, Pane::Mid) => self.net_rows.len(),
            (Screen::Disk, Pane::Left) => self.model.disks.drives.len(),
            (Screen::Disk, Pane::Mid) => self.model.disks.volumes.len(),
            (Screen::Sensors, Pane::Left) => self.model.sensors.chips().len(),
            (Screen::Sensors, Pane::Mid) => self.model.sensors.list.len(),
            (Screen::Sys, Pane::Left) => crate::ui::sys::rows(crate::ui::sys::lines(self), self.sys_room).len(),
            (Screen::Sys, Pane::Mid) => self.model.hw.as_ref().map_or(0, |h| h.slots.len()),
            (Screen::Devices, Pane::Left) => 1 + self.dev_classes.len(),
            (Screen::Devices, Pane::Mid) => self.dev_rows.len(),
            (Screen::Containers, Pane::Left) => CtScope::ALL.len(),
            (Screen::Containers, Pane::Mid) => self.ct_rows.len(),
            _ => 0,
        }
    }

    /// The overview's left column: CPU, memory and swap, then the GPUs and
    /// NPUs, then the drives.
    pub fn ovw_gauges(&self) -> Vec<Gauge> {
        let mut g = vec![Gauge::Cpu, Gauge::Mem, Gauge::Swap];
        g.extend((0..self.model.gpus.len()).map(Gauge::Gpu));
        g.extend((0..self.model.disks.drives.len()).map(Gauge::Disk));
        g
    }

    /// The overview's right column: the top CPU users, then the top memory.
    pub fn ovw_top(&self, i: usize) -> Option<usize> {
        let n = self.top_cpu.len().min(OVW_TOP);
        if i < n { self.top_cpu.get(i).copied() } else { self.top_mem.get(i - n).copied() }
    }

    fn clamp_all(&mut self) {
        let counts: Vec<(Screen, [usize; 3])> = Screen::ALL.iter().map(|&s| (s, Pane::ALL.map(|p| self.count(s, p)))).collect();
        for (s, c) in counts {
            let v = self.views.entry(s).or_default();
            for (sel, n) in v.sel.iter_mut().zip(c) {
                *sel = (*sel).min(n.saturating_sub(1));
            }
        }
    }

    /// Apply what drawing reported.
    pub fn apply_out(&mut self, mut out: Out, ctx: &egui::Context) {
        for p in 0..3 {
            if let Some(s) = out.scroll[p] {
                self.view_mut().scroll[p] = s;
            }
        }
        self.visible = out.visible.map(|v| v.max(1));
        if let Some(r) = out.sys_room {
            self.sys_room = r;
        }
        if let Some(m) = out.help_max {
            self.help_scroll = self.help_scroll.min(m);
        }
        if let Some(s) = out.tab {
            self.goto(s);
        }
        if out.fleet_tab {
            self.fleet = true;
        }
        if let Some(text) = out.copy.take() {
            self.copy(text);
        }
        if let Some(i) = out.cpu_click {
            self.toggle_cpu(i);
        }
        if let Some(c) = out.fleet_cols {
            self.fleet_cols = c.max(1);
        }
        if let Some((i, double)) = out.fleet_click {
            self.fleet_sel = i.min(self.peers.len());
            if double {
                self.run(Act::Enter);
            }
        }
        if let Some(m) = out.moved.take() {
            self.moved(m);
        }
        if let Some((p, m)) = out.stack_moved.take() {
            self.stack_moved(p, m);
        }
        if let Some(z) = out.zoom.take() {
            self.open_zoom(z);
        }
        if let Some((pane, i, double)) = out.click {
            let v = self.view_mut();
            v.focus = pane.index();
            v.sel[pane.index()] = i;
            self.selected_moved(pane);
            if double {
                self.run(Act::Enter);
            }
        }
        self.wheel(ctx, &out);
    }

    fn wheel(&mut self, ctx: &egui::Context, out: &Out) {
        let (delta, pos) = ctx.input(|i| {
            let mut d = 0.0;
            for e in &i.events {
                if let egui::Event::MouseWheel { unit, delta, .. } = e {
                    d += match unit {
                        egui::MouseWheelUnit::Line => delta.y * 3.0,
                        egui::MouseWheelUnit::Point => delta.y / crate::config::theme::ROW,
                        egui::MouseWheelUnit::Page => delta.y * 10.0,
                    };
                }
            }
            (d, i.pointer.hover_pos())
        });
        if delta == 0.0 || self.mode != Mode::Normal {
            return;
        }
        let Some(pos) = pos else { return };
        let Some(p) = (0..3).find(|&p| out.pane_rects[p].is_some_and(|r| r.contains(pos))) else { return };
        self.wheel_acc += delta;
        let rows = self.wheel_acc.trunc();
        if rows != 0.0 {
            self.wheel_acc -= rows;
            self.move_sel(Pane::ALL[p], -(rows as i64));
        }
    }

    fn move_sel(&mut self, pane: Pane, by: i64) {
        let n = self.count(self.screen, pane);
        if n == 0 {
            return;
        }
        let v = self.view_mut();
        let cur = v.sel[pane.index()] as i64;
        v.sel[pane.index()] = (cur + by).clamp(0, n as i64 - 1) as usize;
        self.selected_moved(pane);
    }

    /// Follow-ups of a new selection: procs remembers the process, the
    /// procs scope re-filters.
    fn selected_moved(&mut self, pane: Pane) {
        match (self.screen, pane) {
            (Screen::Procs, Pane::Mid) => self.proc_pid = self.selected_proc().map(|p| p.pid),
            (Screen::Procs, Pane::Left) => self.refresh_rows(),
            (Screen::Svc, Pane::Mid) => self.svc_name = self.selected_svc().map(|s| s.name.clone()),
            (Screen::Svc, Pane::Left) => self.refresh_svc(),
            (Screen::Net, Pane::Mid) => self.net_key = self.selected_conn().map(conn_key),
            (Screen::Devices, Pane::Mid) => self.dev_id = self.selected_dev().map(|d| d.id.clone()),
            (Screen::Containers, Pane::Mid) => self.ct_id = self.selected_ct().map(|c| c.id.clone()),
            (Screen::Containers, Pane::Left) => {
                self.views.entry(Screen::Containers).or_default().sel[1] = 0;
                self.ct_id = None;
                self.refresh_ct();
            }
            (Screen::Devices, Pane::Left) => {
                self.dev_class = self.selected_dev_class().map(|c| c.name.clone());
                self.views.entry(Screen::Devices).or_default().sel[1] = 0;
                self.dev_id = None;
                self.refresh_dev();
            }
            // The chip and its sensors are selected together.
            (Screen::Sensors, Pane::Left) => {
                if let Some(first) = self.selected_chip().map(|c| c.first) {
                    self.view_mut().sel[Pane::Mid.index()] = first;
                }
            }
            (Screen::Sensors, Pane::Mid) => {
                if let Some(i) = self.sensor_chip() {
                    self.view_mut().sel[Pane::Left.index()] = i;
                }
            }
            (Screen::Net, Pane::Left) if self.net_nic.is_some() => {
                let said = self.net_nic.as_deref().map(|n| trf("toast.net_nic", &[n]));
                self.net_nic = self.selected_nic().map(|n| n.name.clone());
                self.refresh_net();
                // The toast that named the last interface names this one.
                if let Some(name) = self.net_nic.clone()
                    && self.shown_toast().is_some_and(|(t, _)| Some(t) == said.as_deref())
                {
                    self.toast(trf("toast.net_nic", &[&name]));
                }
            }
            _ => {}
        }
    }

    /// The sampling interval in seconds.
    pub fn interval(&self) -> f32 {
        self.interval
    }

    pub fn goto(&mut self, s: Screen) {
        // Typing goes on in the screen it began in.
        if self.mode == Mode::Filter && s != self.screen {
            self.mode = Mode::Normal;
        }
        self.screen = s;
        self.drag = None;
        self.stack_drag = None;
        self.pending.clear();
        self.fleet = false;
        self.sync_sources();
    }

    /// One key, through the layer of the current mode.
    pub fn feed(&mut self, k: Key) {
        match self.mode {
            Mode::Help => {
                if let Match::Exact(b) = resolve(&self.km.help, &[k]) {
                    for a in b.run.clone() {
                        self.run_help(a);
                    }
                }
            }
            // The palette's own box takes the keys from egui (`main.rs`).
            Mode::Palette => {}
            Mode::Confirm => {
                if let Match::Exact(b) = resolve(&self.km.confirm, &[k]) {
                    for a in b.run.clone() {
                        self.run_confirm(a);
                    }
                }
            }
            Mode::Zoom => {
                if let Match::Exact(b) = resolve(&self.km.zoom, &[k]) {
                    for a in b.run.clone() {
                        self.run_zoom(a);
                    }
                }
            }
            // The window takes the keys from egui itself (`main.rs`), since
            // the controls want the rest; this is the keys alone.
            Mode::Settings => {
                if let Some(Act::Escape | Act::Settings) = self.settings_act(k) {
                    self.close_settings();
                }
            }
            // The CPUs' line takes the input layer's keys, and digits, commas,
            // dashes and spaces as typed.
            Mode::Affinity => match resolve(&self.km.input, &[k]) {
                Match::Exact(b) => {
                    for a in b.run.clone() {
                        self.run_affinity(a);
                    }
                }
                _ => {
                    if let (Code::Char(c @ ('0'..='9' | ',' | '-' | ' ')), false) = (k.code, k.ctrl || k.alt || k.sup)
                        && let Some(e) = &mut self.affinity
                        && e.ids.is_some()
                    {
                        e.text.push(c);
                    }
                }
            },
            Mode::Filter => match resolve(&self.km.input, &[k]) {
                Match::Exact(b) => {
                    for a in b.run.clone() {
                        self.run(a);
                    }
                }
                _ => {
                    if let (Code::Char(c), false) = (k.code, k.ctrl || k.alt || k.sup) {
                        self.filters.entry(self.screen).or_default().push(c);
                        self.refilter();
                    }
                }
            },
            Mode::Normal => {
                self.pending.push(k);
                match resolve(&self.km.main, &self.pending) {
                    Match::Exact(b) => {
                        let run = b.run.clone();
                        self.pending.clear();
                        for a in run {
                            self.run(a);
                        }
                    }
                    Match::Pending(_) => {}
                    Match::None => {
                        let again = self.pending.len() > 1;
                        self.pending.clear();
                        if again {
                            self.feed(k);
                        }
                    }
                }
            }
        }
    }

    fn run_help(&mut self, a: Act) {
        match a {
            Act::Escape | Act::Help | Act::Quit => self.mode = Mode::Normal,
            Act::Scale(to) => self.scale(to),
            Act::Arrow(Step::By(n)) => self.help_scroll = (self.help_scroll as i64 + n).max(0) as usize,
            Act::Arrow(Step::Top) => self.help_scroll = 0,
            _ => {}
        }
    }

    fn run_zoom(&mut self, a: Act) {
        match a {
            Act::Escape | Act::Enter | Act::Quit => self.close_zoom(),
            Act::Page(by) => {
                if let Some(z) = &mut self.zoom {
                    z.range = z.range.step(by);
                    self.zoom_range = z.range;
                }
            }
            Act::Scale(to) => self.scale(to),
            _ => {}
        }
    }

    fn close_zoom(&mut self) {
        self.zoom = None;
        if self.mode == Mode::Zoom {
            self.mode = Mode::Normal;
        }
    }

    /// Open `z` full size, over the range the last one looked back.
    pub fn open_zoom(&mut self, mut z: Zoom) {
        if self.mode != Mode::Normal {
            return;
        }
        z.range = self.zoom_range;
        self.zoom = Some(z);
        self.mode = Mode::Zoom;
    }

    fn run_confirm(&mut self, a: Act) {
        match a {
            Act::Confirm | Act::Enter => {
                self.mode = Mode::Normal;
                let Some(ask) = self.confirm.take() else { return };
                if self.away.is_some() {
                    self.send_act(ask);
                    return;
                }
                self.toast(ask.starting());
                match ask {
                    Ask::Kill(t) => kill::spawn(t, self.kill_tx.clone(), self.wake.clone()),
                    Ask::Svc(t) => svcctl::spawn(t, self.svc_tx.clone(), self.wake.clone()),
                    Ask::Ct(t) => {
                        self.ct_busy.insert(t.key().to_string());
                        containers::act::spawn(t, self.ct_tx.clone(), self.wake.clone())
                    }
                }
            }
            Act::Escape | Act::Quit => {
                self.mode = Mode::Normal;
                self.confirm = None;
            }
            _ => {}
        }
    }

    /// Ask before ending the process under the cursor.
    fn ask_kill(&mut self) {
        let Some(p) = self.focused_proc() else {
            self.toast(tr("toast.kill_none"));
            return;
        };
        let t = Target { pid: p.pid, name: p.name.clone(), start: p.start };
        // Inside another machine, its agent is the one not to end.
        match self.away_peer() {
            Some(m) if m.hello.as_ref().is_some_and(|h| h.pid == t.pid) => {
                let s = trf("toast.kill_agent", &[&m.name]);
                self.toast(s);
                return;
            }
            None if t.pid == std::process::id() => {
                self.toast(tr("toast.kill_self"));
                return;
            }
            _ => {}
        }
        self.confirm = Some(Ask::Kill(t));
        self.mode = Mode::Confirm;
    }

    /// Step the priority of the process under the cursor, on this machine only.
    fn step_priority(&mut self, raise: bool) {
        if self.screen != Screen::Procs {
            return;
        }
        if let Some(m) = self.away_peer() {
            let s = trf("toast.prio_remote", &[&m.name]);
            self.toast(s);
            return;
        }
        let Some(p) = self.focused_proc() else {
            self.toast(tr("toast.kill_none"));
            return;
        };
        let t = Target { pid: p.pid, name: p.name.clone(), start: p.start };
        crate::renice::spawn(t, raise, self.renice_tx.clone(), self.wake.clone());
    }

    /// Open the editor for the CPUs the process under the cursor may run on,
    /// starting from the ones it has now, read on a thread of their own with
    /// the CPUs there are; this machine's processes only.
    fn open_affinity(&mut self) {
        if self.screen != Screen::Procs {
            return;
        }
        if !crate::affinity::SUPPORTED {
            self.toast(tr("toast.aff_unsupported"));
            return;
        }
        if let Some(m) = self.away_peer() {
            let s = trf("toast.aff_remote", &[&m.name]);
            self.toast(s);
            return;
        }
        let Some(p) = self.focused_proc() else {
            self.toast(tr("toast.kill_none"));
            return;
        };
        let t = Target { pid: p.pid, name: p.name.clone(), start: p.start };
        if let Err(e) = crate::affinity::look_spawn(t.pid, self.look_tx.clone(), self.wake.clone()) {
            self.toast(aff_toast(&t, crate::affinity::Outcome::Failed(e.to_string())));
            return;
        }
        self.affinity = Some(AffEdit { t, text: String::new(), ids: None, now: None });
        self.pending.clear();
        self.mode = Mode::Affinity;
    }

    fn close_affinity(&mut self) {
        self.affinity = None;
        self.mode = Mode::Normal;
    }

    fn run_affinity(&mut self, a: Act) {
        let read = self.affinity.as_ref().is_some_and(|e| e.ids.is_some());
        match a {
            Act::Escape | Act::Quit => self.close_affinity(),
            _ if !read => {}
            Act::Backspace => {
                if let Some(e) = &mut self.affinity {
                    e.text.pop();
                }
            }
            Act::Confirm | Act::Enter => {
                let Some(e) = &self.affinity else { return };
                let Some(ids) = &e.ids else { return };
                // A line that does not read stays open, its fault shown under it.
                if let Ok(cpus) = crate::affinity::parse(&e.text, ids) {
                    crate::affinity::spawn(e.t.clone(), cpus, self.aff_tx.clone(), self.wake.clone());
                    self.close_affinity();
                }
            }
            _ => {}
        }
    }

    /// A CPU box clicked: in or out of the set, and the line written again.
    fn toggle_cpu(&mut self, i: usize) {
        let Some(e) = &mut self.affinity else { return };
        let Some(ids) = &e.ids else { return };
        if ids.binary_search(&i).is_err() {
            return;
        }
        let mut set = crate::affinity::lenient(&e.text, ids);
        match set.binary_search(&i) {
            Ok(at) => {
                set.remove(at);
            }
            Err(at) => set.insert(at, i),
        }
        e.text = crate::procinfo::ranges(&set);
    }

    /// Ask before starting, stopping or restarting the service under the
    /// cursor; one already where it would go only gets a toast.
    fn ask_svc(&mut self, action: Action) {
        let Some(s) = self.selected_svc() else {
            self.toast(tr("toast.svc_none"));
            return;
        };
        let name = s.name.clone();
        if !svcctl::fits(action, s.state) {
            let key = if action == Action::Start { "toast.svc_already_running" } else { "toast.svc_not_running" };
            self.toast(trf(key, &[&name]));
            return;
        }
        // A launchd job that is not loaded: loaded from its plist, unless it
        // is disabled, which is the owner's to undo. Another machine's agent
        // decides that from its own list.
        let mut plist = None;
        if s.unloaded && action != Action::Stop && self.away.is_none() {
            if s.start == svc::Start::Disabled {
                self.toast(trf("toast.svc_disabled", &[&name, &svcctl::enable_line(&name)]));
                return;
            }
            plist = s.file.clone();
        }
        let driver = s.driver;
        self.confirm = Some(Ask::Svc(svcctl::Target { action, name, plist, driver }));
        self.mode = Mode::Confirm;
    }

    /// Ask before starting, stopping or restarting the container under the
    /// cursor, on this machine only; one already where it would go, or a
    /// paused one asked to start, only gets a toast.
    fn ask_ct(&mut self, action: Action) {
        if let Some(p) = self.away_peer() {
            let s = trf("toast.ct_remote", &[&p.name]);
            self.toast(s);
            return;
        }
        let Some(c) = self.selected_ct() else {
            self.toast(tr("ct.none"));
            return;
        };
        let state = c.state;
        let name = if c.name.is_empty() { c.id.clone() } else { c.name.clone() };
        let list = self.model.containers.as_ref();
        let runtime = list.and_then(|l| l.runtime).unwrap_or_default();
        let engine = list.and_then(|l| l.engine.clone());
        let t = containers::act::Target { action, id: c.id.clone(), name, runtime, engine };
        if self.ct_busy.contains(t.key()) {
            self.toast(trf("toast.ct_busy", &[&t.name]));
            return;
        }
        if state == containers::State::Paused && action == Action::Start {
            self.toast(trf("toast.ct_paused", &[&t.name, &containers::act::unpause(&t)]));
            return;
        }
        if !containers::act::fits(action, state) {
            let key = if action == Action::Start { "toast.ct_already_running" } else { "toast.ct_not_running" };
            self.toast(trf(key, &[&t.name]));
            return;
        }
        self.confirm = Some(Ask::Ct(t));
        self.mode = Mode::Confirm;
    }

    /// The keys while the fleet shows; false for those that work as on
    /// any screen.
    fn run_fleet(&mut self, a: Act) -> bool {
        let last = self.peers.len() as i64;
        let (sel, cols) = (self.fleet_sel as i64, self.fleet_cols.max(1) as i64);
        let to = |i: i64| i.clamp(0, last) as usize;
        match a {
            Act::Arrow(Step::By(d)) => self.fleet_sel = to(sel + d * cols),
            Act::Arrow(Step::Page(f)) => self.fleet_sel = to(sel + if f < 0.0 { -cols } else { cols }),
            Act::Arrow(Step::Top) => self.fleet_sel = 0,
            Act::Arrow(Step::Bot) => self.fleet_sel = last as usize,
            Act::Focus(d) => self.fleet_sel = to(sel + d as i64),
            // The screens behind, when they are the card's machine's.
            Act::Enter if self.fleet_sel == self.away.as_ref().map_or(0, |a| a.peer + 1) => self.fleet = false,
            Act::Enter => self.enter_machine(self.fleet_sel),
            Act::Fleet | Act::Escape => self.fleet = false,
            Act::Copy => match self.selected_peer() {
                None if self.peers.is_empty() => self.copy(remote::host_sample(self.prefs.settings.port)),
                None => self.toast(tr("toast.copy_none")),
                Some(p) => {
                    let addr = p.addr.clone();
                    self.copy(addr);
                }
            },
            Act::Kill | Act::Service(_) => {
                let what = if a == Act::Kill { "kill" } else { "service" };
                let t = match self.selected_peer() {
                    None => tr("toast.fleet_local").to_string(),
                    Some(p) if p.hello.as_ref().is_some_and(|h| proto::allowed(&h.allow, what)) => {
                        trf("toast.fleet_act_inside", &[&p.name])
                    }
                    Some(p) => trf("toast.fleet_view_only", &[&p.name, what]),
                };
                self.toast(t);
            }
            Act::Sort | Act::Tree | Act::Scope(_) | Act::Resize(_) | Act::Equalize | Act::Backspace | Act::Priority(_) | Act::Affinity => {}
            // The fleet's tab is the last one, so next is the first screen and
            // previous the last built one.
            Act::Screen(g @ (Goto::Next | Goto::Prev)) => {
                let mut built = Screen::ALL.iter().copied().filter(|s| s.built());
                if let Some(s) = if g == Goto::Next { built.next() } else { built.next_back() } {
                    self.goto(s);
                }
            }
            Act::Screen(_) | Act::Filter => {
                self.fleet = false;
                return false;
            }
            _ => return false,
        }
        true
    }

    pub fn run(&mut self, a: Act) {
        if self.fleet && self.run_fleet(a) {
            return;
        }
        let built = self.screen.built();
        let away = self.away_peer().map(|p| p.name.clone());
        // Ending a process or moving a service on another machine, which its
        // `[agent] allow` may not let.
        let refused = self.away_peer().and_then(|p| {
            let word = match a {
                // Containers are moved on this machine only, which `ask_ct` says.
                Act::Kill | Act::Service(_) if self.screen == Screen::Containers => return None,
                Act::Kill if self.screen != Screen::Svc => "kill",
                Act::Kill | Act::Service(_) if self.screen == Screen::Svc => "service",
                _ => return None,
            };
            let allowed = p.hello.as_ref().is_some_and(|h| proto::allowed(&h.allow, word));
            (!allowed).then(|| trf("toast.fleet_view_only", &[&p.name, word]))
        });
        match a {
            Act::Machine(d) => self.cycle(d),
            Act::Kill | Act::Service(_) if refused.is_some() => self.toast(refused.unwrap_or_default()),
            // Its sampling is its own.
            Act::Pause | Act::Interval(_) if away.is_some() => self.toast(trf("toast.remote_sampling", &[&away.unwrap_or_default()])),
            Act::Fleet => {
                self.fleet = true;
                self.fleet_sel = self.fleet_sel.min(self.peers.len());
            }
            // The fleet is the tab after the last screen: Tab wraps through it.
            Act::Screen(g @ (Goto::Next | Goto::Prev))
                if {
                    let mut built = Screen::ALL.iter().copied().filter(|s| s.built());
                    self.screen == if g == Goto::Next { built.next_back() } else { built.next() }.unwrap_or(self.screen)
                } =>
            {
                self.goto(self.screen);
                self.fleet = true;
                self.fleet_sel = self.fleet_sel.min(self.peers.len());
            }
            Act::Screen(g) => {
                let s = match g {
                    Goto::To(s) => s,
                    Goto::Next | Goto::Prev => {
                        let step = if g == Goto::Next { 1 } else { Screen::ALL.len() - 1 };
                        let mut i = self.screen.index();
                        loop {
                            i = (i + step) % Screen::ALL.len();
                            if Screen::ALL[i].built() {
                                break Screen::ALL[i];
                            }
                        }
                    }
                };
                self.goto(s);
            }
            Act::Arrow(step) if built => {
                let pane = self.focus();
                let n = self.count(self.screen, pane) as i64;
                let cur = self.view().sel[pane.index()] as i64;
                let to = match step {
                    Step::By(d) => cur + d,
                    Step::Top => 0,
                    Step::Bot => n - 1,
                    Step::Page(f) => cur + (self.visible[pane.index()] as f32 * f).round() as i64,
                };
                self.move_sel(pane, to - cur);
            }
            Act::Focus(d) if built => {
                let v = self.view_mut();
                v.focus = (v.focus as i32 + d).clamp(0, 2) as usize;
            }
            Act::Resize(t) if built => {
                let focus = self.focus();
                let l = self.layouts.get_mut(&self.screen).expect("every screen has a layout");
                if l.nudge(&focus, t, NUDGE) {
                    self.save_layouts();
                } else {
                    self.toast(tr("toast.no_divider"));
                }
            }
            Act::Equalize if built => {
                if let Some(l) = self.layouts.get_mut(&self.screen) {
                    l.equalize();
                }
                let screen = self.screen;
                self.stacks.retain(|&(s, _), _| s != screen);
                self.save_layouts();
            }
            Act::Enter if built => self.enter(),
            Act::Filter => {
                // svc, net, devices and containers filter their own lists; anywhere else
                // it is procs'.
                if !matches!(self.screen, Screen::Svc | Screen::Net | Screen::Devices | Screen::Containers) {
                    self.goto(Screen::Procs);
                }
                self.view_mut().focus = Pane::Mid.index();
                self.mode = Mode::Filter;
            }
            Act::Kill if self.screen == Screen::Svc => self.ask_svc(Action::Stop),
            Act::Kill if self.screen == Screen::Containers => self.ask_ct(Action::Stop),
            Act::Kill => self.ask_kill(),
            Act::Priority(raise) => self.step_priority(raise),
            Act::Affinity => self.open_affinity(),
            Act::Service(action) if self.screen == Screen::Svc => self.ask_svc(action),
            Act::Service(action) if self.screen == Screen::Containers => self.ask_ct(action),
            Act::Service(_) => self.toast(tr("toast.svc_elsewhere")),
            Act::Sort => {
                self.sort = self.sort.next();
                self.refresh_rows();
                self.toast(trf("toast.sort", &[self.sort.label()]));
            }
            Act::Tree if self.screen == Screen::Devices => {
                self.dev_tree = !self.dev_tree;
                self.refresh_dev();
                self.toast(tr(if self.dev_tree { "toast.dev_tree_on" } else { "toast.tree_off" }));
            }
            Act::Tree => {
                self.tree = !self.tree;
                self.refresh_rows();
                self.toast(tr(if self.tree { "toast.tree_on" } else { "toast.tree_off" }));
            }
            Act::Scope(to) if self.screen == Screen::Net => {
                self.net_listen = to.unwrap_or(!self.net_listen);
                self.refresh_net();
                self.toast(trf("toast.scope", &[tr(if self.net_listen { "net.listening" } else { "net.all_conns" })]));
            }
            Act::Scope(to) if self.screen == Screen::Svc => {
                let drivers = to.unwrap_or(!self.svc_drivers);
                if drivers && !self.model.services.list.iter().any(|s| s.driver) {
                    self.toast(tr("toast.svc_no_drivers"));
                    return;
                }
                self.svc_drivers = drivers;
                self.views.entry(Screen::Svc).or_default().sel[1] = 0;
                self.refresh_svc();
                self.toast(trf("toast.scope", &[tr(if drivers { "svc.drivers" } else { "svc.services" })]));
            }
            Act::Scope(to) => {
                let mine = to.unwrap_or(!self.mine_only());
                self.views.entry(Screen::Procs).or_default().sel[0] = mine as usize;
                self.refresh_rows();
                self.toast(trf("toast.scope", &[tr(if mine { "procs.mine" } else { "procs.all" })]));
            }
            Act::Pause => {
                self.paused = !self.paused;
                let _ = self.ctl.send(Ctl::Pause(self.paused));
                self.toast(tr(if self.paused { "toast.paused" } else { "toast.resumed" }));
            }
            Act::Interval(d) => {
                let cur = INTERVALS.iter().position(|&i| i >= self.interval).unwrap_or(INTERVALS.len() - 1) as i32;
                let i = (cur + d).clamp(0, INTERVALS.len() as i32 - 1) as usize;
                self.set_interval(INTERVALS[i]);
                self.toast(trf("toast.interval", &[&fmt_interval(self.interval)]));
            }
            Act::Help => {
                self.mode = Mode::Help;
                self.help_scroll = 0;
            }
            Act::BugReport => self.bug_report(),
            Act::Settings => self.open_settings(),
            Act::Palette => {
                self.palette = ito_palette::View::new();
                self.pending.clear();
                self.mode = Mode::Palette;
            }
            Act::Scale(to) => self.scale(to),
            Act::Copy if self.screen == Screen::Devices => match self.selected_dev().map(|d| d.id.clone()) {
                Some(id) => self.copy(id),
                None => self.toast(tr("toast.copy_none")),
            },
            Act::Copy if self.screen == Screen::Containers => match self.selected_ct().map(|c| c.id.clone()) {
                Some(id) => self.copy(id),
                None => self.toast(tr("toast.copy_none")),
            },
            Act::Copy => self.toast(tr("toast.copy_none")),
            Act::Quit => self.quit = true,
            Act::Escape => {
                let nic = self.screen == Screen::Net && self.net_nic.take().is_some();
                if self.mode == Mode::Filter || !self.filter().is_empty() || nic {
                    self.filters.remove(&self.screen);
                    self.refilter();
                }
                self.mode = Mode::Normal;
            }
            Act::Confirm => self.mode = Mode::Normal,
            Act::Backspace if self.filters.get_mut(&self.screen).and_then(String::pop).is_some() => self.refilter(),
            // Arrow, Focus, Resize, Equalize and Enter on a screen not built yet.
            _ => {}
        }
    }

    fn enter(&mut self) {
        let v = self.view();
        let sel = v.sel[v.focus.min(2)];
        match (self.screen, self.focus()) {
            (Screen::Ovw, Pane::Left) => {
                let (s, row) = match self.ovw_gauges().get(sel) {
                    Some(Gauge::Cpu) | None => (Screen::Cpu, None),
                    Some(Gauge::Mem | Gauge::Swap) => (Screen::Mem, None),
                    Some(&Gauge::Gpu(i)) => (Screen::Gpu, Some(i)),
                    Some(&Gauge::Disk(i)) => (Screen::Disk, Some(i)),
                };
                self.goto(s);
                if let Some(i) = row {
                    let v = self.view_mut();
                    v.focus = Pane::Left.index();
                    v.sel[Pane::Left.index()] = i;
                }
            }
            (Screen::Ovw, Pane::Right) => {
                if let Some(i) = self.ovw_top(sel) {
                    self.open_proc(self.model.procs[i].pid);
                }
            }
            (Screen::Cpu, Pane::Left) => {
                let first = match (sel, &self.model.st) {
                    (0, _) | (_, None) => 0,
                    (g, Some(st)) => st.topo.groups.iter().take(g - 1).map(|g| g.cores.len()).sum(),
                };
                let v = self.view_mut();
                v.focus = Pane::Mid.index();
                v.sel[Pane::Mid.index()] = first;
            }
            (Screen::Cpu, Pane::Right) => {
                if let Some(&i) = self.top_cpu.get(sel) {
                    self.open_proc(self.model.procs[i].pid);
                }
            }
            (Screen::Mem, Pane::Right) => {
                if let Some(&i) = self.top_mem.get(sel) {
                    self.open_proc(self.model.procs[i].pid);
                }
            }
            (Screen::Gpu, Pane::Left) if !self.gpu_procs().is_empty() => self.view_mut().focus = Pane::Right.index(),
            (Screen::Gpu, Pane::Right) => {
                if let Some(pid) = self.gpu_procs().get(sel).map(|g| g.pid) {
                    self.open_proc(pid);
                }
            }
            (Screen::Procs, Pane::Left) => self.view_mut().focus = Pane::Mid.index(),
            (Screen::Svc, Pane::Left) => self.view_mut().focus = Pane::Mid.index(),
            (Screen::Devices, Pane::Left) => self.view_mut().focus = Pane::Mid.index(),
            (Screen::Containers, Pane::Left) => self.view_mut().focus = Pane::Mid.index(),
            (Screen::Svc, Pane::Mid | Pane::Right) => match self.selected_svc().map(|s| (s.pid, s.name.clone())) {
                Some((Some(pid), _)) => self.open_proc(pid),
                Some((None, name)) => self.toast(trf("toast.svc_no_pid", &[&name])),
                None => {}
            },
            (Screen::Net, Pane::Left) => {
                if let Some(name) = self.selected_nic().map(|n| n.name.clone()) {
                    self.toast(trf("toast.net_nic", &[&name]));
                    self.net_nic = Some(name);
                    self.refresh_net();
                }
                self.view_mut().focus = Pane::Mid.index();
            }
            (Screen::Net, Pane::Mid | Pane::Right) => match self.selected_conn().map(|c| (c.pid, c.local)) {
                Some((Some(pid), _)) => self.open_proc(pid),
                Some((None, local)) => self.toast(trf("toast.net_no_pid", &[&local.to_string()])),
                None => {}
            },
            // A drive: over to its first volume. A volume: back to its drive.
            (Screen::Disk, Pane::Left) => {
                let d = &self.model.disks;
                let id = self.selected_drive().map(|x| x.id.as_str());
                let first = d.volumes.iter().position(|v| v.drive.is_some() && v.drive.as_deref() == id);
                let v = self.view_mut();
                if let Some(i) = first {
                    v.sel[Pane::Mid.index()] = i;
                }
                v.focus = Pane::Mid.index();
            }
            (Screen::Disk, Pane::Mid) => {
                let d = &self.model.disks;
                let id = self.selected_volume().and_then(|v| v.drive.as_deref());
                if let Some(i) = d.drives.iter().position(|x| Some(x.id.as_str()) == id) {
                    let v = self.view_mut();
                    v.sel[Pane::Left.index()] = i;
                    v.focus = Pane::Left.index();
                }
            }
            // A chip: over to its first sensor. A sensor: back to its chip.
            (Screen::Sensors, Pane::Left) => {
                let first = self.selected_chip().map(|c| c.first);
                let v = self.view_mut();
                if let Some(i) = first {
                    v.sel[Pane::Mid.index()] = i;
                }
                v.focus = Pane::Mid.index();
            }
            (Screen::Sensors, Pane::Mid) => {
                if let Some(i) = self.sensor_chip() {
                    let v = self.view_mut();
                    v.sel[Pane::Left.index()] = i;
                    v.focus = Pane::Left.index();
                }
            }
            _ => {}
        }
    }

    /// Procs with `pid` selected, the scope and filter widened if they hid it.
    fn open_proc(&mut self, pid: u32) {
        self.goto(Screen::Procs);
        let v = self.view_mut();
        v.focus = Pane::Mid.index();
        v.sel[Pane::Left.index()] = 0;
        let f = self.filter_of(Screen::Procs);
        if self.model.procs.iter().any(|p| p.pid == pid && !self.matches(p, false, &f)) {
            self.filters.remove(&Screen::Procs);
        }
        self.proc_pid = Some(pid);
        self.refresh_rows();
    }

    /// A divider dragged or double-clicked with the mouse.
    pub fn moved(&mut self, m: ito_layout::ui::Moved<Pane>) {
        use ito_layout::ui::Moved;
        match m {
            Moved::Dragging(l) => self.drag = Some(l),
            Moved::Released => {
                if let Some(l) = self.drag.take() {
                    self.layouts.insert(self.screen, l);
                    self.save_layouts();
                }
            }
            Moved::Halved(l) => {
                self.drag = None;
                self.layouts.insert(self.screen, l);
                self.save_layouts();
            }
        }
    }

    /// A divider between the panels of a column dragged or double-clicked.
    pub fn stack_moved(&mut self, pane: Pane, m: ito_layout::ui::Moved<u8>) {
        use ito_layout::ui::Moved;
        match m {
            Moved::Dragging(n) => self.stack_drag = Some((self.screen, pane, n)),
            Moved::Released => {
                if let Some((s, p, n)) = self.stack_drag.take() {
                    self.stacks.insert((s, p), n);
                    self.save_layouts();
                }
            }
            Moved::Halved(n) => {
                self.stack_drag = None;
                self.stacks.insert((self.screen, pane), n);
                self.save_layouts();
            }
        }
    }

    /// The keys of the current state, for a scripted run's report.
    pub fn report(&self) -> String {
        let v = self.view();
        let mut s = String::new();
        s += &format!("screen: {}\n", if self.fleet { "fleet" } else { self.screen.name() });
        match (&self.away, self.away_peer()) {
            (Some(a), Some(p)) => s += &format!("machine: {}{}\n", p.name, if a.lost { " (lost)" } else { "" }),
            _ => s += "machine: this\n",
        }
        if self.fleet {
            let now = Instant::now();
            let hosts: Vec<String> = self.peers.iter().map(|p| format!("{}={}", p.name, p.status(now).key())).collect();
            s += &format!("fleet: sel {} of {}; {}\n", self.fleet_sel, self.peers.len() + 1, hosts.join(" "));
        }
        s += &format!("mode: {:?}\n", self.mode);
        if self.mode == Mode::Settings {
            s += &format!("settings: page {}, theme {}\n", self.prefs.state.page, self.prefs.common.theme_choice(None).0);
        }
        s += &format!("focus: {}\n", self.focus().name());
        s += &format!("sel: {:?}\n", v.sel);
        s += &format!("filter: {}\n", self.filter());
        s += &format!("sort: {:?}\n", self.sort);
        s += &format!("tree: {}\n", self.tree);
        s += &format!("scope: {}\n", if self.mine_only() { "mine" } else { "all" });
        s += &format!("interval: {}\n", self.interval);
        s += &format!("paused: {}\n", self.paused);
        s += &format!("samples: {}\n", self.model.samples);
        s += &format!("procs: {}\n", self.rows.len());
        let gpus: Vec<&str> = self.model.gpus.iter().map(|d| d.id.as_str()).collect();
        s += &format!("gpus: {} ({})\n", gpus.len(), gpus.join(" "));
        if self.model.svc_read {
            let sv = &self.model.services;
            let kind = if self.svc_drivers { "drivers" } else { "services" };
            s += &format!("svcs: {} of {} ({}, {kind})\n", self.svc_rows.len(), sv.list.len(), sv.source);
        }
        if let Some(d) = &self.model.devices {
            let classes: Vec<&str> = self.dev_classes.iter().map(|c| c.name.as_str()).collect();
            let problems = d.list.iter().filter(|v| v.status.is_problem()).count();
            let tree = if self.dev_tree { ", tree" } else { "" };
            s += &format!(
                "devices: {} of {} ({problems} problems{tree}) classes: {}\n",
                self.dev_rows.len(),
                d.list.len(),
                classes.join(" | ")
            );
        }
        if let Some(c) = &self.model.containers {
            let rt = c.runtime.map_or("none", |r| r.key());
            s += &format!("containers: {} of {} ({rt}, {:?})\n", self.ct_rows.len(), c.list.len(), self.ct_scope());
        }
        if self.model.nic_read {
            let names: Vec<&str> = self.model.nics.iter().map(|n| n.name.as_str()).collect();
            s += &format!("nics: {} ({})\n", names.len(), names.join(" "));
        }
        if self.model.conn_read {
            let c = &self.model.conns;
            let scope = if self.net_listen { "listening" } else { "all" };
            let on = self.net_nic.as_deref().map(|n| format!(", on {n}")).unwrap_or_default();
            s += &format!("conns: {} of {} ({}, {scope}{on})\n", self.net_rows.len(), c.list.len(), c.source);
        }
        if self.model.disk_read {
            let d = &self.model.disks;
            let drives: Vec<&str> = d.drives.iter().map(|x| x.id.as_str()).collect();
            let vols: Vec<&str> = d.volumes.iter().map(|v| v.mount.as_str()).collect();
            s += &format!("disks: {} ({}); volumes: {} ({})\n", drives.len(), drives.join(" "), vols.len(), vols.join(" "));
        }
        if self.model.sensor_read {
            let m = &self.model.sensors;
            let (t, f) = (m.count(sensors::Kind::Temp), m.count(sensors::Kind::Fan) + m.count(sensors::Kind::Duty));
            s += &format!("sensors: {} (temps {t}, fans {f}; chips {})\n", m.list.len(), m.chips().len());
        }
        if let Some(h) = &self.model.hw {
            let note = h.note.map(|n| format!("; note {n:?}")).unwrap_or_default();
            s += &format!("hw: {} ({}); slots {} of {}{note}\n", crate::ui::sys::machine(self), h.source, h.used(), h.slots_total());
        }
        if let Some(st) = &self.model.st {
            s += &format!("topology: {} ({})\n", st.topo.summary(), st.topo.source);
        }
        let widths: Vec<String> = self.layouts[&self.screen]
            .layout(ito_layout::Rect::new(0.0, 0.0, 1000.0, 100.0), 0.0)
            .iter()
            .map(|(p, r)| format!("{}={:.0}", p.name(), r.w))
            .collect();
        s += &format!("widths: {}\n", widths.join(" "));
        for p in Pane::ALL {
            if let Some(n) = self.stacks.get(&(self.screen, p)) {
                s += &format!("stack {}: {}\n", p.name(), stacks::sizes(n));
            }
        }
        if let Some(z) = &self.zoom {
            s += &format!(
                "zoom: {} {:?}
",
                z.title, z.range
            );
        }
        match &self.confirm {
            Some(Ask::Kill(t)) => s += &format!("confirm: kill {} {}\n", t.pid, t.name),
            Some(Ask::Svc(t)) => s += &format!("confirm: {:?} {}\n", t.action, t.name),
            Some(Ask::Ct(t)) => s += &format!("confirm: ct {:?} {}\n", t.action, t.name),
            None => {}
        }
        if let Some((t, _)) = &self.toast {
            s += &format!("toast: {t}\n");
        }
        s
    }
}

/// The cursors of a model not looked at yet: Sys opens on its slots, its
/// left column being a page to scroll.
fn new_views() -> HashMap<Screen, View> {
    HashMap::from([(Screen::Sys, View { focus: Pane::Mid.index(), ..View::default() })])
}

/// What tells one socket from another while the list is read again.
type ConnKey = (net::Proto, std::net::SocketAddr, Option<std::net::SocketAddr>);

fn conn_key(c: &net::Conn) -> ConnKey {
    (c.proto, c.local, c.remote)
}

pub fn fmt_interval(s: f32) -> String {
    if s.fract() == 0.0 { format!("{s:.0}") } else { format!("{s}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::Mem;

    fn proc(pid: u32, name: &str, cpu: f32, mem: u64) -> Proc {
        Proc {
            pid,
            ppid: None,
            name: name.into(),
            cpu,
            mem,
            threads: Some(1),
            user: Some("me".into()),
            cmd: String::new(),
            exe: None,
            start: 0,
            status: "Run".into(),
        }
    }

    fn app() -> (App, Sender<Msg>, Receiver<Ctl>) {
        let (km, _) = Keymap::load(None);
        let (tx, rx) = crossbeam_channel::unbounded();
        let (ctl_tx, ctl_rx) = crossbeam_channel::unbounded();
        let layouts = Screen::ALL.iter().map(|&s| (s, s.default_layout())).collect();
        (App::new(km, rx, ctl_tx, layouts, Saver::new(None), 1.0), tx, ctl_rx)
    }

    fn keys(a: &mut App, s: &str) {
        for k in s.split_whitespace() {
            a.feed(Key::parse(k).unwrap());
        }
    }

    #[test]
    fn a_dragged_stack_is_kept_while_its_shape_is_and_equalize_forgets_it() {
        use ito_layout::{Dir, ui::Moved};
        let (mut a, _tx, _ctl) = app();
        a.run(Act::Screen(Goto::To(Screen::Cpu)));
        let default = stacks::chain(Dir::Down, 0, 3);
        let mut moved = default.clone();
        moved.set_ratio(&[], 0.6);
        // While dragging, the drag is drawn but not yet kept.
        a.stack_moved(Pane::Mid, Moved::Dragging(moved.clone()));
        assert_eq!(a.stack(Pane::Mid, default.clone()), moved);
        assert!(a.stacks.is_empty());
        a.stack_moved(Pane::Mid, Moved::Released);
        assert_eq!(a.stacks.get(&(Screen::Cpu, Pane::Mid)), Some(&moved));
        assert_eq!(a.stack(Pane::Mid, default.clone()), moved);
        assert!(a.report().contains("stack mid: 0=1000x600"));
        // Another shape (a machine with no swap, say) falls back to the default.
        let other = stacks::chain(Dir::Down, 0, 2);
        assert_eq!(a.stack(Pane::Mid, other.clone()), other);
        // Not another screen's, and gone after equalize.
        a.run(Act::Screen(Goto::To(Screen::Mem)));
        assert_eq!(a.stack(Pane::Mid, default.clone()), default);
        a.run(Act::Screen(Goto::To(Screen::Cpu)));
        a.run(Act::Equalize);
        assert_eq!(a.stack(Pane::Mid, default.clone()), default);
        assert!(a.stacks.is_empty());
    }

    #[test]
    fn the_selection_follows_the_process_while_the_list_reorders() {
        let (mut a, tx, _ctl) = app();
        tx.send(Msg::Mem(Mem::default())).unwrap();
        tx.send(Msg::Procs { procs: vec![proc(1, "a", 50.0, 1), proc(2, "b", 10.0, 2), proc(3, "c", 5.0, 3)], total_threads: None })
            .unwrap();
        a.pump();
        keys(&mut a, "5 l j");
        assert_eq!(a.selected_proc().map(|p| p.pid), Some(2));
        tx.send(Msg::Procs { procs: vec![proc(1, "a", 1.0, 1), proc(2, "b", 90.0, 2), proc(3, "c", 5.0, 3)], total_threads: None })
            .unwrap();
        a.pump();
        assert_eq!(a.view().sel[1], 0);
        assert_eq!(a.selected_proc().map(|p| p.pid), Some(2));
    }

    #[test]
    fn the_overview_has_a_gauge_per_device_and_enter_opens_its_screen() {
        let (mut a, tx, _ctl) = app();
        assert_eq!(a.ovw_gauges(), [Gauge::Cpu, Gauge::Mem, Gauge::Swap]);
        let dev = |id: &str, kind| gpu::Device { id: id.into(), kind, ..gpu::Device::default() };
        tx.send(Msg::Gpu(vec![dev("a", gpu::Kind::Gpu), dev("b", gpu::Kind::Npu)])).unwrap();
        let drive = |id: &str| disk::Drive { id: id.into(), ..disk::Drive::default() };
        tx.send(Msg::Disks(disk::Disks { drives: vec![drive("sda"), drive("sdb")], ..disk::Disks::default() })).unwrap();
        a.pump();
        assert_eq!(a.ovw_gauges(), [Gauge::Cpu, Gauge::Mem, Gauge::Swap, Gauge::Gpu(0), Gauge::Gpu(1), Gauge::Disk(0), Gauge::Disk(1)]);
        keys(&mut a, "j j j j <Enter>");
        assert_eq!((a.screen, a.selected_gpu().map(|d| d.id.as_str())), (Screen::Gpu, Some("b")));
        keys(&mut a, "1 j j <Enter>");
        assert_eq!((a.screen, a.selected_drive().map(|d| d.id.as_str())), (Screen::Disk, Some("sdb")));
        keys(&mut a, "1 k k k k k k <Enter>");
        assert_eq!(a.screen, Screen::Cpu);
        keys(&mut a, "1 j j <Enter>");
        assert_eq!(a.screen, Screen::Mem);
    }

    #[test]
    fn the_filter_types_and_escape_drops_it() {
        let (mut a, tx, _ctl) = app();
        tx.send(Msg::Procs { procs: vec![proc(1, "alpha", 0.0, 1), proc(2, "beta", 0.0, 2)], total_threads: None }).unwrap();
        a.pump();
        keys(&mut a, "/ b e");
        assert_eq!(a.screen, Screen::Procs);
        assert_eq!(a.mode, Mode::Filter);
        assert_eq!(a.rows.len(), 1);
        keys(&mut a, "<Backspace> <Backspace>");
        assert_eq!(a.rows.len(), 2);
        keys(&mut a, "x <Enter>");
        assert_eq!((a.mode, a.filter()), (Mode::Normal, "x"));
        // Each list keeps its own: net starts empty and procs gets its own back.
        keys(&mut a, "7");
        assert_eq!(a.filter(), "");
        keys(&mut a, "/ t c p <Enter> 5");
        assert_eq!((a.screen, a.filter(), a.rows.len()), (Screen::Procs, "x", 0));
        keys(&mut a, "<Esc>");
        assert_eq!((a.filter(), a.rows.len()), ("", 2));
        keys(&mut a, "7");
        assert_eq!(a.filter(), "tcp");
        // A screen change while typing ends the typing.
        keys(&mut a, "/ u");
        a.goto(Screen::Procs);
        assert_eq!((a.mode, a.filter()), (Mode::Normal, ""));
    }

    #[test]
    fn keys_move_a_divider_and_say_when_there_is_none() {
        let (mut a, _tx, _ctl) = app();
        keys(&mut a, "2");
        let (right, up) = if cfg!(target_os = "macos") { ("<C-Right>", "<C-Up>") } else { ("<A-S-Right>", "<A-S-Up>") };
        let before = a.layouts[&Screen::Cpu].clone();
        keys(&mut a, right);
        assert_ne!(a.layouts[&Screen::Cpu], before);
        keys(&mut a, up);
        assert_eq!(a.toast.as_ref().map(|t| t.0.as_str()), Some(tr("toast.no_divider")));
        keys(&mut a, "<C-S-e>");
        let w: Vec<f32> =
            a.layouts[&Screen::Cpu].layout(ito_layout::Rect::new(0.0, 0.0, 300.0, 10.0), 0.0).iter().map(|(_, r)| r.w).collect();
        assert!(w.iter().all(|x| (x - 100.0).abs() < 0.5), "{w:?}");
    }

    #[test]
    fn interval_steps_and_pause_reach_the_sampler() {
        let (mut a, _tx, ctl) = app();
        keys(&mut a, "+ + -");
        assert_eq!(a.interval, 2.0);
        keys(&mut a, "P");
        let got: Vec<Ctl> = ctl.try_iter().collect();
        assert_eq!(got.len(), 4);
        assert!(matches!(got[3], Ctl::Pause(true)));
        for _ in 0..10 {
            keys(&mut a, "-");
        }
        assert_eq!(a.interval, 0.5);
    }

    #[test]
    fn tab_cycles_every_screen_and_enter_opens_a_process() {
        let (mut a, tx, _ctl) = app();
        keys(&mut a, "<Tab>");
        assert_eq!(a.screen, Screen::Cpu);
        keys(&mut a, "<Tab>");
        assert_eq!(a.screen, Screen::Mem);
        keys(&mut a, "<Tab>");
        assert_eq!(a.screen, Screen::Gpu);
        keys(&mut a, "<Tab>");
        assert_eq!(a.screen, Screen::Procs);
        keys(&mut a, "<Tab>");
        assert_eq!(a.screen, Screen::Svc);
        keys(&mut a, "<Tab>");
        assert_eq!(a.screen, Screen::Net);
        keys(&mut a, "<Tab>");
        assert_eq!(a.screen, Screen::Disk);
        keys(&mut a, "<Tab>");
        assert_eq!(a.screen, Screen::Sensors);
        keys(&mut a, "<Tab>");
        assert_eq!(a.screen, Screen::Sys);
        keys(&mut a, "<Tab>");
        assert_eq!(a.screen, Screen::Devices);
        keys(&mut a, "<Tab>");
        assert_eq!(a.screen, Screen::Containers);
        // The fleet's tab comes after the last screen, then round.
        keys(&mut a, "<Tab>");
        assert!(a.fleet);
        keys(&mut a, "<Tab>");
        assert!(!a.fleet && a.screen == Screen::Ovw);
        tx.send(Msg::Procs { procs: vec![proc(7, "big", 80.0, 9), proc(8, "small", 1.0, 1)], total_threads: None }).unwrap();
        a.pump();
        keys(&mut a, "l l <Enter>");
        assert_eq!(a.screen, Screen::Procs);
        assert_eq!(a.selected_proc().map(|p| p.pid), Some(7));
    }

    #[test]
    fn t_shows_the_tree_and_u_switches_the_scope() {
        let (mut a, tx, _ctl) = app();
        let child = |pid, ppid, cpu, user: &str| Proc { ppid: Some(ppid), user: Some(user.into()), ..proc(pid, "c", cpu, 1) };
        let me = Proc { user: Some("me".into()), ..proc(std::process::id(), "yagura", 0.0, 1) };
        let init = Proc { user: Some("root".into()), ..proc(1, "init", 0.0, 1) };
        let procs = vec![init, child(10, 1, 5.0, "root"), child(11, 1, 9.0, "me"), child(20, 10, 1.0, "me"), me];
        tx.send(Msg::Procs { procs, total_threads: None }).unwrap();
        a.pump();
        keys(&mut a, "5 t");
        assert!(a.tree);
        assert_eq!(a.toast.as_ref().map(|t| t.0.as_str()), Some(tr("toast.tree_on")));
        let pids: Vec<u32> = a.rows.iter().map(|&i| a.model.procs[i].pid).collect();
        // Siblings by CPU, as the list is sorted: 11 before 10.
        assert_eq!(&pids[..4], [1, 11, 10, 20]);
        assert_eq!(&a.tree_lines[..4], ["", "├ ", "└ ", "  └ "]);
        keys(&mut a, "u");
        assert!(a.mine_only());
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(trf("toast.scope", &[tr("procs.mine")])));
        // Neither 20's parent nor 11's is mine, so each is a root.
        let pids: Vec<u32> = a.rows.iter().map(|&i| a.model.procs[i].pid).collect();
        assert_eq!(pids.len(), 3);
        assert!(a.tree_lines.iter().all(|l| l.is_empty()), "{:?}", a.tree_lines);
        keys(&mut a, "u t");
        assert!(!a.mine_only() && !a.tree && a.tree_lines.is_empty());
        assert_eq!(a.rows.len(), 5);
    }

    #[test]
    fn the_settings_key_opens_the_screen_and_esc_closes_it() {
        let (mut a, _tx, _ctl) = app();
        // Ctrl+, (Cmd+, on macOS): whatever the keymap binds.
        let open = a.keymap().main.iter().find(|b| b.run == [Act::Settings]).map(|b| b.on[0]).unwrap();
        a.feed(open);
        assert_eq!(a.mode, Mode::Settings);
        // Only the settings layer is looked at: a screen key does nothing.
        keys(&mut a, "3 q");
        assert_eq!((a.mode, a.screen, a.quit), (Mode::Settings, Screen::Ovw, false));
        keys(&mut a, "<Esc>");
        assert_eq!(a.mode, Mode::Normal);
        a.feed(open);
        a.feed(open);
        assert_eq!(a.mode, Mode::Normal);
    }

    #[test]
    fn y_copies_the_sample_or_the_cards_address() {
        use crate::config::settings::Host;
        let (mut a, _tx, _ctl) = app();
        keys(&mut a, "y");
        assert!(a.copy.is_none());
        assert_eq!(a.toast.as_ref().map(|t| t.0.as_str()), Some(tr("toast.copy_none")));
        // No [[host]] yet: the sample to paste into config.toml.
        keys(&mut a, "F y");
        assert_eq!(a.copy.take(), Some(remote::host_sample(a.prefs.settings.port)));
        let (_ftx, frx) = crossbeam_channel::unbounded();
        a.set_fleet(vec![Peer::new(&Host { name: "nas".into(), addr: "localhost:7314".into(), ..Host::default() })], (frx, Vec::new()));
        keys(&mut a, "y");
        assert!(a.copy.is_none());
        keys(&mut a, "l y");
        assert_eq!(a.copy.take().as_deref(), Some("localhost:7314"));
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(trf("toast.copied", &["localhost:7314"])));
        // A line clicked in the fleet's hint.
        a.apply_out(Out { copy: Some("yagura --agent".into()), ..Out::default() }, &egui::Context::default());
        assert_eq!(a.copy.as_deref(), Some("yagura --agent"));
    }

    #[test]
    fn f_shows_the_fleet_and_its_keys_stay_on_the_cards() {
        use crate::config::settings::Host;
        use crate::remote::{Status, proto::Hello};
        let (mut a, _tx, _ctl) = app();
        let (ftx, frx) = crossbeam_channel::unbounded();
        let host = |n: &str| Peer::new(&Host { name: n.into(), addr: format!("localhost:{}", proto::PORT), ..Host::default() });
        a.set_fleet(vec![host("nas"), host("pi"), host("mac")], (frx, Vec::new()));
        keys(&mut a, "3 F");
        assert!(a.fleet && a.report().contains("screen: fleet"), "{}", a.report());
        // Two cards in a row: down goes to the card under.
        a.apply_out(Out { fleet_cols: Some(2), ..Out::default() }, &egui::Context::default());
        keys(&mut a, "j");
        assert_eq!(a.fleet_sel, 2);
        keys(&mut a, "l l l G");
        assert_eq!((a.fleet_sel, a.screen), (3, Screen::Mem));
        keys(&mut a, "k g g");
        assert_eq!(a.fleet_sel, 0);
        // The hosts' threads report through the channel.
        ftx.send((1, Event::Up(Hello { proto: proto::PROTO, interval: 1.0, ..Hello::default() }))).unwrap();
        a.pump();
        assert_eq!(a.peers[1].status(Instant::now()), Status::Ok);
        assert_eq!(a.peers[0].status(Instant::now()), Status::Connecting);
        // View only: the card says why x does nothing.
        keys(&mut a, "l l x");
        assert_eq!((a.fleet_sel, a.mode), (2, Mode::Normal));
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(trf("toast.fleet_view_only", &["pi", "kill"])));
        keys(&mut a, "h h x");
        assert_eq!(a.toast.as_ref().map(|t| t.0.as_str()), Some(tr("toast.fleet_local")));
        // Esc closes it, F opens it on the same card, a screen key closes it there.
        keys(&mut a, "<Esc> F");
        assert!(a.fleet);
        keys(&mut a, "2");
        assert!(!a.fleet && a.screen == Screen::Cpu);
        // Tab and Shift+Tab go round through the fleet's tab, the last one.
        let (first, last) = (Screen::Ovw, *Screen::ALL.iter().rev().find(|s| s.built()).unwrap());
        keys(&mut a, "F <Tab>");
        assert!(!a.fleet && a.screen == first);
        keys(&mut a, "<BackTab>");
        assert!(a.fleet && a.screen == first);
        keys(&mut a, "<BackTab>");
        assert!(!a.fleet && a.screen == last);
        keys(&mut a, "<Tab>");
        assert!(a.fleet);
        keys(&mut a, "<BackTab>");
        assert!(!a.fleet && a.screen == last);
        keys(&mut a, "2");
        a.apply_out(Out { fleet_tab: true, ..Out::default() }, &egui::Context::default());
        assert!(a.fleet);
        a.apply_out(Out { fleet_click: Some((9, false)), ..Out::default() }, &egui::Context::default());
        assert_eq!(a.fleet_sel, 3);
        // Enter on this machine's card is the screens behind.
        keys(&mut a, "g g <Enter>");
        assert!(!a.fleet && a.screen == Screen::Cpu);
    }

    #[test]
    fn enter_on_a_card_shows_that_machines_screens() {
        use crate::config::settings::Host;
        use crate::remote::{client::Why, proto::Hello};
        let (mut a, tx, ctl) = app();
        let (ftx, frx) = crossbeam_channel::unbounded();
        let (w0, wants0) = crossbeam_channel::unbounded();
        let (w1, wants1) = crossbeam_channel::unbounded();
        let host = |n: &str| Peer::new(&Host { name: n.into(), addr: format!("localhost:{}", proto::PORT), ..Host::default() });
        a.set_fleet(vec![host("nas"), host("pi")], (frx, vec![w0, w1]));
        let hello = |user: &str| Hello { proto: proto::PROTO, interval: 2.0, user: user.into(), ..Hello::default() };
        tx.send(Msg::Procs { procs: vec![proc(7, "here", 1.0, 1)], total_threads: None }).unwrap();
        a.pump();
        // A machine that is not connected stays a card.
        keys(&mut a, "5 F l <Enter>");
        assert!(a.fleet && a.away.is_none());
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(trf("toast.fleet_down", &["nas"])));
        assert!(wants0.try_recv().is_err());
        // Up, it opens on the same screen with its own readings only.
        ftx.send((0, Event::Up(hello("ana")))).unwrap();
        a.pump();
        keys(&mut a, "<Enter>");
        assert!(!a.fleet && a.screen == Screen::Procs);
        assert_eq!(a.away_peer().map(|p| p.name.as_str()), Some("nas"));
        assert_eq!((a.me.as_deref(), a.rows.len()), (Some("ana"), 0));
        assert_eq!(wants0.try_iter().collect::<Vec<_>>(), [ToHost::Want(Want { detail: true, ..Want::default() })]);
        assert!(wants1.try_recv().is_err());
        assert!(a.report().contains("machine: nas"), "{}", a.report());
        ftx.send((0, Event::Msg(Box::new(Msg::Procs { procs: vec![proc(9, "there", 5.0, 1)], total_threads: None })))).unwrap();
        // This machine's readings go to its card, the other host's to nothing.
        tx.send(Msg::Procs { procs: vec![proc(8, "here2", 1.0, 1)], total_threads: None }).unwrap();
        ftx.send((1, Event::Msg(Box::new(Msg::Procs { procs: vec![proc(10, "pi", 5.0, 1)], total_threads: None })))).unwrap();
        a.pump();
        assert_eq!(a.selected_proc().map(|p| p.pid), Some(9));
        assert_eq!(a.home().procs.iter().map(|p| p.pid).collect::<Vec<_>>(), [8]);
        // Its svc screen asks it for services, not this machine's sampler.
        let _ = ctl.try_iter().count();
        keys(&mut a, "6");
        assert_eq!(
            wants0.try_iter().collect::<Vec<_>>(),
            [ToHost::Want(Want { detail: true, services: true, conns: false, devices: false, containers: false })]
        );
        assert!(ctl.try_recv().is_err());
        // Its [agent] allow lists nothing, so it is view only; sampling stays with it.
        keys(&mut a, "x");
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(trf("toast.fleet_view_only", &["nas", "service"])));
        assert!(a.confirm.is_none() && wants0.try_recv().is_err());
        keys(&mut a, "P");
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(trf("toast.remote_sampling", &["nas"])));
        // Lost, the screens keep its last readings.
        ftx.send((0, Event::Down(Why::Closed))).unwrap();
        a.pump();
        assert!(a.away.as_ref().is_some_and(|w| w.lost));
        assert!(a.report().contains("machine: nas (lost)"), "{}", a.report());
        ftx.send((0, Event::Up(hello("ana")))).unwrap();
        a.pump();
        assert!(a.away.as_ref().is_some_and(|w| !w.lost));
        // ] goes round: pi is down, so back to this machine, where it was.
        keys(&mut a, "]");
        assert!(a.away.is_none() && a.screen == Screen::Svc);
        assert_eq!(wants0.try_iter().collect::<Vec<_>>(), [ToHost::Want(Want::default())]);
        assert!(matches!(ctl.try_iter().collect::<Vec<_>>()[..], [Ctl::Services(true)]));
        assert_eq!(a.model.procs.iter().map(|p| p.pid).collect::<Vec<_>>(), [8]);
        assert!(a.report().contains("machine: this"), "{}", a.report());
        ftx.send((1, Event::Up(hello("")))).unwrap();
        a.pump();
        keys(&mut a, "[");
        assert_eq!(a.away_peer().map(|p| p.name.as_str()), Some("pi"));
        assert_eq!(a.me, None);
        keys(&mut a, "[");
        assert_eq!(a.away_peer().map(|p| p.name.as_str()), Some("nas"));
        // The machine inside stays a stop while it is down; once out of it,
        // with every host down, there is nowhere to go.
        keys(&mut a, "]");
        ftx.send((0, Event::Down(Why::Closed))).unwrap();
        ftx.send((1, Event::Down(Why::Closed))).unwrap();
        a.pump();
        keys(&mut a, "]");
        assert!(a.away.is_none());
        assert_eq!(a.toast.as_ref().map(|t| t.0.as_str()), Some(tr("toast.machine_home")));
        keys(&mut a, "]");
        assert_eq!(a.toast.as_ref().map(|t| t.0.as_str()), Some(tr("toast.machine_none")));
    }

    #[test]
    fn a_machine_that_allows_kill_is_asked_to_end_and_answers() {
        use crate::config::settings::Host;
        use crate::remote::{client::Why, proto::Hello};
        let (mut a, _tx, _ctl) = app();
        let (ftx, frx) = crossbeam_channel::unbounded();
        let (w0, wants0) = crossbeam_channel::unbounded();
        a.set_fleet(
            vec![Peer::new(&Host { name: "nas".into(), addr: format!("localhost:{}", proto::PORT), ..Host::default() })],
            (frx, vec![w0]),
        );
        let allow = vec!["kill".to_string()];
        let hello = Hello { proto: proto::PROTO, user: "ana".into(), os: "Linux 24.04 Ubuntu".into(), allow, pid: 3, ..Hello::default() };
        ftx.send((0, Event::Up(hello))).unwrap();
        a.pump();
        keys(&mut a, "5 F l <Enter>");
        assert_eq!(a.away_peer().map(|p| p.name.as_str()), Some("nas"));
        let procs = vec![proc(3, "yagura", 50.0, 1), proc(std::process::id(), "here", 1.0, 1), proc(7, "big", 80.0, 9)];
        ftx.send((0, Event::Msg(Box::new(Msg::Procs { procs, total_threads: None })))).unwrap();
        a.pump();
        let _ = wants0.try_iter().count();
        // Its agent is the one not to end; this machine's PID there is just a number.
        keys(&mut a, "l j x");
        assert_eq!((a.mode, a.toast.as_ref().map(|t| t.0.clone())), (Mode::Normal, Some(trf("toast.kill_agent", &["nas"]))));
        // CPUs are chosen on this machine only.
        keys(&mut a, "A");
        if crate::affinity::SUPPORTED {
            assert_eq!((a.mode, a.toast.as_ref().map(|t| t.0.clone())), (Mode::Normal, Some(trf("toast.aff_remote", &["nas"]))));
        }
        keys(&mut a, "k x");
        assert!(matches!(&a.confirm, Some(Ask::Kill(t)) if t.pid == 7), "{:?}", a.confirm);
        assert_eq!(a.family(), Family::Other);
        keys(&mut a, "y");
        assert_eq!(wants0.try_iter().collect::<Vec<_>>(), [ToHost::Act(1, proto::Act::Kill { pid: 7, start: 0 })]);
        let t = Target { pid: 7, name: "big".into(), start: 0 };
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(trf("toast.on_machine", &["nas", &Ask::Kill(t.clone()).starting()])));
        assert_eq!(a.asked.len(), 1);
        // The answer names the machine; one for another request is not this one's.
        ftx.send((0, Event::Done(9, Done::Kill(Outcome::Gone)))).unwrap();
        ftx.send((0, Event::Done(1, Done::Kill(Outcome::Ended)))).unwrap();
        a.pump();
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(trf("toast.on_machine", &["nas", &kill_toast(&t, Outcome::Ended)])));
        assert!(a.asked.is_empty());
        // A refusal says why, in its own words.
        keys(&mut a, "x y");
        ftx.send((0, Event::Done(2, Done::Refused(Refusal::NotAllowed)))).unwrap();
        a.pump();
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(trf("toast.fleet_view_only", &["nas", "kill"])));
        // Lost before it answers: said once, and the request is forgotten.
        keys(&mut a, "x y");
        assert_eq!(a.asked.len(), 1);
        ftx.send((0, Event::Down(Why::Closed))).unwrap();
        a.pump();
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(trf("toast.remote_no_answer", &["nas"])));
        assert!(a.asked.is_empty());
        // Down, nothing is sent.
        assert_eq!(wants0.try_iter().filter(|w| matches!(w, ToHost::Act(..))).count(), 2);
        keys(&mut a, "x y");
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(trf("toast.fleet_down", &["nas"])));
        assert!(a.asked.is_empty() && !wants0.try_iter().any(|w| matches!(w, ToHost::Act(..))));
        // service is not in its allow.
        keys(&mut a, "6 x");
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(trf("toast.fleet_view_only", &["nas", "service"])));
        // Its containers are view only, whatever its allow says.
        for k in ["C x", "S", "R"] {
            keys(&mut a, k);
            assert_eq!((a.mode, a.toast.as_ref().map(|t| t.0.clone())), (Mode::Normal, Some(trf("toast.ct_remote", &["nas"]))));
        }
        assert!(!wants0.try_iter().any(|w| matches!(w, ToHost::Act(..))));
    }

    #[test]
    fn the_os_in_a_hello_picks_the_words() {
        assert_eq!(Family::of("Windows 11 Pro"), Family::Windows);
        assert_eq!(Family::of("macOS 15 Sequoia"), Family::Mac);
        assert_eq!(Family::of("Darwin 24.1"), Family::Mac);
        assert_eq!(Family::of("Linux 24.04 Ubuntu"), Family::Other);
        assert_eq!(Family::of(""), Family::Other);
    }

    #[test]
    fn a_zoomed_graph_steps_its_range_and_closes() {
        use crate::model::Series;
        use crate::zoom::Unit;
        let (mut a, _tx, _ctl) = app();
        a.open_zoom(Zoom::new(Series::Cpu, "CPU", egui::Color32::WHITE, Unit::Pct));
        assert_eq!(a.mode, Mode::Zoom);
        // The main layer is shut while it is open.
        keys(&mut a, "3");
        assert_eq!((a.screen, a.mode), (Screen::Ovw, Mode::Zoom));
        keys(&mut a, "]");
        assert_eq!(a.zoom.as_ref().map(|z| z.range), Some(Range::Min15));
        keys(&mut a, "] ]");
        assert_eq!(a.zoom.as_ref().map(|z| z.range), Some(Range::Hour1));
        keys(&mut a, "<Esc>");
        assert_eq!((a.mode, a.zoom.is_none()), (Mode::Normal, true));
        // The next one opens where this one was left.
        a.open_zoom(Zoom::new(Series::Mem, "MEM", egui::Color32::WHITE, Unit::Pct));
        assert_eq!(a.zoom.as_ref().map(|z| z.range), Some(Range::Hour1));
    }

    #[test]
    fn x_asks_first_and_n_keeps_the_process() {
        let (mut a, tx, _ctl) = app();
        keys(&mut a, "x");
        assert_eq!((a.mode, a.toast.as_ref().map(|t| t.0.as_str())), (Mode::Normal, Some(tr("toast.kill_none"))));
        tx.send(Msg::Procs { procs: vec![proc(7, "big", 80.0, 9), proc(std::process::id(), "yagura", 1.0, 1)], total_threads: None })
            .unwrap();
        a.pump();
        keys(&mut a, "5 l x");
        assert_eq!(a.mode, Mode::Confirm);
        assert!(matches!(&a.confirm, Some(Ask::Kill(t)) if t.pid == 7), "{:?}", a.confirm);
        // Keys of the main layer do nothing while the question is up.
        keys(&mut a, "j 1");
        assert_eq!((a.screen, a.mode), (Screen::Procs, Mode::Confirm));
        keys(&mut a, "n");
        assert_eq!((a.mode, a.confirm.clone()), (Mode::Normal, None));
        keys(&mut a, "j x");
        assert_eq!(a.mode, Mode::Normal);
        assert_eq!(a.toast.as_ref().map(|t| t.0.as_str()), Some(tr("toast.kill_self")));
    }

    #[test]
    fn a_opens_the_cpus_of_the_process_and_takes_only_cpus() {
        let (mut a, tx, _ctl) = app();
        let toast = |a: &App| a.toast.as_ref().map(|t| t.0.clone());
        if !crate::affinity::SUPPORTED {
            keys(&mut a, "5 l A");
            assert_eq!((a.mode, toast(&a)), (Mode::Normal, Some(tr("toast.aff_unsupported").to_string())));
            return;
        }
        // Not on another screen, and not without a process.
        keys(&mut a, "A");
        assert_eq!((a.mode, toast(&a)), (Mode::Normal, None));
        keys(&mut a, "5 A");
        assert_eq!((a.mode, toast(&a)), (Mode::Normal, Some(tr("toast.kill_none").to_string())));
        tx.send(Msg::Procs { procs: vec![proc(7, "big", 80.0, 9)], total_threads: None }).unwrap();
        a.pump();
        // Its CPUs are read on their own thread (and `keys` does not pump,
        // so they are given below); until then nothing is typed or set.
        keys(&mut a, "l A");
        assert_eq!(a.mode, Mode::Affinity);
        let e = a.affinity.clone().unwrap();
        assert_eq!((e.t.pid, e.text.as_str(), e.ids, a.affinity_now()), (7, "", None, None));
        keys(&mut a, "1 <Backspace> <Enter>");
        a.toggle_cpu(1);
        assert_eq!((a.mode, a.affinity.as_ref().map(|e| e.text.as_str())), (Mode::Affinity, Some("")));
        // Another process's reading is not this one's. CPU 9 it may run on
        // is switched off, so the line starts without it.
        use crate::affinity::Look;
        a.aff_looked(8, Look { now: Some("2".into()), ids: vec![0, 1, 2] });
        a.aff_looked(7, Look { now: Some("0-3,9".into()), ids: vec![0, 1, 2, 3] });
        assert_eq!((a.affinity.as_ref().map(|e| e.text.as_str()), a.affinity_now()), (Some("0-3"), Some("0-3,9")));
        // A second reading does not undo what is typed.
        keys(&mut a, "<Backspace>");
        a.aff_looked(7, Look { now: Some("1".into()), ids: vec![0, 1, 2, 3] });
        assert_eq!(a.affinity.as_ref().map(|e| e.text.as_str()), Some("0-"));
        keys(&mut a, "3");
        // Digits, commas, dashes and spaces are typed; letters, Ctrl and the
        // main layer's keys are not.
        keys(&mut a, "<Backspace> <Backspace> <Backspace> 1 , <Space> 3 x j <C-2> q");
        assert_eq!((a.screen, a.affinity.as_ref().map(|e| e.text.as_str())), (Screen::Procs, Some("1, 3")));
        // A click puts a CPU in or takes it out, and the line is written again.
        a.toggle_cpu(2);
        assert_eq!(a.affinity.as_ref().map(|e| e.text.as_str()), Some("1-3"));
        a.toggle_cpu(1);
        a.toggle_cpu(9);
        assert_eq!(a.affinity.as_ref().map(|e| e.text.as_str()), Some("2-3"));
        // A line that does not read stays open to be put right.
        keys(&mut a, ", 9 <Enter>");
        assert_eq!((a.mode, a.affinity.as_ref().map(|e| e.text.as_str())), (Mode::Affinity, Some("2-3,9")));
        keys(&mut a, "<Esc>");
        assert_eq!((a.mode, a.affinity.is_none()), (Mode::Normal, true));
        // CPUs with gaps (Linux, some switched off) are the boxes there are;
        // with nothing read of what it has now, the line starts empty.
        keys(&mut a, "A");
        a.aff_looked(7, Look { now: None, ids: vec![0, 1, 8, 9] });
        assert_eq!((a.affinity.as_ref().map(|e| e.text.as_str()), a.affinity_now()), (Some(""), None));
        keys(&mut a, "<Enter>");
        a.toggle_cpu(5);
        a.toggle_cpu(8);
        assert_eq!((a.mode, a.affinity.as_ref().map(|e| e.text.as_str())), (Mode::Affinity, Some("8")));
        keys(&mut a, "<Esc>");
        assert_eq!(a.mode, Mode::Normal);
    }

    #[test]
    fn a_pins_a_child_and_the_toast_says_where() {
        if !crate::affinity::SUPPORTED {
            return;
        }
        #[cfg(windows)]
        let mut child =
            std::process::Command::new("ping").args(["-n", "30", "127.0.0.1"]).stdout(std::process::Stdio::null()).spawn().unwrap();
        #[cfg(not(windows))]
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let (mut a, tx, _ctl) = app();
        tx.send(Msg::Procs { procs: vec![proc(child.id(), "child", 1.0, 1)], total_threads: None }).unwrap();
        a.pump();
        keys(&mut a, "5 l A");
        // What it has now is read on its own thread, as the change is; the
        // line is put on the first CPU it may run on.
        let deadline = Instant::now() + Duration::from_secs(10);
        while a.affinity.as_ref().is_some_and(|e| e.ids.is_none()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            a.pump();
        }
        let e = a.affinity.as_mut().unwrap();
        let first = e.ids.as_ref().and_then(|ids| crate::affinity::lenient(&e.text, ids).first().copied()).expect("its CPUs");
        e.text = first.to_string();
        keys(&mut a, "<Enter>");
        assert_eq!((a.mode, a.affinity.is_none()), (Mode::Normal, true));
        // The outcome comes back as a toast.
        while a.toast.is_none() && Instant::now() < deadline + Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
            a.pump();
        }
        let _ = child.kill();
        let _ = child.wait();
        let want = trf("toast.aff_set", &["child", &child.id().to_string(), &first.to_string()]);
        assert_eq!(a.toast.as_ref().map(|t| t.0.as_str()), Some(want.as_str()));
    }

    #[test]
    fn enter_on_a_gpu_and_then_its_process_opens_procs() {
        let (mut a, tx, _ctl) = app();
        tx.send(Msg::Procs { procs: vec![proc(7, "game", 80.0, 9), proc(8, "idle", 1.0, 1)], total_threads: None }).unwrap();
        let procs = vec![gpu::GpuProc { pid: 7, usage: Some(60.0), mem: Some(1 << 30) }];
        let dev = |id: &str, procs| gpu::Device { id: id.into(), usage: Some(60.0), procs, ..gpu::Device::default() };
        tx.send(Msg::Gpu(vec![dev("a", Vec::new()), dev("b", procs)])).unwrap();
        a.pump();
        keys(&mut a, "4 <Enter>");
        assert_eq!(a.focus(), Pane::Left, "nothing to open on a device without processes");
        keys(&mut a, "j <Enter>");
        assert_eq!((a.focus(), a.count(Screen::Gpu, Pane::Right)), (Pane::Right, 1));
        assert_eq!(a.focused_proc().map(|p| p.pid), Some(7));
        keys(&mut a, "<Enter>");
        assert_eq!(a.screen, Screen::Procs);
        assert_eq!(a.selected_proc().map(|p| p.pid), Some(7));
        // A device that goes away takes the selection back into the list.
        tx.send(Msg::Gpu(vec![dev("a", Vec::new())])).unwrap();
        a.pump();
        assert_eq!(a.view_of(Screen::Gpu).sel, [0, 0, 0]);
    }

    #[test]
    fn svc_reads_only_while_shown_and_enter_opens_its_process() {
        let (mut a, tx, ctl) = app();
        keys(&mut a, "6 6 5");
        let got: Vec<Ctl> = ctl.try_iter().collect();
        assert!(matches!(got[..], [Ctl::Services(true), Ctl::Services(false)]), "{got:?}");
        tx.send(Msg::Procs { procs: vec![proc(7, "sshd", 1.0, 1)], total_threads: None }).unwrap();
        let svc = |name: &str, state, pid| svc::Service { name: name.into(), state, pid, ..svc::Service::default() };
        let list =
            vec![svc("cron", svc::State::Running, None), svc("dead", svc::State::Failed, None), svc("sshd", svc::State::Running, Some(7))];
        tx.send(Msg::Svc(svc::Services { list, source: "systemd", note: None })).unwrap();
        a.pump();
        keys(&mut a, "6");
        assert_eq!((a.svc_rows.len(), a.count_svc(Some(svc::State::Failed))), (3, 1));
        // The left column's third row shows the stopped ones: none.
        keys(&mut a, "j j");
        assert!(a.svc_rows.is_empty());
        keys(&mut a, "k <Enter>");
        assert_eq!((a.focus(), a.svc_rows.len()), (Pane::Mid, 2));
        keys(&mut a, "<Enter>");
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(trf("toast.svc_no_pid", &["cron"])));
        // The filter narrows the list and the selection stays on the name.
        keys(&mut a, "j / s s <Enter>");
        assert_eq!((a.screen, a.svc_rows.len()), (Screen::Svc, 1));
        assert_eq!(a.selected_svc().map(|s| s.name.as_str()), Some("sshd"));
        keys(&mut a, "<Enter>");
        assert_eq!(a.screen, Screen::Procs);
        assert_eq!(a.selected_proc().map(|p| p.pid), Some(7));
        assert!(matches!(ctl.try_iter().last(), Some(Ctl::Services(false))));
    }

    #[test]
    fn containers_read_only_while_shown_scope_and_filter() {
        use containers::{Container, Containers, State};
        let (mut a, tx, ctl) = app();
        keys(&mut a, "C");
        assert_eq!(a.screen, Screen::Containers);
        keys(&mut a, "1");
        let got: Vec<Ctl> = ctl.try_iter().filter(|c| matches!(c, Ctl::Containers(_))).collect();
        assert!(matches!(got[..], [Ctl::Containers(true), Ctl::Containers(false)]), "{got:?}");
        let ct = |name: &str, state| Container {
            id: format!("id-{name}"),
            name: name.into(),
            image: "img".into(),
            state,
            ..Container::default()
        };
        let list = vec![ct("web", State::Running), ct("db", State::Running), ct("old", State::Exited), ct("hold", State::Paused)];
        tx.send(Msg::Containers(Box::new(Containers { list, runtime: Some(containers::Runtime::Docker), note: None, engine: None })))
            .unwrap();
        a.pump();
        keys(&mut a, "C");
        assert_eq!(a.ct_rows.len(), 4);
        // The left column narrows to running, then to stopped.
        keys(&mut a, "j");
        assert_eq!((a.ct_scope(), a.ct_rows.len()), (CtScope::Running, 2));
        keys(&mut a, "j j");
        assert_eq!(a.selected_ct().map(|c| c.name.as_str()), Some("old"));
        keys(&mut a, "k k <Enter>");
        assert_eq!(a.focus(), Pane::Mid);
        keys(&mut a, "/ d b <Enter>");
        assert_eq!(a.selected_ct().map(|c| c.name.as_str()), Some("db"));
        assert!(a.report().contains("containers: 1 of 4"), "{}", a.report());
    }

    #[test]
    fn a_container_is_moved_after_asking_and_only_somewhere_else() {
        use containers::act::{Outcome as O, Target as T};
        use containers::{Container, Containers, Endpoint, Runtime, State};
        let (mut a, tx, _ctl) = app();
        // Where nothing listens: what is asked never reaches a real engine.
        let nowhere = if cfg!(windows) {
            Endpoint::Pipe(r"\\.\pipe\yagura-test-nowhere".into())
        } else {
            Endpoint::Unix("/nonexistent/yagura-test-nowhere".into())
        };
        keys(&mut a, "C");
        // Nothing listed yet.
        keys(&mut a, "x");
        assert_eq!((a.mode, a.toast.as_ref().map(|t| t.0.clone())), (Mode::Normal, Some(tr("ct.none").to_string())));
        let ct = |name: &str, state| Container {
            id: format!("id-{name}"),
            name: name.into(),
            image: "img".into(),
            state,
            ..Container::default()
        };
        let list = vec![ct("web", State::Running), ct("old", State::Exited), ct("hold", State::Paused), ct("", State::Created)];
        tx.send(Msg::Containers(Box::new(Containers { list, runtime: Some(Runtime::Podman), note: None, engine: Some(nowhere.clone()) })))
            .unwrap();
        a.pump();
        keys(&mut a, "<Enter>");
        assert_eq!(a.selected_ct().map(|c| c.name.as_str()), Some("web"));
        let t = |action, name: &str, id: &str| {
            Some(Ask::Ct(T { action, id: id.into(), name: name.into(), runtime: Runtime::Podman, engine: Some(nowhere.clone()) }))
        };
        // Stop and restart a running one, after asking; Esc forgets it.
        keys(&mut a, "x");
        assert_eq!((a.mode, &a.confirm), (Mode::Confirm, &t(Action::Stop, "web", "id-web")));
        assert!(a.report().contains("confirm: ct Stop web\n"), "{}", a.report());
        keys(&mut a, "<Esc>");
        assert_eq!((a.mode, &a.confirm), (Mode::Normal, &None));
        keys(&mut a, "R");
        assert_eq!(a.confirm, t(Action::Restart, "web", "id-web"));
        keys(&mut a, "<Esc>");
        // A running one is not started again.
        keys(&mut a, "S");
        assert_eq!((a.mode, a.toast.as_ref().map(|t| t.0.clone())), (Mode::Normal, Some(trf("toast.ct_already_running", &["web"]))));
        // A stopped one is not stopped, and is started.
        keys(&mut a, "j x");
        assert_eq!((a.mode, a.toast.as_ref().map(|t| t.0.clone())), (Mode::Normal, Some(trf("toast.ct_not_running", &["old"]))));
        keys(&mut a, "S");
        assert_eq!(a.confirm, t(Action::Start, "old", "id-old"));
        // Asked: until the engine answers, it is not asked again.
        keys(&mut a, "<Enter>");
        assert_eq!((a.mode, a.toast.as_ref().map(|t| t.0.clone())), (Mode::Normal, Some(trf("toast.ct_starting", &["old"]))));
        assert!(a.ct_busy.contains("id-old"), "{:?}", a.ct_busy);
        keys(&mut a, "S");
        assert_eq!((a.mode, a.toast.as_ref().map(|t| t.0.clone())), (Mode::Normal, Some(trf("toast.ct_busy", &["old"]))));
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !a.ct_busy.is_empty() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(10));
            a.pump();
        }
        assert!(a.ct_busy.is_empty(), "no answer in 5 s");
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(trf("toast.ct_no_engine", &["old"])));
        keys(&mut a, "S");
        assert_eq!(a.confirm, t(Action::Start, "old", "id-old"));
        keys(&mut a, "<Esc>");
        // A paused one is resumed, not started; the toast says how.
        keys(&mut a, "j S");
        assert_eq!(
            (a.mode, a.toast.as_ref().map(|t| t.0.clone())),
            (Mode::Normal, Some(trf("toast.ct_paused", &["hold", "podman unpause hold"])))
        );
        // One with no name goes by its id.
        keys(&mut a, "j S");
        assert_eq!(a.confirm, t(Action::Start, "id-", "id-"));
        keys(&mut a, "<Esc>");
        // On another screen S says where it works.
        keys(&mut a, "5 S");
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(tr("toast.svc_elsewhere").to_string()));
        // The toasts once the engine has answered.
        let web = T { action: Action::Stop, id: "id-web".into(), name: "web".into(), runtime: Runtime::Docker, engine: None };
        assert_eq!(ct_toast(&web, O::Done), trf("toast.ct_stopped", &["web"]));
        assert_eq!(ct_toast(&web, O::Already), trf("toast.ct_not_running", &["web"]));
        assert_eq!(ct_toast(&T { action: Action::Start, ..web.clone() }, O::Already), trf("toast.ct_already_running", &["web"]));
        assert_eq!(ct_toast(&web, O::Gone), trf("toast.ct_gone", &["web"]));
        assert_eq!(ct_toast(&web, O::Slow), trf("toast.ct_slow", &["web"]));
        assert_eq!(ct_toast(&web, O::Unclear), trf("toast.ct_unclear", &["web"]));
        assert_eq!(
            ct_toast(&T { action: Action::Restart, ..web.clone() }, O::Failed("port is already allocated".into())),
            trf("toast.ct_restart_failed", &["web", "port is already allocated"])
        );
        assert_eq!(Ask::Ct(web).starting(), trf("toast.ct_stopping", &["web"]));
    }

    #[test]
    fn devices_read_only_while_shown_filter_tree_and_copy() {
        use devices::{Device, Devices, Status};
        let (mut a, tx, ctl) = app();
        keys(&mut a, "D");
        assert_eq!(a.screen, Screen::Devices);
        keys(&mut a, "5");
        let got: Vec<Ctl> = ctl.try_iter().filter(|c| matches!(c, Ctl::Devices(_))).collect();
        assert!(matches!(got[..], [Ctl::Devices(true), Ctl::Devices(false)]), "{got:?}");
        let dev = |id: &str, parent: Option<&str>, name: &str, class: &str, status| Device {
            id: id.into(),
            parent: parent.map(Into::into),
            name: name.into(),
            class_name: class.into(),
            status,
            ..Device::default()
        };
        let list = vec![
            // In name order, as the source sends them.
            dev("cam", Some("hub"), "Camera", "Usb", Status::Problem(43)),
            dev("mouse", Some("hub"), "Mouse", "Input", Status::Ok),
            dev("hub", None, "Root hub", "Usb", Status::Ok),
        ];
        tx.send(Msg::Devices(Box::new(Devices { list, note: None }))).unwrap();
        a.pump();
        keys(&mut a, "D");
        let names: Vec<(&str, usize, bool)> = a.dev_classes.iter().map(|c| (c.name.as_str(), c.total, c.problem)).collect();
        assert_eq!(names, [("Input", 1, false), ("Usb", 2, true)]);
        assert_eq!(a.dev_rows.len(), 3);
        // The second class keeps only its own devices.
        keys(&mut a, "j j");
        assert_eq!(a.selected_dev_class().map(|c| c.name.as_str()), Some("Usb"));
        assert_eq!(a.dev_rows.len(), 2);
        keys(&mut a, "k k");
        // The tree puts the mouse and the camera under the hub.
        keys(&mut a, "t");
        assert!(a.dev_tree);
        let order: Vec<&str> = a.dev_rows.iter().map(|&i| a.devices()[i].id.as_str()).collect();
        assert_eq!(order, ["hub", "cam", "mouse"]);
        assert!(a.dev_lines[0].is_empty() && !a.dev_lines[1].is_empty());
        assert_eq!(a.dev_rows.len(), 3);
        keys(&mut a, "t / m o u <Enter>");
        assert!(!a.dev_tree);
        assert_eq!(a.selected_dev().map(|d| d.id.as_str()), Some("mouse"));
        keys(&mut a, "y");
        assert_eq!(a.copy.as_deref(), Some("mouse"));
        assert!(a.report().contains("devices: 1 of 3 (1 problems) classes: Input | Usb"), "{}", a.report());
    }

    #[test]
    fn on_svc_s_x_and_r_ask_about_the_service_and_n_leaves_it() {
        let (mut a, tx, _ctl) = app();
        keys(&mut a, "6 S");
        assert_eq!(a.toast.as_ref().map(|t| t.0.as_str()), Some(tr("toast.svc_none")));
        let svc = |name: &str, state| svc::Service { name: name.into(), state, ..svc::Service::default() };
        let list = vec![svc("cron", svc::State::Running), svc("dead", svc::State::Failed)];
        tx.send(Msg::Svc(svc::Services { list, source: "systemd", note: None })).unwrap();
        a.pump();
        // Already where it would go: a toast, no question.
        keys(&mut a, "S");
        assert_eq!((a.mode, a.toast.as_ref().map(|t| t.0.clone())), (Mode::Normal, Some(trf("toast.svc_already_running", &["cron"]))));
        keys(&mut a, "x");
        assert_eq!(a.mode, Mode::Confirm);
        assert_eq!(a.confirm, Some(Ask::Svc(svcctl::Target { action: Action::Stop, name: "cron".into(), plist: None, driver: false })));
        keys(&mut a, "<Esc>");
        assert_eq!((a.mode, a.confirm.clone()), (Mode::Normal, None));
        keys(&mut a, "l j x");
        assert_eq!((a.mode, a.toast.as_ref().map(|t| t.0.clone())), (Mode::Normal, Some(trf("toast.svc_not_running", &["dead"]))));
        keys(&mut a, "R");
        assert_eq!(a.confirm, Some(Ask::Svc(svcctl::Target { action: Action::Restart, name: "dead".into(), plist: None, driver: false })));
        keys(&mut a, "n S");
        assert_eq!(a.confirm, Some(Ask::Svc(svcctl::Target { action: Action::Start, name: "dead".into(), plist: None, driver: false })));
        keys(&mut a, "q 5 S");
        assert_eq!((a.mode, a.toast.as_ref().map(|t| t.0.as_str())), (Mode::Normal, Some(tr("toast.svc_elsewhere"))));
    }

    #[test]
    fn a_job_not_loaded_is_started_from_its_plist_unless_it_is_disabled() {
        let (mut a, tx, _ctl) = app();
        let job = |name: &str, start| svc::Service {
            name: name.into(),
            state: svc::State::Stopped,
            start,
            file: Some(format!("/Library/LaunchAgents/{name}.plist")),
            unloaded: true,
            ..svc::Service::default()
        };
        let list = vec![job("off", svc::Start::Disabled), job("on", svc::Start::Manual)];
        tx.send(Msg::Svc(svc::Services { list, source: "launchctl", note: None })).unwrap();
        keys(&mut a, "6");
        a.pump();
        keys(&mut a, "l S");
        let hint = trf("toast.svc_disabled", &["off", &svcctl::enable_line("off")]);
        assert_eq!((a.mode, a.toast.as_ref().map(|t| t.0.clone())), (Mode::Normal, Some(hint)));
        keys(&mut a, "j R");
        let plist = Some("/Library/LaunchAgents/on.plist".to_string());
        assert_eq!(a.confirm, Some(Ask::Svc(svcctl::Target { action: Action::Restart, name: "on".into(), plist, driver: false })));
    }

    #[test]
    fn u_on_svc_switches_between_services_and_drivers() {
        let (mut a, tx, _ctl) = app();
        let svc = |name: &str, driver| svc::Service { name: name.into(), state: svc::State::Running, driver, ..svc::Service::default() };
        tx.send(Msg::Svc(svc::Services { list: vec![svc("cron", false)], source: "systemd", note: None })).unwrap();
        keys(&mut a, "6");
        a.pump();
        // No driver in the list (not Windows): a toast, the kind stays.
        keys(&mut a, "u");
        assert_eq!((a.svc_drivers, a.toast.as_ref().map(|t| t.0.as_str())), (false, Some(tr("toast.svc_no_drivers"))));
        let list = vec![svc("Spooler", false), svc("Tcpip", true), svc("disk", true)];
        tx.send(Msg::Svc(svc::Services { list, source: "SCM", note: None })).unwrap();
        a.pump();
        assert_eq!((a.svc_rows.len(), a.count_svc(None)), (1, 1));
        keys(&mut a, "u");
        assert_eq!((a.svc_drivers, a.svc_rows.len(), a.count_svc(None)), (true, 2, 2));
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(trf("toast.scope", &[tr("svc.drivers")])));
        keys(&mut a, "l x");
        assert_eq!(a.confirm, Some(Ask::Svc(svcctl::Target { action: Action::Stop, name: "Tcpip".into(), plist: None, driver: true })));
        keys(&mut a, "<Esc> u");
        assert_eq!((a.svc_drivers, a.selected_svc().map(|s| s.name.as_str())), (false, Some("Spooler")));
    }

    #[test]
    fn svc_start_counts_follow_the_kind_and_the_filter() {
        let (mut a, tx, _ctl) = app();
        let svc = |name: &str, start, driver| svc::Service { name: name.into(), start, driver, ..svc::Service::default() };
        let list = vec![
            svc("Spooler", svc::Start::Auto, false),
            svc("Audiosrv", svc::Start::Auto, false),
            svc("ALG", svc::Start::Manual, false),
            svc("Tcpip", svc::Start::Auto, true),
        ];
        tx.send(Msg::Svc(svc::Services { list, source: "SCM", note: None })).unwrap();
        keys(&mut a, "6");
        a.pump();
        assert_eq!((a.svc_kind().count(), a.count_start(svc::Start::Auto), a.count_start(svc::Start::Manual)), (3, 2, 1));
        keys(&mut a, "/ a u d i o <Enter>");
        assert_eq!((a.count_start(svc::Start::Auto), a.count_start(svc::Start::Manual)), (1, 0));
        keys(&mut a, "<Esc>");
        keys(&mut a, "u");
        assert_eq!((a.svc_kind().count(), a.count_start(svc::Start::Auto)), (1, 1));
    }

    #[test]
    fn a_long_toast_stays_longer() {
        assert_eq!(toast_time("Copied"), TOAST);
        let long = toast_time(&trf("toast.svc_denied_win", &["Spooler"]));
        assert!(long > TOAST && long <= TOAST + Duration::from_secs(5), "{long:?}");
    }

    #[test]
    fn y_on_a_service_runs_on_a_thread_and_the_list_is_read_again() {
        let (mut a, tx, ctl) = app();
        // A name no OS has: the worker comes back refused or failed.
        let name = "yagura-test-no-such-service";
        let list = vec![svc::Service { name: name.into(), state: svc::State::Running, ..svc::Service::default() }];
        tx.send(Msg::Svc(svc::Services { list, source: "test", note: None })).unwrap();
        keys(&mut a, "6");
        a.pump();
        keys(&mut a, "x y");
        assert_eq!((a.mode, a.confirm.clone()), (Mode::Normal, None));
        let stopping = trf("toast.svc_stopping", &[name]);
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(stopping.clone()));
        let _: Vec<Ctl> = ctl.try_iter().collect();
        let until = Instant::now() + Duration::from_secs(20);
        while a.toast.as_ref().is_some_and(|t| t.0 == stopping) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
            a.pump();
        }
        let toast = a.toast.map(|t| t.0).unwrap_or_default();
        assert!(toast != stopping && toast.contains(name), "{toast}");
        assert!(matches!(ctl.try_iter().last(), Some(Ctl::Services(true))));
    }

    #[test]
    fn net_reads_sockets_only_while_shown_and_enter_opens_the_owner() {
        let (mut a, tx, ctl) = app();
        keys(&mut a, "7 1");
        let got: Vec<Ctl> = ctl.try_iter().collect();
        assert!(matches!(got[..], [Ctl::Net(true), Ctl::Net(false)]), "{got:?}");
        tx.send(Msg::Procs { procs: vec![proc(7, "sshd", 1.0, 1)], total_threads: None }).unwrap();
        let conn = |proto, local: &str, remote: Option<&str>, state, pid| net::Conn {
            proto,
            local: local.parse().unwrap(),
            remote: remote.map(|r| r.parse().unwrap()),
            state,
            pid,
        };
        let list = vec![
            conn(net::Proto::Tcp, "0.0.0.0:22", None, net::State::Listen, Some(7)),
            conn(net::Proto::Tcp, "10.0.0.2:50000", Some("8.8.8.8:443"), net::State::Established, None),
            conn(net::Proto::Udp, "0.0.0.0:5353", None, net::State::None, None),
            conn(net::Proto::Tcp, "127.0.0.1:40000", Some("127.0.0.1:631"), net::State::Established, Some(7)),
        ];
        tx.send(Msg::Conns(net::Conns { list, source: "/proc/net", note: None })).unwrap();
        let nic = |name: &str, kind, ip: &str| net::Nic { name: name.into(), kind, up: true, ipv4: vec![ip.into()], ..net::Nic::default() };
        tx.send(Msg::Nics(vec![nic("eth0", net::Kind::Wired, "10.0.0.2/24"), nic("lo", net::Kind::Loopback, "127.0.0.1/8")])).unwrap();
        a.pump();
        keys(&mut a, "7");
        assert_eq!((a.net_rows.len(), a.selected_nic().map(|n| n.name.as_str())), (4, Some("eth0")));
        // u keeps the sockets that wait for others: TCP listening and unconnected UDP.
        keys(&mut a, "u");
        assert!(a.net_listen);
        assert_eq!(a.net_rows.len(), 2);
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(trf("toast.scope", &[tr("net.listening")])));
        keys(&mut a, "u");
        assert_eq!(a.net_rows.len(), 4);
        // Enter on an interface keeps its sockets (and those on any address)
        // and moves to them; one with no owner toasts.
        keys(&mut a, "<Enter>");
        assert_eq!((a.focus(), a.net_nic.as_deref(), a.net_rows.len()), (Pane::Mid, Some("eth0"), 3));
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(trf("toast.net_nic", &["eth0"])));
        keys(&mut a, "j <Enter>");
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(trf("toast.net_no_pid", &["10.0.0.2:50000"])));
        // The narrowing follows the selected interface, and Esc drops it.
        keys(&mut a, "h j");
        assert_eq!(a.net_nic.as_deref(), Some("lo"));
        // Only a toast still naming the last interface is said again.
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(trf("toast.net_no_pid", &["10.0.0.2:50000"])));
        keys(&mut a, "<Esc> k <Enter> h j");
        assert_eq!(a.toast.as_ref().map(|t| t.0.clone()), Some(trf("toast.net_nic", &["lo"])));
        assert!(a.net_rows.iter().all(|&i| a.model.conns.list[i].local.port() != 50000));
        assert_eq!(a.net_rows.len(), 3);
        keys(&mut a, "<Esc>");
        assert_eq!((a.net_nic.as_deref(), a.net_rows.len()), (None, 4));
        // Moving between interfaces no longer narrows.
        keys(&mut a, "k");
        assert_eq!((a.net_nic.as_deref(), a.net_rows.len()), (None, 4));
        // The filter matches the process name, and the selection follows the socket.
        keys(&mut a, "l / s s h <Enter>");
        assert_eq!((a.screen, a.net_rows.len()), (Screen::Net, 2));
        keys(&mut a, "g g");
        assert_eq!(a.selected_conn().map(|c| c.local.port()), Some(22));
        keys(&mut a, "<Enter>");
        assert_eq!(a.screen, Screen::Procs);
        assert_eq!(a.selected_proc().map(|p| p.pid), Some(7));
        assert!(matches!(ctl.try_iter().last(), Some(Ctl::Net(false))));
    }

    #[test]
    fn enter_on_the_disk_screen_moves_between_a_drive_and_its_volumes() {
        let (mut a, tx, _ctl) = app();
        let drive = |id: &str| disk::Drive { id: id.into(), ..disk::Drive::default() };
        let vol = |m: &str, d: Option<&str>| disk::Volume { mount: m.into(), drive: d.map(Into::into), ..disk::Volume::default() };
        let volumes = vec![vol("/", Some("sda")), vol("/boot", Some("sdb")), vol("/home", Some("sdb")), vol("/net", None)];
        tx.send(Msg::Disks(disk::Disks { drives: vec![drive("sda"), drive("sdb")], volumes, note: None })).unwrap();
        a.pump();
        keys(&mut a, "8 j <Enter>");
        assert_eq!((a.focus(), a.selected_volume().map(|v| v.mount.as_str())), (Pane::Mid, Some("/boot")));
        keys(&mut a, "k <Enter>");
        assert_eq!((a.focus(), a.selected_drive().map(|d| d.id.as_str())), (Pane::Left, Some("sda")));
        // A volume on no known drive stays where it is.
        keys(&mut a, "l G <Enter>");
        assert_eq!((a.focus(), a.selected_volume().map(|v| v.mount.as_str())), (Pane::Mid, Some("/net")));
        assert!(a.report().contains("disks: 2 (sda sdb); volumes: 4"));
    }

    #[test]
    fn enter_on_the_sensors_screen_moves_between_a_chip_and_its_sensors() {
        let (mut a, tx, _ctl) = app();
        let temp = |chip: &str, label: &str| sensors::Sensor {
            id: format!("{chip}/{label}"),
            chip_id: chip.into(),
            chip: chip.into(),
            label: label.into(),
            ..sensors::Sensor::default()
        };
        let list = vec![temp("coretemp", "core0"), temp("coretemp", "core1"), temp("nvme", "temp1"), temp("nvme", "temp2")];
        tx.send(Msg::Sensors(sensors::Sensors { list, note: None })).unwrap();
        a.pump();
        // The sensors and their detail follow the chip, and the chip the sensor.
        keys(&mut a, "9 j");
        assert_eq!((a.focus(), a.selected_sensor().map(|s| s.id.as_str())), (Pane::Left, Some("nvme/temp1")));
        keys(&mut a, "<Enter>");
        assert_eq!((a.focus(), a.selected_sensor().map(|s| s.id.as_str())), (Pane::Mid, Some("nvme/temp1")));
        keys(&mut a, "j k k");
        assert_eq!((a.focus(), a.selected_chip().map(|c| c.name)), (Pane::Mid, Some("coretemp")));
        keys(&mut a, "<Enter>");
        assert_eq!((a.focus(), a.selected_chip().map(|c| c.name)), (Pane::Left, Some("coretemp")));
        assert!(a.report().contains("sensors: 4 (temps 4, fans 0; chips 2)"));
    }

    #[test]
    fn sys_opens_on_its_slots_and_the_left_column_skips_blank_facts() {
        let (mut a, tx, _ctl) = app();
        keys(&mut a, "0");
        assert_eq!((a.screen, a.focus(), a.count(Screen::Sys, Pane::Mid)), (Screen::Sys, Pane::Mid, 0));
        let slot = |locator: &str, size| hw::Slot {
            locator: locator.into(),
            present: size > 0,
            size: (size > 0).then_some(size),
            ..hw::Slot::default()
        };
        let cpu =
            hw::Cpu { socket: "AM5".into(), name: "AMD Ryzen 7 7700".into(), cores: 8, threads: 16, max_mhz: 5300, ..hw::Cpu::default() };
        let h = hw::Hardware {
            system: hw::Part { vendor: "ASUS".into(), name: "ROG STRIX B650E-F".into(), ..hw::Part::default() },
            cpus: vec![cpu.clone(), cpu],
            slots: vec![slot("DIMM_A1", 0), slot("DIMM_A2", 16 << 30), slot("DIMM_B1", 0), slot("DIMM_B2", 16 << 30)],
            source: "SMBIOS 3.5 (test)".into(),
            ..hw::Hardware::default()
        };
        tx.send(Msg::Hardware(Box::new(h))).unwrap();
        a.pump();
        keys(&mut a, "j");
        assert_eq!(a.selected_slot().map(|s| s.locator.as_str()), Some("DIMM_A2"));
        use crate::ui::sys::{Line, lines};
        let l = lines(&a);
        // No board facts: no such section, and no blank rows.
        assert!(!l.contains(&Line::Head(tr("sys.board"))));
        assert!(!l.contains(&Line::Kv(tr("sys.version"), String::new())));
        assert!(l.contains(&Line::Kv(tr("sys.slots"), trf("sys.slots_used", &["2", "4"]))));
        assert!(l.contains(&Line::Kv(tr("sys.installed"), "32.0 GiB".into())));
        // Two sockets add up; the caches the firmware leaves out are left out.
        let at = l.iter().position(|x| *x == Line::Head(tr("sys.processor"))).unwrap();
        assert_eq!(
            l[at + 1..at + 6],
            [
                Line::Kv(tr("sys.model"), "AMD Ryzen 7 7700".into()),
                Line::Kv(tr("sys.socket"), trf("sys.sockets", &["2", "AM5"])),
                Line::Kv(tr("sys.cores"), "16".into()),
                Line::Kv(tr("sys.threads"), "32".into()),
                Line::Kv(tr("sys.max_speed"), trf("sys.mhz_firmware", &["5300"])),
            ]
        );
        assert!(!l.iter().any(|x| matches!(x, Line::Kv(k, _) if *k == tr("sys.l1"))));
        // No GPUs or drives read yet: no such sections.
        assert!(!l.contains(&Line::Head(tr("sys.graphics"))));
        assert!(!l.contains(&Line::Head(tr("sys.drives"))));
        let gpu = gpu::Device { name: "Radeon RX 7800 XT".into(), vram_total: Some(16 << 30), ..gpu::Device::default() };
        let npu = gpu::Device { kind: gpu::Kind::Npu, vendor: Some("Intel"), ..gpu::Device::default() };
        tx.send(Msg::Gpu(vec![gpu, npu])).unwrap();
        let drive = disk::Drive { name: "nvme0n1".into(), size: Some(2_000_398_934_016), kind: disk::Kind::Ssd, ..disk::Drive::default() };
        tx.send(Msg::Disks(disk::Disks { drives: vec![drive], ..disk::Disks::default() })).unwrap();
        a.pump();
        let l = lines(&a);
        let at = l.iter().position(|x| *x == Line::Head(tr("sys.graphics"))).unwrap();
        assert_eq!(
            l[at + 1..at + 7],
            [
                Line::Kv(tr("gpu.kind_gpu"), "Radeon RX 7800 XT · 16.0 GiB".into()),
                Line::Kv(tr("gpu.kind_npu"), trf("gpu.unnamed", &["Intel", tr("gpu.kind_npu")])),
                Line::Kv(tr("sys.details"), trf("sys.press", &["4"])),
                Line::Head(tr("sys.drives")),
                Line::Kv(tr("disk.kind.ssd"), "nvme0n1 · 1.8 TiB".into()),
                Line::Kv(tr("sys.details"), trf("sys.press", &["8"])),
            ]
        );
        assert_eq!(a.count(Screen::Sys, Pane::Left), l.len());
        assert!(a.report().contains("hw: ASUS ROG STRIX B650E-F (SMBIOS 3.5 (test)); slots 2 of 4\n"), "{}", a.report());
        // A self-built PC: the system is left blank and the board names it.
        let h = hw::Hardware {
            system: hw::Part { vendor: "ASUS".into(), version: "1.0".into(), ..hw::Part::default() },
            board: hw::Part { vendor: "ASUSTeK COMPUTER INC.".into(), name: "PRIME B650M-A".into(), ..hw::Part::default() },
            ..hw::Hardware::default()
        };
        tx.send(Msg::Hardware(Box::new(h))).unwrap();
        a.pump();
        assert_eq!(crate::ui::sys::machine(&a), "ASUSTeK COMPUTER INC. PRIME B650M-A");
    }

    #[test]
    fn y_ends_it_on_a_thread_and_the_outcome_comes_back_as_a_toast() {
        let (mut a, tx, _ctl) = app();
        // A PID nothing holds: the worker finds it gone and sends nothing.
        tx.send(Msg::Procs { procs: vec![proc(9_999_999, "ghost", 1.0, 1)], total_threads: None }).unwrap();
        a.pump();
        keys(&mut a, "5 l x y");
        assert_eq!((a.mode, a.confirm.clone()), (Mode::Normal, None));
        let until = Instant::now() + Duration::from_secs(10);
        while a.toast.as_ref().is_some_and(|t| t.0.starts_with("Ending")) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
            a.pump();
        }
        assert_eq!(a.toast.map(|t| t.0), Some(trf("toast.gone", &["ghost", "9999999"])));
    }
}
