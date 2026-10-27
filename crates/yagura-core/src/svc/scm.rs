//! The service control manager: every Win32 service and driver with its
//! state and PID in one enumeration, then each one's configuration (start type, account,
//! command line). The configuration seldom changes and costs a handle per
//! service, so it is kept for a minute.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::ERROR_MORE_DATA;
use windows::Win32::System::Services::{
    CloseServiceHandle, ENUM_SERVICE_STATUS_PROCESSW, EnumServicesStatusExW, OpenSCManagerW, OpenServiceW, QUERY_SERVICE_CONFIGW,
    QueryServiceConfig2W, QueryServiceConfigW, SC_ENUM_PROCESS_INFO, SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_ENUMERATE_SERVICE,
    SERVICE_CONFIG_DELAYED_AUTO_START_INFO, SERVICE_DELAYED_AUTO_START_INFO, SERVICE_DRIVER, SERVICE_QUERY_CONFIG, SERVICE_STATE_ALL,
    SERVICE_WIN32,
};
use windows::core::{PCWSTR, PWSTR};

use super::{Note, Service, Services, Start, windows as fold};

const KEEP: Duration = Duration::from_secs(60);

#[derive(Clone, Default)]
struct Config {
    start: Start,
    start_raw: String,
    user: Option<String>,
    cmd: Option<String>,
}

#[derive(Default)]
pub struct Cache {
    configs: HashMap<String, Config>,
    at: Option<Instant>,
}

/// A buffer of `n` bytes at least, aligned for the structs the calls write.
fn buffer(n: u32) -> Vec<u64> {
    vec![0u64; (n as usize).div_ceil(8).max(1)]
}

fn bytes(b: &mut [u64]) -> &mut [u8] {
    // SAFETY: the same memory as bytes; u8 has no alignment or invalid values.
    unsafe { std::slice::from_raw_parts_mut(b.as_mut_ptr().cast::<u8>(), b.len() * 8) }
}

fn text(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: a NUL-terminated string inside a buffer the SCM filled.
    unsafe { p.to_string() }.unwrap_or_default()
}

fn some(s: String) -> Option<String> {
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Close the handle when dropped. `svcctl` opens services with it too.
pub struct Handle(pub SC_HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a handle from OpenSCManagerW or OpenServiceW, closed once.
        let _ = unsafe { CloseServiceHandle(self.0) };
    }
}

pub fn read(cache: &mut Cache) -> Services {
    // SAFETY: plain calls; the handle is closed by `Handle`.
    let Ok(h) = (unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE) }) else {
        return Services { list: Vec::new(), source: "scm", note: Some(Note::NoScm) };
    };
    let scm = Handle(h);
    if cache.at.is_none_or(|t| t.elapsed() > KEEP) {
        cache.configs.clear();
        cache.at = Some(Instant::now());
    }
    let mut list = Vec::new();
    let mut resume = 0u32;
    let mut buf = buffer(64 * 1024);
    loop {
        let (mut needed, mut returned) = (0u32, 0u32);
        // SAFETY: the buffer and counters outlive the call; `resume` carries on
        // from where a full buffer stopped.
        let r = unsafe {
            EnumServicesStatusExW(
                scm.0,
                SC_ENUM_PROCESS_INFO,
                SERVICE_WIN32 | SERVICE_DRIVER,
                SERVICE_STATE_ALL,
                Some(bytes(&mut buf)),
                &mut needed,
                &mut returned,
                Some(&mut resume),
                PCWSTR::null(),
            )
        };
        let more = matches!(&r, Err(e) if e.code() == ERROR_MORE_DATA.to_hresult());
        // SAFETY: the call wrote `returned` entries at the start of the buffer.
        let entries = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<ENUM_SERVICE_STATUS_PROCESSW>(), returned as usize) };
        for e in entries {
            let name = text(e.lpServiceName);
            let st = e.ServiceStatusProcess;
            let current = st.dwCurrentState.0;
            let cfg = cache.configs.entry(name.clone()).or_insert_with(|| config(&scm, &name)).clone();
            list.push(Service {
                display: text(e.lpDisplayName),
                state: fold::state(current, st.dwWin32ExitCode),
                pid: (st.dwProcessId != 0).then_some(st.dwProcessId),
                exit: fold::exit(current, st.dwWin32ExitCode, st.dwServiceSpecificExitCode),
                start: cfg.start,
                start_raw: cfg.start_raw,
                user: cfg.user,
                cmd: cfg.cmd,
                driver: fold::driver(st.dwServiceType.0),
                name,
                ..Service::default()
            });
        }
        if !more {
            break;
        }
        if returned == 0 {
            // Not one entry fitted: grow to what it asked for.
            buf = buffer(needed);
        }
    }
    Services { list, source: "scm", note: None }
}

/// Its start type, account and command line; empty when it cannot be opened.
fn config(scm: &Handle, name: &str) -> Config {
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    let Ok(h) = (unsafe { OpenServiceW(scm.0, PCWSTR(wide.as_ptr()), SERVICE_QUERY_CONFIG) }) else { return Config::default() };
    let svc = Handle(h);
    let mut needed = 0u32;
    // SAFETY: asks for the size only.
    let _ = unsafe { QueryServiceConfigW(svc.0, None, 0, &mut needed) };
    if needed == 0 {
        return Config::default();
    }
    let mut buf = buffer(needed);
    let p = buf.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>();
    // SAFETY: the buffer holds `needed` bytes, aligned for the struct.
    if unsafe { QueryServiceConfigW(svc.0, Some(p), needed, &mut needed) }.is_err() {
        return Config::default();
    }
    // SAFETY: filled by the call above.
    let q = unsafe { &*p };
    let mut delayed = SERVICE_DELAYED_AUTO_START_INFO::default();
    let mut n = 0u32;
    // SAFETY: the struct is the buffer, its size passed with it.
    let d = unsafe {
        let b = std::slice::from_raw_parts_mut((&raw mut delayed).cast::<u8>(), size_of::<SERVICE_DELAYED_AUTO_START_INFO>());
        QueryServiceConfig2W(svc.0, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, Some(b), &mut n)
    };
    let (start, start_raw) = fold::start(q.dwStartType.0, d.is_ok() && delayed.fDelayedAutostart.as_bool());
    Config { start, start_raw, user: some(text(q.lpServiceStartName)), cmd: some(text(q.lpBinaryPathName)) }
}
