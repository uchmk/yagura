//! Windows: the raw SMBIOS table from `GetSystemFirmwareTable('RSMB')`, which
//! any user may read, and the boot mode from `GetFirmwareType`.

use windows::Win32::System::SystemInformation::{
    FIRMWARE_TYPE, FirmwareTypeBios, FirmwareTypeUefi, GetFirmwareType, GetSystemFirmwareTable, RSMB,
};

use super::{Hardware, Note, smbios};

pub fn read() -> Hardware {
    let mut hw = match table() {
        Some(buf) => match smbios::rsmb(&buf) {
            Some(((major, minor), table)) => {
                let mut hw = smbios::parse(table);
                hw.source = format!("SMBIOS {major}.{minor} (GetSystemFirmwareTable)");
                hw
            }
            None => Hardware { note: Some(Note::Failed), ..Hardware::default() },
        },
        None => Hardware { note: Some(Note::Failed), ..Hardware::default() },
    };
    let mut kind = FIRMWARE_TYPE(0);
    // SAFETY: the pointer is to a local that outlives the call.
    if unsafe { GetFirmwareType(&mut kind) }.is_ok() {
        if kind == FirmwareTypeUefi {
            hw.bios.uefi = Some(true);
        } else if kind == FirmwareTypeBios {
            hw.bios.uefi = Some(false);
        }
    }
    hw
}

/// `RawSMBIOSData`: asked for its size first, then read.
fn table() -> Option<Vec<u8>> {
    // SAFETY: no buffer; the call returns the size it needs.
    let need = unsafe { GetSystemFirmwareTable(RSMB, 0, None) };
    if need == 0 {
        return None;
    }
    let mut buf = vec![0u8; need as usize];
    // SAFETY: the buffer is as long as the call was told.
    let got = unsafe { GetSystemFirmwareTable(RSMB, 0, Some(&mut buf)) };
    (got > 0 && got <= need).then(|| {
        buf.truncate(got as usize);
        buf
    })
}
