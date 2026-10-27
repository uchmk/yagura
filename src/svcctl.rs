//! Starting, stopping and restarting a service: the confirmed `S`, `x` and
//! `R` on the svc screen. Like ending a process, each request runs on its own
//! short-lived thread and reports back through a channel, waking the UI.
//!
//! Windows asks the service control manager; Linux runs `systemctl`, which
//! may ask polkit for a password on a desktop; macOS runs `launchctl` on the
//! job in the user's domain, or the system's as root. How each OS words a
//! refusal is read by a pure function, so the tests run everywhere.
//!
//! A launchd job that is not loaded is loaded from its plist first
//! (`launchctl bootstrap`). A disabled one is left alone: enabling it changes
//! the owner's settings, so the toast only says the command.

use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};

use crate::collect::svc::State;

pub use yagura_core::svc::Action;

#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub action: Action,
    /// The unit, service or job label.
    pub name: String,
    /// The plist of a launchd job that is not loaded, loaded before it is
    /// started.
    pub plist: Option<String>,
    /// A Windows driver: shown as `sc.exe`, which reaches drivers, where
    /// PowerShell's `*-Service` does not.
    pub driver: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    Done,
    /// The OS wants more rights than yagura has.
    Denied,
    /// The OS refused, in its own words.
    Failed(String),
}

/// Whether asking makes sense: a running service is not started again, a
/// stopped one not stopped. Restart is asked in any state.
pub fn fits(action: Action, state: State) -> bool {
    match action {
        Action::Start => !matches!(state, State::Running | State::Starting),
        Action::Stop => !matches!(state, State::Stopped | State::Failed),
        Action::Restart => true,
    }
}

/// Run `t` on a thread of its own; the outcome comes back on `tx`.
pub fn spawn(t: Target, tx: Sender<(Target, Outcome)>, wake: Option<egui::Context>) {
    let (t2, tx2) = (t.clone(), tx.clone());
    let started = std::thread::Builder::new().name("yagura-svc".into()).spawn(move || {
        let o = run(&t);
        let _ = tx.send((t, o));
        if let Some(w) = wake {
            w.request_repaint();
        }
    });
    if let Err(e) = started {
        let _ = tx2.send((t2, Outcome::Failed(e.to_string())));
    }
}

/// The command line that does the same, shown before it is asked.
pub fn how(t: &Target) -> String {
    #[cfg(target_os = "linux")]
    return format!("systemctl {}", linux::args(t.action, &t.name).join(" "));
    #[cfg(target_os = "macos")]
    return mac::how(&mac::steps(t, mac::root(), mac::uid()));
    #[cfg(windows)]
    return win::how(t.action, &t.name, t.driver);
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    return t.name.clone();
}

/// The command that turns a disabled launchd job back on, for the toast.
pub fn enable_line(name: &str) -> String {
    #[cfg(target_os = "macos")]
    return format!("launchctl enable {}", mac::target(mac::root(), mac::uid(), name));
    #[cfg(not(target_os = "macos"))]
    return format!("launchctl enable {name}");
}

/// The last line the command wrote to stderr, or its exit status.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn reason(stderr: &str, status: Option<i32>) -> String {
    stderr.lines().map(str::trim).rfind(|l| !l.is_empty()).map_or_else(|| format!("exit status {}", status.unwrap_or(-1)), str::to_string)
}

/// Run `prog args`, reading a refusal from its stderr with `denied`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn command(prog: &str, args: &[String], denied: fn(&str) -> bool) -> Outcome {
    use std::process::{Command, Stdio};

    let out = Command::new(prog).args(args).env("LC_ALL", "C").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).output();
    match out {
        Ok(o) if o.status.success() => Outcome::Done,
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            if denied(&err) { Outcome::Denied } else { Outcome::Failed(reason(&err, o.status.code())) }
        }
        Err(e) => Outcome::Failed(format!("{prog}: {e}")),
    }
}

/// Run `t` and wait for it. Blocks.
#[cfg(target_os = "linux")]
pub fn run(t: &Target) -> Outcome {
    command("systemctl", &linux::args(t.action, &t.name), linux::denied)
}

#[cfg(target_os = "macos")]
pub fn run(t: &Target) -> Outcome {
    let steps = mac::steps(t, mac::root(), mac::uid());
    mac::settle(steps.iter().map(|s| command("launchctl", s, mac::denied)).collect())
}

