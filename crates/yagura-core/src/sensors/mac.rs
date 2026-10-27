//! macOS: the System Management Controller's keys, read in `smc.rs`, made
//! into sensors here. Every SMC value is a four-letter key with a four-letter
//! type: temperatures start with `T` (`TC0P` is the CPU proximity sensor on
//! Intel Macs, `Tp09` a CPU sensor on Apple silicon), fans are `FNum` and
//! `F0Ac` / `F0Mn` / `F0Mx` (actual, least and most rpm). Intel Macs keep
//! numbers as big-endian fixed point (`sp78`, `fpe2`), Apple silicon as
//! little-endian floats (`flt `). The keys carry no names, so the known ones
//! are named from the lists the SMC tools share, and the Apple silicon
//! families are numbered in key order; a key without a name is not listed.
//! The HID sensors sysinfo reads on Apple silicon get friendlier names too.

/// The SMC's parameter block (`SMCKeyData_t`), 80 bytes in and out.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyData {
    pub key: u32,
    /// The SMC's version: major, minor, build, reserved, a 16-bit release.
    pub vers: [u8; 6],
    /// The power limits: version and length (16 bits each), CPU, GPU, memory.
    pub limits: [u32; 4],
    pub info: KeyInfo,
    pub result: u8,
    pub status: u8,
    /// The command.
    pub data8: u8,
    /// The index for `READ_INDEX`.
    pub data32: u32,
    pub bytes: [u8; 32],
}

/// A key's size and type (`SMCKeyData_keyInfo_t`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct KeyInfo {
    pub size: u32,
    pub ty: u32,
    pub attrs: u8,
}

/// The `AppleSMC` user client's method that takes a `KeyData`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const SELECTOR: u32 = 2;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const READ_BYTES: u8 = 5;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const READ_INDEX: u8 = 8;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const READ_KEYINFO: u8 = 9;

pub fn code(key: &str) -> u32 {
    let b = key.as_bytes();
    u32::from_be_bytes([
        b.first().copied().unwrap_or(b' '),
        b.get(1).copied().unwrap_or(b' '),
        b.get(2).copied().unwrap_or(b' '),
        b.get(3).copied().unwrap_or(b' '),
    ])
}

pub fn name(code: u32) -> String {
    String::from_utf8_lossy(&code.to_be_bytes()).into_owned()
}

/// A value of the types sensors use; `None` for any other type.
pub fn decode(ty: u32, b: &[u8]) -> Option<f32> {
    let be16 = || Some(u16::from_be_bytes([*b.first()?, *b.get(1)?]));
    let v = match &ty.to_be_bytes() {
        b"sp78" => f32::from(be16()? as i16) / 256.0,
        b"fpe2" => f32::from(be16()?) / 4.0,
        b"fp88" => f32::from(be16()?) / 256.0,
        b"flt " => f32::from_le_bytes(b.get(..4)?.try_into().ok()?),
        b"ui8 " => f32::from(*b.first()?),
        b"ui16" => f32::from(be16()?),
        b"ui32" => u32::from_be_bytes(b.get(..4)?.try_into().ok()?) as f32,
        _ => return None,
    };
    v.is_finite().then_some(v)
}

/// A temperature worth showing: unwired sensors read 0 or nonsense.
pub fn plausible(t: f32) -> bool {
    t > 0.0 && t < 130.0
}

/// Intel Macs' keys, from the lists the SMC tools share.
const NAMED: &[(&str, &str)] = &[
    ("TA0P", "Ambient"),
    ("TA1P", "Ambient 2"),
    ("TB0T", "Battery"),
    ("TB1T", "Battery 1"),
    ("TB2T", "Battery 2"),
    ("TC0D", "CPU die"),
    ("TC0H", "CPU heatsink"),
    ("TC0P", "CPU proximity"),
    ("TCXC", "CPU (PECI)"),
    ("TCGC", "Integrated GPU (PECI)"),
    ("TG0D", "GPU die"),
    ("TG0H", "GPU heatsink"),
    ("TG0P", "GPU proximity"),
    ("TH0P", "Drive bay"),
    ("TL0P", "Display proximity"),
    ("TM0P", "Memory proximity"),
    ("TN0D", "Northbridge die"),
    ("TN0P", "Northbridge proximity"),
    ("TPCD", "Platform controller hub"),
    ("TW0P", "Wireless proximity"),
    ("Th0H", "Heatsink"),
    ("Th1H", "Heatsink 2"),
    ("Ts0P", "Palm rest"),
];

/// Apple silicon's families: which sensor of a family is which core or
/// cluster differs from chip to chip, so they are numbered in key order.
const FAMILIES: &[(&str, &str)] = &[("Tp", "CPU"), ("Te", "CPU efficiency"), ("Tg", "GPU"), ("TH", "SSD")];

/// The temperature keys to list, each with its name, in key order. `keys`
/// are every key the SMC has.
pub fn temp_names(keys: &[String]) -> Vec<(String, String)> {
    let mut keys: Vec<&String> = keys.iter().filter(|k| k.len() == 4 && k.starts_with('T')).collect();
    keys.sort();
    keys.dedup();
    let mut counts = vec![0; FAMILIES.len()];
    let mut out = Vec::new();
    for k in keys {
        let b = k.as_bytes();
        let name = if let Some((_, n)) = NAMED.iter().find(|(key, _)| key == k) {
            n.to_string()
        } else if b[1] == b'C' && b[2].is_ascii_digit() && b[3] == b'C' {
            // Intel's per-core sensors, TC1C for the first core.
            let n = b[2] - b'0';
            if n == 0 {
                continue;
            }
            format!("CPU core {n}")
        } else if let Some(i) = FAMILIES.iter().position(|(p, _)| k.starts_with(p)) {
            counts[i] += 1;
            format!("{} {}", FAMILIES[i].1, counts[i])
        } else {
            continue;
        };
        out.push((k.clone(), name));
    }
    out
}

