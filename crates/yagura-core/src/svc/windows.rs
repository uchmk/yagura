//! The service control manager's numbers in yagura's words: pure, so the
//! tests run everywhere. The calls are in `scm.rs`.

use super::{Start, State};

/// ERROR_SERVICE_NEVER_STARTED: stopped, not failed.
const NEVER_STARTED: u32 = 1077;
/// ERROR_SERVICE_SPECIFIC_ERROR: the service's own code says why.
const SPECIFIC: u32 = 1066;

/// `dwCurrentState`, with `dwWin32ExitCode` to tell a failed stop from a clean one.
pub fn state(current: u32, exit: u32) -> State {
    match current {
        1 if failed(exit) => State::Failed,
        1 => State::Stopped,
        2 | 5 => State::Starting,
        3 | 6 => State::Stopping,
        4 => State::Running,
        7 => State::Paused,
        _ => State::Unknown,
    }
}

fn failed(exit: u32) -> bool {
    exit != 0 && exit != NEVER_STARTED
}

/// The code to show for a stopped service: the service's own when it gave one.
pub fn exit(current: u32, exit: u32, specific: u32) -> Option<i64> {
    if current != 1 || !failed(exit) {
        return None;
    }
    Some(if exit == SPECIFIC { specific as i64 } else { exit as i64 })
}

/// `dwStartType` and the delayed flag, folded and in words.
/// Whether a service type is a driver (kernel, file system, adapter or
/// recognizer) rather than a Win32 service.
pub fn driver(kind: u32) -> bool {
    kind & 0x0F != 0 && kind & 0x30 == 0
}

pub fn start(kind: u32, delayed: bool) -> (Start, String) {
    let (s, raw) = match kind {
        0 => (Start::Auto, "boot"),
        1 => (Start::Auto, "system"),
        2 if delayed => (Start::Auto, "auto (delayed)"),
        2 => (Start::Auto, "auto"),
        3 => (Start::Manual, "manual"),
        4 => (Start::Disabled, "disabled"),
        _ => (Start::Unknown, ""),
    };
    (s, raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scm_numbers_fold_into_states_and_starts() {
        assert_eq!(state(4, 0), State::Running);
        assert_eq!(state(1, 0), State::Stopped);
        assert_eq!(state(1, NEVER_STARTED), State::Stopped);
        assert_eq!(state(1, 5), State::Failed);
        assert_eq!(state(2, 0), State::Starting);
        assert_eq!(state(6, 0), State::Stopping);
        assert_eq!(state(7, 0), State::Paused);
        assert_eq!(state(9, 0), State::Unknown);
        assert_eq!(exit(1, SPECIFIC, 42), Some(42));
        assert_eq!(exit(1, 5, 0), Some(5));
        assert_eq!(exit(1, NEVER_STARTED, 0), None);
        assert_eq!(exit(4, 5, 0), None);
        assert_eq!(start(2, true), (Start::Auto, "auto (delayed)".to_string()));
        assert_eq!(start(3, false).0, Start::Manual);
        assert_eq!(start(4, false).0, Start::Disabled);
        assert_eq!(start(0, false), (Start::Auto, "boot".to_string()));
    }

    #[test]
    fn drivers_are_told_from_services_by_their_type() {
        // Kernel, file system; an own-process and a shared-process service,
        // and one that may talk to the desktop.
        assert!(driver(0x01) && driver(0x02));
        assert!(!driver(0x10) && !driver(0x20) && !driver(0x110));
        assert!(!driver(0x50) && !driver(0xE0), "user services are services");
    }
}
