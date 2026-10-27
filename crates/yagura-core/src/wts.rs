//! Windows: every process as Remote Desktop Services lists it. The owner's
//! SID and the working set come without opening the process, so they are
//! there for other users' processes (SYSTEM, LOCAL SERVICE, dwm) when yagura
//! is not elevated and sysinfo cannot read them. [`super::fill`] lays them
//! over what sysinfo gave.

use std::collections::HashMap;

use windows::Win32::Security::{GetLengthSid, IsValidSid, LookupAccountSidW, PSID, SID_NAME_USE};
use windows::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_PROCESS_INFO_EXW, WTSEnumerateProcessesExW, WTSFreeMemoryExW, WTSTypeProcessInfoLevel1,
};
use windows::core::{PCWSTR, PWSTR};

use super::Seen;

/// Every session's processes (`WTS_ANY_SESSION`; not in the bindings).
const ANY_SESSION: u32 = 0xFFFF_FFFE;

#[derive(Default)]
pub struct Wts {
    /// A SID's bytes → its account name; `None` where it has none.
    names: HashMap<Vec<u8>, Option<String>>,
}

impl Wts {
    /// Account names are looked up again after this.
    pub fn forget(&mut self) {
        self.names.clear();
    }

    /// pid → what WTS knows of it; empty when the call fails.
    pub fn read(&mut self) -> HashMap<u32, Seen> {
        let mut level = 1u32;
        let mut list = PWSTR::null();
        let mut count = 0u32;
        // SAFETY: out-parameters for the call to fill; the list is freed below.
        if unsafe { WTSEnumerateProcessesExW(Some(WTS_CURRENT_SERVER_HANDLE), &mut level, ANY_SESSION, &mut list, &mut count) }.is_err()
            || list.is_null()
        {
            return HashMap::new();
        }
        // SAFETY: at level 1 the list is `count` WTS_PROCESS_INFO_EXW.
        let rows = unsafe { std::slice::from_raw_parts(list.0 as *const WTS_PROCESS_INFO_EXW, count as usize) };
        let mut out = HashMap::with_capacity(rows.len());
        for r in rows {
            // SAFETY: a NUL-terminated name inside the list, or null.
            let name = if r.pProcessName.is_null() { String::new() } else { unsafe { r.pProcessName.to_string() }.unwrap_or_default() };
            out.insert(r.ProcessId, Seen { name, user: self.name(r.pUserSid), mem: u64::from(r.WorkingSetSize) });
        }
        // SAFETY: the list from the call above, freed once with its count.
        let _ = unsafe { WTSFreeMemoryExW(WTSTypeProcessInfoLevel1, list.0 as _, count) };
        out
    }

    fn name(&mut self, sid: PSID) -> Option<String> {
        // SAFETY: a SID inside the list, checked before it is read.
        if sid.is_invalid() || !unsafe { IsValidSid(sid) }.as_bool() {
            return None;
        }
        // SAFETY: a valid SID is GetLengthSid bytes long.
        let key = unsafe { std::slice::from_raw_parts(sid.0 as *const u8, GetLengthSid(sid) as usize) }.to_vec();
        self.names.entry(key).or_insert_with(|| lookup(sid)).clone()
    }
}

/// The account's name without its domain (`SYSTEM`, `LOCAL SERVICE`), as
/// sysinfo gives it for the user's own.
fn lookup(sid: PSID) -> Option<String> {
    // Account and domain names are at most 256 characters.
    let mut name = [0u16; 257];
    let mut domain = [0u16; 257];
    let (mut n, mut d) = (name.len() as u32, domain.len() as u32);
    let mut kind = SID_NAME_USE::default();
    // SAFETY: the buffers and their lengths, as the call asks.
    unsafe {
        LookupAccountSidW(PCWSTR::null(), sid, Some(PWSTR(name.as_mut_ptr())), &mut n, Some(PWSTR(domain.as_mut_ptr())), &mut d, &mut kind)
    }
    .ok()?;
    let name = String::from_utf16_lossy(&name[..(n as usize).min(name.len())]);
    (!name.is_empty()).then_some(name)
}
