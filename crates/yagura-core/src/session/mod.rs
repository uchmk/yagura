//! Who is logged in and the power plan, read every [`EVERY`] on the sampler's
//! thread. Each OS reads them its own way; turning what it says into a
//! [`Session`] is pure, so every OS tests it.

#[cfg(any(target_os = "linux", test))]
mod linux;
#[cfg(any(target_os = "macos", test))]
mod mac;
#[cfg(any(windows, test))]
mod windows;

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How often the logins and the power plan are read again.
pub const EVERY: Duration = Duration::from_secs(15);

/// One login.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub name: String,
    /// The terminal or session: `tty1`, `pts/0`, `console`, `RDP-Tcp#0`.
    pub line: String,
    /// Where it came from, when it came over the network.
    pub host: String,
    /// Seconds since the epoch; 0 when the OS does not say.
    pub since: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub users: Vec<User>,
    /// The power plan, profile or governor; empty where there is none.
    pub power: String,
}

#[cfg(target_os = "linux")]
pub fn read() -> Session {
    linux::read()
}

#[cfg(windows)]
pub fn read() -> Session {
    windows::read()
}

#[cfg(target_os = "macos")]
pub fn read() -> Session {
    mac::read()
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
pub fn read() -> Session {
    Session::default()
}