#[cfg(windows)]
pub fn run(t: &Target) -> Outcome {
    win::run(t)
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn run(_: &Target) -> Outcome {
    Outcome::Failed("not supported on this OS".into())
}

/// systemd: `systemctl start|stop|restart <unit>`.
#[cfg(any(target_os = "linux", test))]
mod linux {
    use super::Action;

    pub fn args(action: Action, unit: &str) -> Vec<String> {
        let verb = match action {
            Action::Start => "start",
            Action::Stop => "stop",
            Action::Restart => "restart",
        };
        vec![verb.into(), unit.into()]
    }

    /// systemctl's words when polkit or D-Bus says no.
    pub fn denied(stderr: &str) -> bool {
        let s = stderr.to_lowercase();
        ["access denied", "authentication required", "permission denied"].iter().any(|w| s.contains(w))
    }
}

/// launchd: `launchctl kickstart [-k]` and `launchctl kill SIGTERM` on the job
/// in the domain `launchctl list` showed it from; `launchctl bootstrap` first
/// for one that is not loaded.
#[cfg(any(target_os = "macos", test))]
mod mac {
    use super::{Action, Outcome, Target};

    #[cfg(target_os = "macos")]
    pub use crate::collect::svc::mac::{root, uid};

    /// The service target: the system domain as root, the user's GUI domain
    /// otherwise, as `launchctl list` lists them.
    pub fn target(root: bool, uid: u32, label: &str) -> String {
        format!("{}/{label}", domain(root, uid))
    }

    pub fn domain(root: bool, uid: u32) -> String {
        if root { "system".to_string() } else { format!("gui/{uid}") }
    }

    /// The `launchctl` runs for `t`: a job that is not loaded is loaded from
    /// its plist and then kicked, whether it was asked to start or restart.
    pub fn steps(t: &Target, root: bool, uid: u32) -> Vec<Vec<String>> {
        let target = target(root, uid, &t.name);
        match &t.plist {
            Some(p) if t.action != Action::Stop => {
                vec![vec!["bootstrap".to_string(), domain(root, uid), p.clone()], args(Action::Start, &target)]
            }
            _ => vec![args(t.action, &target)],
        }
    }

    /// The steps as one shell line.
    pub fn how(steps: &[Vec<String>]) -> String {
        let quote = |a: &String| if a.contains([' ', '\'', '"']) { format!("'{}'", a.replace('\'', "'\\''")) } else { a.clone() };
        steps.iter().map(|s| format!("launchctl {}", s.iter().map(quote).collect::<Vec<_>>().join(" "))).collect::<Vec<_>>().join(" && ")
    }

    /// One outcome for the steps: done when the last one is, else the first
    /// that went wrong. A bootstrap that fails because another load won the
    /// race still counts when the kick after it works.
    pub fn settle(outcomes: Vec<Outcome>) -> Outcome {
        if outcomes.last().is_none_or(|o| *o == Outcome::Done) {
            return Outcome::Done;
        }
        outcomes.into_iter().find(|o| *o != Outcome::Done).unwrap_or(Outcome::Done)
    }

    pub fn args(action: Action, target: &str) -> Vec<String> {
        let a: &[&str] = match action {
            Action::Start => &["kickstart"],
            Action::Restart => &["kickstart", "-k"],
            Action::Stop => &["kill", "SIGTERM"],
        };
        a.iter().map(|s| s.to_string()).chain(Some(target.to_string())).collect()
    }

    pub fn denied(stderr: &str) -> bool {
        let s = stderr.to_lowercase();
        ["not permitted", "not privileged", "permission"].iter().any(|w| s.contains(w))
    }
}

/// The service control manager.
#[cfg(any(windows, test))]
mod win {
    use super::{Action, Outcome};

    const ERROR_ACCESS_DENIED: u32 = 5;
    const ERROR_SERVICE_ALREADY_RUNNING: u32 = 1056;
    const ERROR_SERVICE_NOT_ACTIVE: u32 = 1062;

    /// What a Win32 error from starting (`start`) or stopping a service means:
    /// one already where it was asked to go is done.
    pub fn fold(start: bool, code: u32, msg: String) -> Outcome {
        match code {
            ERROR_ACCESS_DENIED => Outcome::Denied,
            ERROR_SERVICE_ALREADY_RUNNING if start => Outcome::Done,
            ERROR_SERVICE_NOT_ACTIVE if !start => Outcome::Done,
            _ => Outcome::Failed(msg),
        }
    }

    /// The PowerShell line that does the same: `sc.exe` for a driver.
    pub fn how(action: Action, name: &str, driver: bool) -> String {
        if driver {
            let name = if name.contains(' ') { format!("\"{name}\"") } else { name.to_string() };
            return match action {
                Action::Start => format!("sc.exe start {name}"),
                Action::Stop => format!("sc.exe stop {name}"),
                Action::Restart => format!("sc.exe stop {name}; sc.exe start {name}"),
            };
        }
        let verb = match action {
            Action::Start => "Start",
            Action::Stop => "Stop",
            Action::Restart => "Restart",
        };
        let name = if name.contains([' ', '\'']) { format!("'{}'", name.replace('\'', "''")) } else { name.to_string() };
        format!("{verb}-Service {name}")
    }

    #[cfg(windows)]
    pub fn run(t: &super::Target) -> Outcome {
        use std::time::{Duration, Instant};

        use windows::Win32::System::Services::{
            ControlService, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_MANAGER_CONNECT, SERVICE_CONTROL_STOP,
            SERVICE_QUERY_STATUS, SERVICE_START, SERVICE_STATUS, SERVICE_STOP, SERVICE_STOPPED, StartServiceW,
        };
        use windows::core::PCWSTR;

        use crate::collect::svc::scm::Handle;

        const WAIT: Duration = Duration::from_secs(30);
        const POLL: Duration = Duration::from_millis(250);

        /// The Win32 code inside the error, or the HRESULT as it is.
        fn code(e: &windows::core::Error) -> u32 {
            let h = e.code().0 as u32;
            if h & 0xFFFF_0000 == 0x8007_0000 { h & 0xFFFF } else { h }
        }

        let access = match t.action {
            Action::Start => SERVICE_START,
            Action::Stop => SERVICE_STOP,
            Action::Restart => SERVICE_START | SERVICE_STOP,
        } | SERVICE_QUERY_STATUS;
        // SAFETY: plain calls; the handles are closed by `Handle`.
        let scm = match unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) } {
            Ok(h) => Handle(h),
            Err(e) => return fold(true, code(&e), e.message()),
        };
        let wide: Vec<u16> = t.name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let svc = match unsafe { OpenServiceW(scm.0, PCWSTR(wide.as_ptr()), access) } {
            Ok(h) => Handle(h),
            Err(e) => return fold(true, code(&e), e.message()),
        };
        let start = || {
            // SAFETY: a handle opened with SERVICE_START.
            match unsafe { StartServiceW(svc.0, None) } {
                Ok(()) => Outcome::Done,
                Err(e) => fold(true, code(&e), e.message()),
            }
        };
        let stop = || {
            let mut st = SERVICE_STATUS::default();
            // SAFETY: a handle opened with SERVICE_STOP; `st` outlives the call.
            match unsafe { ControlService(svc.0, SERVICE_CONTROL_STOP, &mut st) } {
                Ok(()) => Outcome::Done,
                Err(e) => fold(false, code(&e), e.message()),
            }
        };
        match t.action {
            Action::Start => start(),
            Action::Stop => stop(),
            Action::Restart => {
                let o = stop();
                if o != Outcome::Done {
                    return o;
                }
                let until = Instant::now() + WAIT;
                loop {
                    let mut st = SERVICE_STATUS::default();
                    // SAFETY: a handle opened with SERVICE_QUERY_STATUS.
                    if let Err(e) = unsafe { QueryServiceStatus(svc.0, &mut st) } {
                        return Outcome::Failed(e.message());
                    }
                    if st.dwCurrentState == SERVICE_STOPPED {
                        break;
                    }
                    if Instant::now() >= until {
                        return Outcome::Failed(format!("still stopping after {} s", WAIT.as_secs()));
                    }
                    std::thread::sleep(POLL);
                }
                start()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_change_is_asked() {
        assert!(!fits(Action::Start, State::Running));
        assert!(!fits(Action::Start, State::Starting));
        assert!(fits(Action::Start, State::Failed));
        assert!(fits(Action::Start, State::Unknown));
        assert!(!fits(Action::Stop, State::Stopped));
        assert!(!fits(Action::Stop, State::Failed));
        assert!(fits(Action::Stop, State::Paused));
        assert!(fits(Action::Restart, State::Stopped));
        assert!(fits(Action::Restart, State::Running));
    }

    #[test]
    fn systemctl_lines_and_refusals() {
        assert_eq!(linux::args(Action::Restart, "cron.service"), ["restart", "cron.service"]);
        assert!(linux::denied(
            "Failed to stop cron.service: Access denied\nSee system logs and 'systemctl status cron.service' for details."
        ));
        assert!(linux::denied("Failed to start cron.service: Interactive authentication required."));
        assert!(!linux::denied("Failed to start nope.service: Unit nope.service not found."));
        assert_eq!(
            reason("Failed to start nope.service: Unit nope.service not found.\n\n", Some(5)),
            "Failed to start nope.service: Unit nope.service not found."
        );
        assert_eq!(reason("", Some(5)), "exit status 5");
    }

    #[test]
    fn launchctl_targets_the_domain_the_list_came_from() {
        assert_eq!(mac::target(false, 501, "com.example.agent"), "gui/501/com.example.agent");
        assert_eq!(mac::target(true, 0, "com.example.daemon"), "system/com.example.daemon");
        assert_eq!(mac::args(Action::Restart, "gui/501/x"), ["kickstart", "-k", "gui/501/x"]);
        assert_eq!(mac::args(Action::Stop, "gui/501/x"), ["kill", "SIGTERM", "gui/501/x"]);
        assert!(mac::denied("Could not kickstart service \"x\": 1: Operation not permitted"));
        assert!(!mac::denied("Could not find service \"x\" in domain for user gui: 501"));
    }

    #[test]
    fn a_job_that_is_not_loaded_is_bootstrapped_from_its_plist_then_kicked() {
        let t =
            |action, plist: Option<&str>| Target { action, name: "com.example.sync".into(), plist: plist.map(String::from), driver: false };
        let plist = "/Users/me/Library/LaunchAgents/My Sync.plist";
        let steps = mac::steps(&t(Action::Restart, Some(plist)), false, 501);
        assert_eq!(steps, [vec!["bootstrap", "gui/501", plist], vec!["kickstart", "gui/501/com.example.sync"]]);
        assert_eq!(
            mac::how(&steps),
            "launchctl bootstrap gui/501 '/Users/me/Library/LaunchAgents/My Sync.plist' && launchctl kickstart gui/501/com.example.sync"
        );
        assert_eq!(mac::steps(&t(Action::Start, None), true, 0), [vec!["kickstart", "system/com.example.sync"]]);
        assert_eq!(mac::steps(&t(Action::Stop, Some(plist)), false, 501), [vec!["kill", "SIGTERM", "gui/501/com.example.sync"]]);
        let failed = || Outcome::Failed("Bootstrap failed: 5: Input/output error".into());
        assert_eq!(mac::settle(vec![failed(), Outcome::Done]), Outcome::Done);
        assert_eq!(mac::settle(vec![failed(), Outcome::Failed("not found".into())]), failed());
        assert_eq!(mac::settle(vec![Outcome::Done, Outcome::Denied]), Outcome::Denied);
        assert_eq!(mac::settle(vec![Outcome::Done]), Outcome::Done);
    }

    #[test]
    fn scm_errors_fold_into_outcomes() {
        assert_eq!(win::fold(true, 5, "denied".into()), Outcome::Denied);
        assert_eq!(win::fold(true, 1056, "running".into()), Outcome::Done);
        assert_eq!(win::fold(false, 1062, "not active".into()), Outcome::Done);
        assert_eq!(win::fold(false, 1056, "running".into()), Outcome::Failed("running".into()));
        assert_eq!(win::fold(false, 1051, "dependents".into()), Outcome::Failed("dependents".into()));
        assert_eq!(win::how(Action::Restart, "Spooler", false), "Restart-Service Spooler");
        assert_eq!(win::how(Action::Stop, "My Svc", false), "Stop-Service 'My Svc'");
        assert_eq!(win::how(Action::Stop, "Beep", true), "sc.exe stop Beep");
        assert_eq!(win::how(Action::Restart, "My Drv", true), "sc.exe stop \"My Drv\"; sc.exe start \"My Drv\"");
    }
}
