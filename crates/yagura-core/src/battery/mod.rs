//! A laptop's battery, read with the sensors every interval: `None` on a
//! machine without one. Each OS reads it its own way; turning what it says
//! into a [`Battery`] is pure, so every OS tests it.

#[cfg(any(target_os = "linux", test))]
mod linux;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(any(windows, test))]
mod windows;

use serde::{Deserialize, Serialize};

/// The charge and whether mains power is in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Battery {
    /// Charge, percent of full.
    pub pct: f32,
    pub charging: bool,
    /// On mains power (charging or full).
    pub plugged: bool,
}

#[cfg(target_os = "linux")]
pub fn read() -> Option<Battery> {
    linux::read()
}

#[cfg(windows)]
pub fn read() -> Option<Battery> {
    windows::read()
}

#[cfg(target_os = "macos")]
pub fn read() -> Option<Battery> {
    mac::read()
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
pub fn read() -> Option<Battery> {
    None
}

/// macOS's `CurrentCapacity` of `MaxCapacity`: Apple silicon says the charge
/// in percent (max 100), Intel Macs in mAh.
#[cfg(any(target_os = "macos", test))]
fn from_capacity(cur: Option<i64>, max: Option<i64>, charging: bool, external: bool) -> Option<Battery> {
    let (cur, max) = (cur?, max?);
    if max <= 0 {
        return None;
    }
    let pct = (cur.clamp(0, max) as f32 * 100.0 / max as f32).round();
    Some(Battery { pct, charging, plugged: external || charging })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_capacities_read_as_percent() {
        assert_eq!(from_capacity(Some(87), Some(100), false, false), Some(Battery { pct: 87.0, charging: false, plugged: false }));
        assert_eq!(from_capacity(Some(2500), Some(5000), true, true), Some(Battery { pct: 50.0, charging: true, plugged: true }));
        assert_eq!(from_capacity(Some(100), Some(100), false, true), Some(Battery { pct: 100.0, charging: false, plugged: true }));
        assert_eq!(from_capacity(None, Some(100), false, false), None);
        assert_eq!(from_capacity(Some(5), Some(0), false, false), None);
    }
}
