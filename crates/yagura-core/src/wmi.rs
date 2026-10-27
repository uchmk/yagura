//! WMI on the collect thread: connecting to a namespace, the rows of a
//! query and their properties. LibreHardwareMonitor's sensors and the
//! adapters `Get-NetAdapter` lists both come this way.

use ::windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoSetProxyBlanket, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL,
    RPC_C_IMP_LEVEL_IMPERSONATE,
};
use ::windows::Win32::System::Variant::{VARIANT, VT_BSTR, VT_R4, VT_R8, VariantClear};
use ::windows::Win32::System::Wmi::{
    IEnumWbemClassObject, IWbemClassObject, IWbemLocator, IWbemServices, WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY,
    WBEM_INFINITE, WbemLocator,
};
use ::windows::core::{BSTR, HSTRING, PCWSTR};

/// `RPC_C_AUTHN_WINNT` and `RPC_C_AUTHZ_NONE`, as WMI's samples set them.
const AUTHN_WINNT: u32 = 10;
const AUTHZ_NONE: u32 = 0;

/// Starts COM on this thread. Calling it again does nothing, and a thread
/// already in another apartment keeps it, which works too.
pub fn init() {
    // SAFETY: no arguments to keep alive; the result is only a status.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

/// The namespace `ns` (`root\StandardCimv2`), `None` when it is not there.
pub fn connect(ns: &str) -> Option<IWbemServices> {
    init();
    // SAFETY: COM calls with owned arguments; the interfaces release
    // themselves when dropped.
    unsafe {
        let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER).ok()?;
        let none = BSTR::new();
        let svc = locator.ConnectServer(&BSTR::from(ns), &none, &none, &none, 0, &none, None).ok()?;
        CoSetProxyBlanket(
            &svc,
            AUTHN_WINNT,
            AUTHZ_NONE,
            PCWSTR::null(),
            RPC_C_AUTHN_LEVEL_CALL,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        )
        .ok()?;
        Some(svc)
    }
}

/// Every row of `query`, through `each`; `None` when the query fails.
pub fn rows<T>(svc: &IWbemServices, query: &str, each: impl Fn(&IWbemClassObject) -> Option<T>) -> Option<Vec<T>> {
    // SAFETY: as in `connect`.
    unsafe {
        let flags = WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY;
        let list: IEnumWbemClassObject = svc.ExecQuery(&BSTR::from("WQL"), &BSTR::from(query), flags, None).ok()?;
        let mut out = Vec::new();
        loop {
            let mut row = [None];
            let mut got = 0;
            if list.Next(WBEM_INFINITE, &mut row, &mut got).is_err() || got == 0 {
                break;
            }
            if let Some(v) = row[0].as_ref().and_then(&each) {
                out.push(v);
            }
        }
        Some(out)
    }
}

/// A property as it comes, cleared again after `f` looks at it.
fn property<T>(o: &IWbemClassObject, name: &str, f: impl Fn(&VARIANT) -> Option<T>) -> Option<T> {
    let mut v = VARIANT::default();
    // SAFETY: `v` is a valid VARIANT that `Get` fills and VariantClear frees.
    unsafe {
        o.Get(&HSTRING::from(name), 0, &mut v, None, None).ok()?;
        let out = f(&v);
        let _ = VariantClear(&mut v);
        out
    }
}

pub fn text(o: &IWbemClassObject, name: &str) -> Option<String> {
    property(o, name, |v| {
        // SAFETY: the union is read as the type its tag says.
        unsafe { (v.Anonymous.Anonymous.vt == VT_BSTR).then(|| v.Anonymous.Anonymous.Anonymous.bstrVal.to_string()) }
    })
}

pub fn number(o: &IWbemClassObject, name: &str) -> Option<f32> {
    property(o, name, |v| {
        // SAFETY: as in `text`.
        unsafe {
            let inner = &v.Anonymous.Anonymous;
            match inner.vt {
                VT_R4 => Some(inner.Anonymous.fltVal),
                VT_R8 => Some(inner.Anonymous.dblVal as f32),
                _ => None,
            }
        }
    })
}
