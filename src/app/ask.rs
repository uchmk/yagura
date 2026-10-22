//! What the confirm question asks, the request sent to another machine for it,
//! and the kind of OS a machine runs.

use crate::collect::containers;
use crate::i18n::trf;
use crate::kill::Target;
use crate::remote::proto;
use crate::svcctl::{self, Action};

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
    pub(super) fn act(&self) -> Option<proto::Act> {
        match self {
            Ask::Kill(t) => Some(proto::Act::Kill { pid: t.pid, start: t.start }),
            Ask::Svc(t) => Some(proto::Act::Service { action: t.action, name: t.name.clone(), driver: t.driver }),
            Ask::Ct(_) => None,
        }
    }

    /// The toast while it runs.
    pub(super) fn starting(&self) -> String {
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
