//! Windows: what the SetupAPI reads (`setupapi.rs`) mean, kept pure so the
//! tests run everywhere.

use super::{Kind, Status};

/// The device setup classes' GUIDs that most of Device Manager's 4d36e9xx
/// classes share past their first group.
const SHARED: u128 = 0x0000_0000_e325_11ce_bfc1_0800_2be1_0318;

const fn class(first: u32) -> u128 {
    ((first as u128) << 96) | SHARED
}

/// A device setup class GUID (as `u128`, most significant first) as a kind.
pub fn kind(guid: u128) -> Kind {
    match guid {
        g if g == class(0x4d36e968) => Kind::Display,
        g if g == class(0x4d36e972) => Kind::Net,
        g if g == class(0x4d36e96c) => Kind::Audio,
        0xc166523c_fe0c_4a94_a586_f1a80cfbbf3e => Kind::Audio,
        g if g == class(0x4d36e967) || g == class(0x4d36e96a) || g == class(0x4d36e97b) || g == class(0x4d36e965) => Kind::Storage,
        0x71a27cdd_812a_11d0_bec7_08002be2092f => Kind::Storage,
        g if g == class(0x4d36e96b) || g == class(0x4d36e96f) => Kind::Input,
        0x745a17a0_74d3_11d0_b6fe_00a0c90f57da => Kind::Input,
        0xca3e7ab9_b4c3_4ae6_8251_579ef933890f | 0x6bdd1fc6_810f_11d0_bec7_08002be2092f => Kind::Camera,
        0xe0cbf06c_cd8b_4647_bb8a_263b43f0f974 => Kind::Bluetooth,
        g if g == class(0x4d36e96e) => Kind::Monitor,
        g if g == class(0x4d36e979) => Kind::Printer,
        0x1ed2bbf9_11f0_4084_b21f_ad83a8e6dcdc => Kind::Printer,
        0x50127dc3_0f36_415e_a6cc_4cb3be910b65 => Kind::Processor,
        0x72631e54_78a4_11d0_bcf7_00aa00b7b32a => Kind::Battery,
        g if g == class(0x4d36e97d) => Kind::System,
        0x36fc9e60_c465_11cf_8056_444553540000 | 0x88bae032_5a81_49f0_bc3d_a4ff138216d6 => Kind::Usb,
        _ => Kind::Other,
    }
}

/// The configuration manager's `DN_HAS_PROBLEM`.
pub const DN_HAS_PROBLEM: u32 = 0x0000_0400;
/// `CM_PROB_DISABLED`: turned off in Device Manager.
const CM_PROB_DISABLED: u32 = 22;

/// A device node's status flags and problem code as its status; a device
/// with no driver installed is a problem (28), as Device Manager shows it.
pub fn status(flags: u32, problem: u32) -> Status {
    if flags & DN_HAS_PROBLEM == 0 {
        Status::Ok
    } else if problem == CM_PROB_DISABLED {
        Status::Disabled
    } else {
        Status::Problem(problem)
    }
}

/// The text of a `REG_MULTI_SZ` / `DEVPROP_TYPE_STRING` buffer of UTF-16
/// code units: its first string.
pub fn first_string(units: &[u16]) -> String {
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end]).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_guids_give_kinds() {
        assert_eq!(kind(0x4d36e968_e325_11ce_bfc1_08002be10318), Kind::Display);
        assert_eq!(kind(0x4d36e972_e325_11ce_bfc1_08002be10318), Kind::Net);
        assert_eq!(kind(0x4d36e967_e325_11ce_bfc1_08002be10318), Kind::Storage);
        assert_eq!(kind(0x4d36e97d_e325_11ce_bfc1_08002be10318), Kind::System);
        assert_eq!(kind(0x745a17a0_74d3_11d0_b6fe_00a0c90f57da), Kind::Input);
        assert_eq!(kind(0x36fc9e60_c465_11cf_8056_444553540000), Kind::Usb);
        assert_eq!(kind(0xe0cbf06c_cd8b_4647_bb8a_263b43f0f974), Kind::Bluetooth);
        assert_eq!(kind(0x4d36e968_e325_11ce_bfc1_08002be10319), Kind::Other);
        assert_eq!(kind(0), Kind::Other);
    }

    #[test]
    fn node_status_folds_to_a_status() {
        assert_eq!(status(0x0180_200a, 0), Status::Ok);
        assert_eq!(status(DN_HAS_PROBLEM, 22), Status::Disabled);
        assert_eq!(status(DN_HAS_PROBLEM | 0x8, 28), Status::Problem(28));
        // A problem code without the flag is stale.
        assert_eq!(status(0, 43), Status::Ok);
    }

    #[test]
    fn utf16_buffers_read_as_strings() {
        let w = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        let mut multi = w("USB\\VID_046D&PID_C52B&REV_1211");
        multi.push(0);
        multi.extend(w("USB\\VID_046D&PID_C52B"));
        multi.extend([0, 0]);
        assert_eq!(first_string(&multi), "USB\\VID_046D&PID_C52B&REV_1211");
        assert_eq!(first_string(&w(" Logitech ")), "Logitech");
        assert_eq!(first_string(&[]), "");
    }
}
