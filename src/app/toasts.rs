//! The toast text for each outcome: a priority, CPUs, a process, a service or a
//! container that was moved, or could not be.

use super::Family;
use crate::collect::containers;
use crate::i18n::trf;
use crate::kill::{Outcome, Target};
use crate::svcctl::{self, Action};

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
