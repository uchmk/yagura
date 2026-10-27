//! SetupAPI and the configuration manager: every present device in every
//! class, as Device Manager lists them (devices it hides are left out too).
//! No administrator is needed. The class names are the OS's own, in its
//! language, and kept per class.

use std::collections::HashMap;

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_DEVNODE_STATUS_FLAGS, CM_Get_DevNode_Status, CM_PROB, CR_SUCCESS, DIGCF_ALLCLASSES, DIGCF_PRESENT, DN_NO_SHOW_IN_DM, HDEVINFO,
    SETUP_DI_REGISTRY_PROPERTY, SP_DEVINFO_DATA, SPDRP_DEVICEDESC, SPDRP_FRIENDLYNAME, SPDRP_HARDWAREID, SPDRP_LOCATION_INFORMATION,
    SPDRP_MFG, SPDRP_SERVICE, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDescriptionW, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiGetDevicePropertyW, SetupDiGetDeviceRegistryPropertyW,
};
use windows::Win32::Devices::Properties::{DEVPKEY_Device_DriverVersion, DEVPKEY_Device_Parent, DEVPROPTYPE};
use windows::Win32::Foundation::DEVPROPKEY;
use windows::core::{GUID, PCWSTR};

use super::{Device, Devices, Note, hardware_id, windows as fold};

/// The class names by class GUID.
#[derive(Default)]
pub struct Classes(HashMap<u128, String>);

impl Classes {
    fn name(&mut self, guid: &GUID) -> String {
        self.0
            .entry(guid.to_u128())
            .or_insert_with(|| {
                let mut buf = [0u16; 256];
                // SAFETY: the buffer's length goes with it.
                match unsafe { SetupDiGetClassDescriptionW(guid, &mut buf, None) } {
                    Ok(()) => fold::first_string(&buf),
                    Err(_) => String::new(),
                }
            })
            .clone()
    }
}

/// Destroy the device list when dropped.
struct List(HDEVINFO);

impl Drop for List {
    fn drop(&mut self) {
        // SAFETY: a list from SetupDiGetClassDevsW, destroyed once.
        let _ = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

/// UTF-16 code units, `u16`-aligned, as the bytes the calls write.
fn bytes(b: &mut [u16]) -> &mut [u8] {
    // SAFETY: the same memory as bytes; u8 has no alignment or invalid values.
    unsafe { std::slice::from_raw_parts_mut(b.as_mut_ptr().cast::<u8>(), b.len() * 2) }
}

fn registry(list: &List, data: &SP_DEVINFO_DATA, prop: SETUP_DI_REGISTRY_PROPERTY) -> String {
    let mut buf = vec![0u16; 512];
    let mut need = 0u32;
    // SAFETY: the buffer's length goes with it; a longer value says its size.
    if unsafe { SetupDiGetDeviceRegistryPropertyW(list.0, data, prop, None, Some(bytes(&mut buf)), Some(&mut need)) }.is_err() {
        if (need as usize) <= buf.len() * 2 {
            return String::new();
        }
        buf = vec![0u16; (need as usize).div_ceil(2)];
        // SAFETY: as above, with the size it asked for.
        if unsafe { SetupDiGetDeviceRegistryPropertyW(list.0, data, prop, None, Some(bytes(&mut buf)), None) }.is_err() {
            return String::new();
        }
    }
    fold::first_string(&buf)
}

fn property(list: &List, data: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> String {
    let mut buf = [0u16; 512];
    let mut kind = DEVPROPTYPE::default();
    // SAFETY: the buffer's length goes with it; the strings asked for are short.
    match unsafe { SetupDiGetDevicePropertyW(list.0, data, key, &mut kind, Some(bytes(&mut buf)), None, 0) } {
        Ok(()) => fold::first_string(&buf),
        Err(_) => String::new(),
    }
}

pub fn read(classes: &mut Classes) -> Devices {
    // SAFETY: plain call; the list is destroyed by `List`.
    let Ok(h) = (unsafe { SetupDiGetClassDevsW(None, PCWSTR::null(), None, DIGCF_PRESENT | DIGCF_ALLCLASSES) }) else {
        return Devices { list: Vec::new(), note: Some(Note::NoSetupApi) };
    };
    let list = List(h);
    let mut out = Vec::new();
    for i in 0.. {
        let mut data = SP_DEVINFO_DATA { cbSize: size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
        // SAFETY: `data` is sized; an error ends the list.
        if unsafe { SetupDiEnumDeviceInfo(list.0, i, &mut data) }.is_err() {
            break;
        }
        let (mut flags, mut problem) = (CM_DEVNODE_STATUS_FLAGS(0), CM_PROB(0));
        // SAFETY: plain call on the node the list gave.
        let known = unsafe { CM_Get_DevNode_Status(&mut flags, &mut problem, data.DevInst, 0) } == CR_SUCCESS;
        if known && flags.0 & DN_NO_SHOW_IN_DM.0 != 0 {
            continue;
        }
        let mut id = [0u16; 512];
        // SAFETY: the buffer's length goes with it.
        if unsafe { SetupDiGetDeviceInstanceIdW(list.0, &data, Some(&mut id), None) }.is_err() {
            continue;
        }
        let id = fold::first_string(&id);
        let name = Some(registry(&list, &data, SPDRP_FRIENDLYNAME))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| registry(&list, &data, SPDRP_DEVICEDESC));
        let parent = property(&list, &data, &DEVPKEY_Device_Parent);
        out.push(Device {
            // The parent's ID comes in lower case ("7&397afaa3") where the device's own is upper: match them as `id` has it.
            parent: (!parent.is_empty()).then(|| parent.to_ascii_uppercase()),
            name: if name.is_empty() { id.clone() } else { name },
            vendor: registry(&list, &data, SPDRP_MFG),
            kind: fold::kind(data.ClassGuid.to_u128()),
            class_name: classes.name(&data.ClassGuid),
            driver: registry(&list, &data, SPDRP_SERVICE),
            driver_version: property(&list, &data, &DEVPKEY_Device_DriverVersion),
            status: if known { fold::status(flags.0, problem.0) } else { super::Status::Ok },
            bus: hardware_id(&registry(&list, &data, SPDRP_HARDWAREID)),
            location: registry(&list, &data, SPDRP_LOCATION_INFORMATION),
            id,
        });
    }
    let note = out.is_empty().then_some(Note::NoSetupApi);
    Devices { list: out, note }
}