/// Whether a sysinfo component's id is an SMC key (Intel Macs), which the
/// SMC reader lists already; the HID sensors' ids are serial numbers.
pub fn is_smc_key(id: &str) -> bool {
    id.len() == 4 && id.starts_with('T') && id.bytes().all(|b| b.is_ascii_graphic())
}

/// The fan keys: actual, least and most rpm.
pub fn fan_keys(i: u32) -> [String; 3] {
    ["Ac", "Mn", "Mx"].map(|s| format!("F{i}{s}"))
}

/// A friendlier name for a HID sensor sysinfo lists on Apple silicon;
/// `None` for the calibration value that is not a temperature.
pub fn hid_name(label: &str) -> Option<String> {
    if label == "PMU tcal" {
        return None;
    }
    let numbered = |prefix: &str, name: &str| label.strip_prefix(prefix).map(|n| format!("{name} {n}").trim_end().to_string());
    Some(
        numbered("PMU tdie", "CPU die")
            .or_else(|| numbered("PMU tdev", "SoC"))
            .or_else(|| numbered("PMU2 tdie", "CPU die (2)"))
            .or_else(|| numbered("PMU2 tdev", "SoC (2)"))
            .unwrap_or_else(|| match label {
                "NAND CH0 temp" => "SSD".into(),
                "gas gauge battery" => "Battery".into(),
                _ => label.to_string(),
            }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn the_parameter_block_is_laid_out_as_the_smc_wants() {
        assert_eq!(size_of::<KeyData>(), 80);
        assert_eq!(size_of::<KeyInfo>(), 12);
        let offsets = [
            offset_of!(KeyData, vers),
            offset_of!(KeyData, limits),
            offset_of!(KeyData, info),
            offset_of!(KeyData, result),
            offset_of!(KeyData, data8),
            offset_of!(KeyData, data32),
            offset_of!(KeyData, bytes),
        ];
        assert_eq!(offsets, [4, 12, 28, 40, 42, 44, 48]);
    }

    #[test]
    fn keys_and_values_are_decoded_by_their_type() {
        assert_eq!(code("TC0P"), 0x5443_3050);
        assert_eq!(name(code("F0Ac")), "F0Ac");
        assert_eq!(code("ui8"), code("ui8 "));
        // 45.5 °C in sp78; 1,200 rpm in fpe2; a float on Apple silicon.
        assert_eq!(decode(code("sp78"), &[0x2D, 0x80]), Some(45.5));
        assert_eq!(decode(code("sp78"), &[0xFF, 0x00]), Some(-1.0));
        assert_eq!(decode(code("fpe2"), &[0x12, 0xC0]), Some(1200.0));
        assert_eq!(decode(code("flt "), &1850.5f32.to_le_bytes()), Some(1850.5));
        assert_eq!(decode(code("flt "), &f32::NAN.to_le_bytes()), None);
        assert_eq!(decode(code("ui8 "), &[2]), Some(2.0));
        assert_eq!(decode(code("ui32"), &[0, 0, 0x05, 0xDC]), Some(1500.0));
        assert_eq!(decode(code("ch8*"), &[1, 2]), None);
        assert_eq!(decode(code("sp78"), &[1]), None);
        assert!(plausible(38.0) && !plausible(0.0) && !plausible(-1.0) && !plausible(255.0));
    }

    #[test]
    fn known_keys_get_names_and_families_are_numbered() {
        let keys: Vec<String> =
            ["TC0P", "TC1C", "TC0C", "TC2C", "Tp09", "Tp01", "Tg05", "Te05", "TXYZ", "F0Ac", "TB0T", "Tp01", "Tx"].map(String::from).into();
        let names = temp_names(&keys);
        let pairs: Vec<(&str, &str)> = names.iter().map(|(k, n)| (k.as_str(), n.as_str())).collect();
        assert_eq!(
            pairs,
            [
                ("TB0T", "Battery"),
                ("TC0P", "CPU proximity"),
                ("TC1C", "CPU core 1"),
                ("TC2C", "CPU core 2"),
                ("Te05", "CPU efficiency 1"),
                ("Tg05", "GPU 1"),
                ("Tp01", "CPU 1"),
                ("Tp09", "CPU 2"),
            ]
        );
        assert_eq!(fan_keys(1), ["F1Ac", "F1Mn", "F1Mx"]);
        assert!(is_smc_key("TCXc") && !is_smc_key("TC0") && !is_smc_key("0x0001"));
    }

    #[test]
    fn hid_sensors_get_friendlier_names() {
        assert_eq!(hid_name("PMU tdie3").as_deref(), Some("CPU die 3"));
        assert_eq!(hid_name("PMU tdev1").as_deref(), Some("SoC 1"));
        assert_eq!(hid_name("PMU2 tdie2").as_deref(), Some("CPU die (2) 2"));
        assert_eq!(hid_name("NAND CH0 temp").as_deref(), Some("SSD"));
        assert_eq!(hid_name("gas gauge battery").as_deref(), Some("Battery"));
        assert_eq!(hid_name("PMU TP3w").as_deref(), Some("PMU TP3w"));
        assert_eq!(hid_name("PMU tcal"), None);
    }
}
