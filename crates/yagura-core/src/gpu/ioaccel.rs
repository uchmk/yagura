//! macOS: the IOKit calls behind the GPUs. Every `IOAccelerator` service is
//! read once per tick; its PCI device, when it has one, gives the name, the
//! ids and the VRAM's size.

use std::collections::HashMap;
use std::ffi::CStr;

use objc2_core_foundation::{CFData, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_io_kit::{
    IOIteratorNext, IOObjectConformsTo, IOObjectRelease, IORegistryEntryCreateCFProperty, IORegistryEntryGetParentEntry,
    IORegistryEntryGetRegistryEntryID, IOServiceGetMatchingServices, IOServiceMatching, io_iterator_t, io_object_t, kIOServicePlane,
};

use super::mac::{self, Raw, STATS};

/// `kIOMainPortDefault`.
const MAIN_PORT: u32 = 0;
const ACCELERATOR: &CStr = c"IOAccelerator";
const PCI: &CStr = c"IOPCIDevice";

/// An IOKit object, released when dropped.
struct Obj(io_object_t);

impl Drop for Obj {
    fn drop(&mut self) {
        IOObjectRelease(self.0);
    }
}

fn prop(e: io_object_t, key: &str) -> Option<CFRetained<CFType>> {
    let key = CFString::from_str(key);
    // SAFETY: a live entry and a string key; the default allocator; no options.
    unsafe { IORegistryEntryCreateCFProperty(e, Some(&key), None, 0) }
}

fn parent(e: io_object_t) -> Option<Obj> {
    let mut p = 0;
    // SAFETY: a live entry and a plane name that IOKit defines; the parent is
    // written on success and released by `Obj`.
    let ok = unsafe { IORegistryEntryGetParentEntry(e, kIOServicePlane.as_ptr().cast_mut().cast(), &mut p) } == 0;
    (ok && p != 0).then_some(Obj(p))
}

/// The PCI device the accelerator hangs from, a few steps up at most.
fn pci_device(e: io_object_t) -> Option<Obj> {
    let mut at = parent(e)?;
    for _ in 0..4 {
        // SAFETY: a live object and a class name that is a C string.
        if unsafe { IOObjectConformsTo(at.0, PCI.as_ptr().cast_mut().cast()) } {
            return Some(at);
        }
        at = parent(at.0)?;
    }
    None
}

fn text(v: Option<CFRetained<CFType>>) -> String {
    let Some(v) = v else { return String::new() };
    if let Some(s) = v.downcast_ref::<CFString>() {
        return s.to_string().trim().to_string();
    }
    v.downcast_ref::<CFData>().map(|d| mac::c_text(&d.to_vec())).unwrap_or_default()
}

/// A number, or a little-endian word kept as data (`vendor-id`).
fn number(v: Option<CFRetained<CFType>>) -> Option<i64> {
    let v = v?;
    if let Some(n) = v.downcast_ref::<CFNumber>() {
        return n.as_i64();
    }
    let b = v.downcast_ref::<CFData>()?.to_vec();
    Some(i64::from(u32::from_le_bytes(b.get(..4)?.try_into().ok()?)))
}

fn stats(e: io_object_t) -> HashMap<String, i64> {
    let Some(table) = prop(e, "PerformanceStatistics") else { return HashMap::new() };
    let Some(dict) = table.downcast_ref::<CFDictionary>() else { return HashMap::new() };
    // SAFETY: IOKit's property tables are keyed by strings; the values are
    // only looked at through `downcast_ref`.
    let dict: &CFDictionary<CFString, CFType> = unsafe { dict.cast_unchecked() };
    STATS.iter().filter_map(|&k| Some((k.to_string(), number(dict.get(&CFString::from_str(k)))?))).collect()
}

fn raw(acc: &Obj) -> Raw {
    let mut entry = 0;
    // SAFETY: a live entry; the id is written on success.
    if unsafe { IORegistryEntryGetRegistryEntryID(acc.0, &mut entry) } != 0 {
        entry = u64::from(acc.0);
    }
    let mut r =
        Raw { entry, class: text(prop(acc.0, "IOClass")), model: text(prop(acc.0, "model")), stats: stats(acc.0), ..Raw::default() };
    if let Some(dev) = pci_device(acc.0) {
        let id = |k: &str| number(prop(dev.0, k)).map(|n| (n & 0xffff) as u32);
        r.vendor = id("vendor-id");
        r.device = id("device-id");
        r.vram_mb = number(prop(dev.0, "VRAM,totalMB")).filter(|&mb| mb > 0).map(|mb| mb as u64);
        if r.model.is_empty() {
            r.model = text(prop(dev.0, "model"));
        }
    }
    r
}

/// Every accelerator; none when IOKit cannot be asked.
pub fn read() -> Vec<Raw> {
    // SAFETY: a C string.
    let Some(matching) = (unsafe { IOServiceMatching(ACCELERATOR.as_ptr()) }) else { return Vec::new() };
    let mut iter: io_iterator_t = 0;
    // SAFETY: the dictionary is consumed; the iterator is written on success.
    if unsafe { IOServiceGetMatchingServices(MAIN_PORT, Some(CFRetained::<CFDictionary>::from(&matching)), &mut iter) } != 0 {
        return Vec::new();
    }
    let iter = Obj(iter);
    let mut out = Vec::new();
    loop {
        let a = IOIteratorNext(iter.0);
        if a == 0 {
            break;
        }
        out.push(raw(&Obj(a)));
    }
    out
}
