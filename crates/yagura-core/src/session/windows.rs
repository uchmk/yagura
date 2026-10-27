//! Windows: the logins from Remote Desktop Services' session list, the power
//! plan from `powrprof`.

#[cfg(windows)]
use super::Session;
use super::User;

#[cfg(windows)]
pub fn read() -> Session {
    Session { users: users(), power: plan().unwrap_or_default() }
}

/// A session with a user is a login; the services' session 0 and the
/// listeners have none.
pub fn login(name: &str, station: &str) -> Option<User> {
    let name = name.trim();
    (!name.is_empty()).then(|| User { name: name.to_string(), line: station.trim().to_string(), host: String::new(), since: 0 })
}

#[cfg(windows)]
fn users() -> Vec<User> {
    use ::windows::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW, WTSEnumerateSessionsW, WTSFreeMemory, WTSQuerySessionInformationW, WTSUserName,
    };
    use ::windows::core::PWSTR;

    let mut list: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: out-parameters for the call to fill; the list is freed below.
    if unsafe { WTSEnumerateSessionsW(Some(WTS_CURRENT_SERVER_HANDLE), 0, 1, &mut list, &mut count) }.is_err() || list.is_null() {
        return Vec::new();
    }
    // SAFETY: the list is `count` WTS_SESSION_INFOW.
    let rows = unsafe { std::slice::from_raw_parts(list, count as usize) };
    let mut out = Vec::new();
    for r in rows {
        let mut name = PWSTR::null();
        let mut bytes = 0u32;
        // SAFETY: the buffer it fills is freed right after it is copied.
        let user =
            if unsafe { WTSQuerySessionInformationW(Some(WTS_CURRENT_SERVER_HANDLE), r.SessionId, WTSUserName, &mut name, &mut bytes) }
                .is_ok()
                && !name.is_null()
            {
                // SAFETY: a NUL-terminated string from the call above.
                let s = unsafe { name.to_string() }.unwrap_or_default();
                // SAFETY: the buffer WTS allocated.
                unsafe { WTSFreeMemory(name.0 as _) };
                s
            } else {
                String::new()
            };
        // SAFETY: a NUL-terminated name inside the list, or null.
        let station =
            if r.pWinStationName.is_null() { String::new() } else { unsafe { r.pWinStationName.to_string() }.unwrap_or_default() };
        out.extend(login(&user, &station));
    }
    // SAFETY: the list from the call above, freed once.
    unsafe { WTSFreeMemory(list as _) };
    out
}

#[cfg(windows)]
fn plan() -> Option<String> {
    use ::windows::Win32::Foundation::{ERROR_SUCCESS, HLOCAL, LocalFree};
    use ::windows::Win32::System::Power::{PowerGetActiveScheme, PowerReadFriendlyName};
    use ::windows::core::GUID;

    let mut guid: *mut GUID = std::ptr::null_mut();
    // SAFETY: fills a GUID that LocalFree releases.
    if unsafe { PowerGetActiveScheme(None, &mut guid) } != ERROR_SUCCESS || guid.is_null() {
        return None;
    }
    let mut size = 0u32;
    // SAFETY: the first call only asks how many bytes the name takes.
    let _ = unsafe { PowerReadFriendlyName(None, Some(guid), None, None, None, &mut size) };
    let mut buf = vec![0u8; size as usize];
    // SAFETY: a buffer of the size it asked for.
    let ok = size > 0 && unsafe { PowerReadFriendlyName(None, Some(guid), None, None, Some(buf.as_mut_ptr()), &mut size) } == ERROR_SUCCESS;
    // SAFETY: the GUID PowerGetActiveScheme allocated.
    unsafe { LocalFree(Some(HLOCAL(guid as _))) };
    if !ok {
        return None;
    }
    let wide: Vec<u16> = buf.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).take_while(|&c| c != 0).collect();
    Some(String::from_utf16_lossy(&wide)).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_without_a_user_is_no_login() {
        assert_eq!(login("", "Services"), None);
        assert_eq!(login("  ", "RDP-Tcp"), None);
        assert_eq!(login("ann", "Console "), Some(User { name: "ann".into(), line: "Console".into(), host: String::new(), since: 0 }));
    }
}
