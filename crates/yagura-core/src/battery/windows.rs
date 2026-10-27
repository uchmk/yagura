//! Windows: `GetSystemPowerStatus`.

use super::Battery;

#[cfg(windows)]
pub fn read() -> Option<Battery> {
    use ::windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

    let mut s = SYSTEM_POWER_STATUS::default();
    // SAFETY: writes the struct it is given.
    unsafe { GetSystemPowerStatus(&mut s) }.ok()?;
    from_status(s.ACLineStatus, s.BatteryFlag, s.BatteryLifePercent)
}

/// `ACLineStatus` (1 online, 255 unknown), `BatteryFlag` (8 charging, 128
/// no battery, 255 unknown) and `BatteryLifePercent` (255 unknown).
pub fn from_status(ac: u8, flag: u8, pct: u8) -> Option<Battery> {
    if flag == 255 || flag & 128 != 0 || pct > 100 {
        return None;
    }
    let charging = flag & 8 != 0;
    Some(Battery { pct: f32::from(pct), charging, plugged: ac == 1 || charging })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_power_status_reads_as_a_battery() {
        assert_eq!(from_status(1, 128, 255), None, "no battery");
        assert_eq!(from_status(255, 255, 255), None, "unknown");
        assert_eq!(from_status(0, 1, 87), Some(Battery { pct: 87.0, charging: false, plugged: false }));
        assert_eq!(from_status(1, 8 | 2, 30), Some(Battery { pct: 30.0, charging: true, plugged: true }));
        assert_eq!(from_status(1, 1, 100), Some(Battery { pct: 100.0, charging: false, plugged: true }));
        assert_eq!(from_status(1, 0, 255), None, "a battery that says no charge");
    }
}
