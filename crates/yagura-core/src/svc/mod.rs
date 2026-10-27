//! Services: Windows' services, systemd's units, launchd's jobs, read on the
//! sampler thread and only while the svc screen shows. Starting and stopping
//! them is `svcctl`'s.
//!
//! Each OS has its own source: the service control manager on Windows,
//! `systemctl` on Linux (unit files alone when systemd is not running) and
//! `launchctl` with launchd's plists on macOS. The parsing is kept in pure functions so the
//! tests run everywhere.

#[cfg(any(target_os = "linux", test))]
pub mod linux;
#[cfg(any(target_os = "macos", test))]
pub mod mac;
#[cfg(windows)]
pub mod scm;
#[cfg(any(windows, test))]
pub mod windows;

use serde::{Deserialize, Serialize};

/// Where a service is, folded from each source's own words.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum State {
    Running,
    // launchctl only knows running or not.
    #[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
    Starting,
    #[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
    Stopping,
    // Only the service control manager reports it.
    #[cfg_attr(not(windows), allow(dead_code))]
    Paused,
    Stopped,
    Failed,
    /// Not known: systemd is not running, only its unit files were read.
    #[default]
    Unknown,
}

impl State {
    pub fn key(self) -> &'static str {
        match self {
            Self::Running => "svc.state.running",
            Self::Starting => "svc.state.starting",
            Self::Stopping => "svc.state.stopping",
            Self::Paused => "svc.state.paused",
            Self::Stopped => "svc.state.stopped",
            Self::Failed => "svc.state.failed",
            Self::Unknown => "svc.state.unknown",
        }
    }
}

/// When it starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Start {
    /// At boot or login.
    Auto,
    /// When asked, or when something else needs it.
    Manual,
    Disabled,
    #[default]
    Unknown,
}

impl Start {
    pub fn key(self) -> &'static str {
        match self {
            Self::Auto => "svc.start.auto",
            Self::Manual => "svc.start.manual",
            Self::Disabled => "svc.start.disabled",
            Self::Unknown => "svc.start.unknown",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Service {
    /// The unit, service or job label; unique within a source.
    pub name: String,
    /// The description or display name; empty when there is none.
    pub display: String,
    pub state: State,
    /// The source's own word for the state, shown as it is: systemd's sub
    /// state (`running`, `exited`, `dead`); empty elsewhere.
    pub sub: String,
    pub start: Start,
    /// The source's own word for the start: `enabled`, `static`, `masked`,
    /// `auto (delayed)`; empty when there is none.
    pub start_raw: String,
    pub pid: Option<u32>,
    /// The last exit code where it says one (launchd, a failed Windows
    /// service); negative for a signal on macOS.
    pub exit: Option<i64>,
    pub user: Option<String>,
    pub cmd: Option<String>,
    /// The unit file or the plist.
    pub file: Option<String>,
    /// A launchd job known only from its plist: starting it loads the plist
    /// first (`launchctl bootstrap`).
    pub unloaded: bool,
    /// A Windows kernel or file-system driver rather than a service; svc
    /// shows one kind at a time (`u`).
    pub driver: bool,
}

/// Why the list is short or empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)]
pub enum Note {
    /// systemd is not PID 1: the unit files are listed, their state unknown.
    NotRunning,
    /// No `systemctl` to ask.
    NoSystemctl,
    /// `launchctl list` as a user shows that user's jobs only.
    MacUser,
    /// The service control manager could not be opened.
    NoScm,
    OtherOs,
}

impl Note {
    pub const ALL: [Note; 5] = [Self::NotRunning, Self::NoSystemctl, Self::MacUser, Self::NoScm, Self::OtherOs];

    pub fn key(self) -> &'static str {
        match self {
            Self::NotRunning => "svc.note.not_running",
            Self::NoSystemctl => "svc.note.no_systemctl",
            Self::MacUser => "svc.note.mac_user",
            Self::NoScm => "svc.note.no_scm",
            Self::OtherOs => "svc.note.other_os",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Services {
    /// By name.
    pub list: Vec<Service>,
    /// `systemd`, `unit files`, `scm`, `launchctl`.
    #[serde(deserialize_with = "crate::wire::name")]
    pub source: crate::Name,
    pub note: Option<Note>,
}

impl Services {
    /// Failed services, leaving out drivers: svc lists them apart (`u`) and
    /// its crumb counts them apart.
    pub fn failed_services(&self) -> usize {
        self.list.iter().filter(|v| !v.driver && v.state == State::Failed).count()
    }
}

/// Reads the services; it keeps Windows' configuration a while and
/// macOS's plists while they are unchanged.
#[derive(Default)]
pub struct Reader {
    #[cfg(windows)]
    cache: scm::Cache,
    #[cfg(target_os = "macos")]
    plists: mac::Cache,
}

impl Reader {
    /// Every service the OS lets this user see, by name.
    pub fn read(&mut self) -> Services {
        let mut out = self.read_os();
        out.list.sort_by_cached_key(|s| s.name.to_lowercase());
        out
    }

    #[cfg(target_os = "linux")]
    fn read_os(&mut self) -> Services {
        linux::read()
    }

    #[cfg(windows)]
    fn read_os(&mut self) -> Services {
        scm::read(&mut self.cache)
    }

    #[cfg(target_os = "macos")]
    fn read_os(&mut self) -> Services {
        mac::read(&mut self.plists)
    }

    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    fn read_os(&mut self) -> Services {
        Services { list: Vec::new(), source: "", note: Some(Note::OtherOs) }
    }
}

/// What a service (or container) can be asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Start,
    Stop,
    Restart,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::tr;

    #[test]
    fn every_state_start_and_note_has_english() {
        let states = [State::Running, State::Starting, State::Stopping, State::Paused, State::Stopped, State::Failed, State::Unknown];
        for s in states {
            assert_ne!(tr(s.key()), s.key(), "{s:?}");
        }
        for s in [Start::Auto, Start::Manual, Start::Disabled, Start::Unknown] {
            assert_ne!(tr(s.key()), s.key(), "{s:?}");
        }
        for n in Note::ALL {
            assert_ne!(tr(n.key()), n.key(), "{n:?}");
        }
    }
}
