//! macOS: the IOKit calls behind the devices. Every `IOUSBHostDevice` and
//! `IOPCIDevice` in the registry, with the nearest such device above each
//! as its parent and the class of the driver matched to it.

use std::ffi::CStr;

use objc2_core_foundation::{CFData, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_io_kit::{
    IOIteratorNext, IOObjectConformsTo, IOObjectCopyClass, IOObjectRelease, IORegistryEntryCreateCFProperty, IORegistryEntryGetChildEntry,
    IORegistryEntryGetParentEntry, IORegistryEntryGetRegistryEntryID, IOServiceGetMatchingServices, IOServiceMatching, io_iterator_t,
    io_object_t, kIOServicePlane,
};

use super::mac::{self, Raw};
use super::{Devices, Note};

/// `kIOMainPortDefault`.
const MAIN_PORT: u32 = 0;
const USB: &CStr = c"IOUSBHostDevice";
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

fn text(e: io_object_t, key: &str) -> String {
    prop(e, key).and_then(|v| v.downcast_ref::<CFString>().map(|s| s.to_string().trim().to_string())).unwrap_or_default()
}

fn number(e: io_object_t, key: &str) -> Option<u64> {
    prop(e, key)?.downcast_ref::<CFNumber>()?.as_i64().map(|n| n.max(0) as u64)
}

fn data(e: io_object_t, key: &str) -> Vec<u8> {
    prop(e, key).and_then(|v| v.downcast_ref::<CFData>().map(CFData::to_vec)).unwrap_or_default()
}

fn entry_id(e: io_object_t) -> u64 {
    let mut id = 0;
    // SAFETY: a live entry; the id is written on success.
    unsafe { IORegistryEntryGetRegistryEntryID(e, &mut id) };
    id
}

fn conforms(e: io_object_t, class: &CStr) -> bool {
    // SAFETY: a live object and a class name that is a C string.
    unsafe { IOObjectConformsTo(e, class.as_ptr().cast_mut().cast()) }
}

fn parent(e: io_object_t) -> Option<Obj> {
    let mut p = 0;
    // SAFETY: a live entry and a plane name that IOKit defines; the parent is
    // written on success and released by `Obj`.
    let ok = unsafe { IORegistryEntryGetParentEntry(e, kIOServicePlane.as_ptr().cast_mut().cast(), &mut p) } == 0;
    (ok && p != 0).then_some(Obj(p))
}

fn child(e: io_object_t) -> Option<Obj> {
    let mut c = 0;
    // SAFETY: as `parent`, down the plane.
    let ok = unsafe { IORegistryEntryGetChildEntry(e, kIOServicePlane.as_ptr().cast_mut().cast(), &mut c) } == 0;
    (ok && c != 0).then_some(Obj(c))
}

fn class(e: io_object_t) -> String {
    IOObjectCopyClass(e).map(|s| s.to_string()).unwrap_or_default()
}

/// The nearest USB or PCI device above `e`.
fn owner(e: io_object_t) -> Option<u64> {
    let mut at = parent(e)?;
    // The service plane is a few dozen deep at most; the bound is against a loop.
    for _ in 0..64 {
        if conforms(at.0, USB) || conforms(at.0, PCI) {
            return Some(entry_id(at.0));
        }
        at = parent(at.0)?;
    }
    None
}

/// The driver matched to a device: its first child's class, or for a USB
/// device whose child is an interface, the interface's driver.
fn driver(e: io_object_t) -> String {
    let Some(c) = child(e) else { return String::new() };
    let name = class(c.0);
    if name == "IOUSBHostInterface"
        && let Some(d) = child(c.0)
    {
        return class(d.0);
    }
    name
}

fn raw(e: io_object_t, usb: bool) -> Raw {
    let mut r = Raw { entry: entry_id(e), parent: owner(e), usb, driver: driver(e), ..Raw::default() };
    if usb {
        let n16 = |key: &str| number(e, key).map(|n| n as u16);
        r.name = text(e, "USB Product Name");
        r.vendor = text(e, "USB Vendor Name");
        r.vid = n16("idVendor");
        r.pid = n16("idProduct");
        r.rev = n16("bcdDevice");
        r.speed = number(e, "Device Speed");
        r.usb_class = number(e, "bDeviceClass").map(|n| n as u8);
        r.location = number(e, "locationID");
    } else {
        let n16 = |key: &str| mac::le(&data(e, key)).map(|n| n as u16);
        r.name = Some(mac::bytes_text(&data(e, "model"))).filter(|s| !s.is_empty()).unwrap_or_else(|| text(e, "IOName"));
        r.vid = n16("vendor-id");
        r.pid = n16("device-id");
        r.pci_class = mac::le(&data(e, "class-code"));
    }
    r
}

/// Every device of a class; `None` when IOKit cannot be asked.
fn each(class: &CStr, usb: bool, out: &mut Vec<Raw>) -> Option<()> {
    // SAFETY: a C string.
    let matching = unsafe { IOServiceMatching(class.as_ptr()) }?;
    let mut iter: io_iterator_t = 0;
    // SAFETY: the dictionary is consumed; the iterator is written on success.
    if unsafe { IOServiceGetMatchingServices(MAIN_PORT, Some(CFRetained::<CFDictionary>::from(&matching)), &mut iter) } != 0 {
        return None;
    }
    let iter = Obj(iter);
    loop {
        let d = IOIteratorNext(iter.0);
        if d == 0 {
            break;
        }
        let d = Obj(d);
        out.push(raw(d.0, usb));
    }
    Some(())
}

pub fn read() -> Devices {
    let mut raws = Vec::new();
    let usb = each(USB, true, &mut raws);
    let pci = each(PCI, false, &mut raws);
    let note = (usb.is_none() && pci.is_none()).then_some(Note::NoIoKit);
    Devices { list: raws.into_iter().map(mac::fold).collect(), note }
}
