//! macOS: the `AppleSmartBattery` entry of the IORegistry.

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_io_kit::{IOObjectRelease, IORegistryEntryCreateCFProperty, IOServiceGetMatchingService, IOServiceMatching, io_object_t};

use super::Battery;

/// `kIOMainPortDefault`.
const MAIN_PORT: u32 = 0;

fn prop(e: io_object_t, key: &str) -> Option<CFRetained<CFType>> {
    let key = CFString::from_str(key);
    // SAFETY: a live entry and a string key; the default allocator; no options.
    unsafe { IORegistryEntryCreateCFProperty(e, Some(&key), None, 0) }
}

fn number(v: Option<CFRetained<CFType>>) -> Option<i64> {
    v?.downcast_ref::<CFNumber>()?.as_i64()
}

fn flag(v: Option<CFRetained<CFType>>) -> bool {
    v.and_then(|v| v.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool)).unwrap_or(false)
}

pub fn read() -> Option<Battery> {
    // SAFETY: a C string.
    let matching = unsafe { IOServiceMatching(c"AppleSmartBattery".as_ptr()) }?;
    // SAFETY: the dictionary is consumed; the service is released below.
    let e = unsafe { IOServiceGetMatchingService(MAIN_PORT, Some(CFRetained::<CFDictionary>::from(&matching))) };
    if e == 0 {
        return None;
    }
    let out = super::from_capacity(
        number(prop(e, "CurrentCapacity")),
        number(prop(e, "MaxCapacity")),
        flag(prop(e, "IsCharging")),
        flag(prop(e, "ExternalConnected")),
    );
    IOObjectRelease(e);
    out
}
