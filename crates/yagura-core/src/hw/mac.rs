//! macOS: `system_profiler -xml SPHardwareDataType SPMemoryDataType`. An Intel
//! Mac lists its memory slots; Apple silicon has one block of unified memory
//! in the chip's package. The serial numbers it also gives are left alone.

use plist::{Dictionary, Value};

use super::{Bios, Cpu, Ecc, Hardware, Note, Part, Slot, clean, maker};

#[cfg(target_os = "macos")]
pub fn read() -> Hardware {
    use std::process::{Command, Stdio};
    let out = Command::new("system_profiler")
        .args(["-xml", "SPHardwareDataType", "SPMemoryDataType"])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => parse(&o.stdout),
        _ => Hardware { note: Some(Note::Failed), ..Hardware::default() },
    }
}

fn text(d: &Dictionary, key: &str) -> String {
    d.get(key).and_then(Value::as_string).map(clean).unwrap_or_default()
}

/// `8 GB`, `512 MB` in bytes; `empty` and the like are `None`.
fn bytes(s: &str) -> Option<u64> {
    let (n, unit) = s.trim().split_once(' ')?;
    let n: u64 = n.parse().ok()?;
    let shift = match unit {
        "KB" => 10,
        "MB" => 20,
        "GB" => 30,
        "TB" => 40,
        _ => return None,
    };
    Some(n << shift)
}

/// A part number that the firmware wrote as hex (`0x4D3437...`), read back
/// as the text it spells; anything else as it is.
fn unhex(s: &str) -> String {
    let Some(hex) = s.strip_prefix("0x").filter(|h| h.len() >= 4 && h.len() % 2 == 0) else { return s.to_string() };
    let raw: Option<Vec<u8>> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect();
    match raw {
        Some(raw) if raw.iter().all(|&b| b == 0 || b.is_ascii_graphic() || b == b' ') => clean(&String::from_utf8_lossy(&raw)),
        _ => s.to_string(),
    }
}

/// The processors: Apple silicon names its chip and counts its cores as
/// `proc 12:8:4` (all, performance, efficiency); an Intel Mac names its CPU,
/// counts every package's cores as a number and says each one's L3.
fn cpus(d: &Dictionary) -> Vec<Cpu> {
    let Some(name) = [text(d, "chip_type"), text(d, "cpu_type")].into_iter().find(|s| !s.is_empty()) else { return Vec::new() };
    let cores = match d.get("number_processors") {
        Some(Value::Integer(n)) => n.as_unsigned().unwrap_or(0) as u32,
        Some(Value::String(s)) => s.trim_start_matches("proc ").split(':').next().and_then(|n| n.trim().parse().ok()).unwrap_or(0),
        _ => 0,
    };
    let packages = d.get("packages").and_then(Value::as_unsigned_integer).unwrap_or(1).clamp(1, 8) as u32;
    let vendor = if name.starts_with("Apple") {
        "Apple"
    } else if name.contains("Intel") {
        "Intel"
    } else {
        ""
    };
    let cpu = Cpu {
        vendor: vendor.into(),
        name,
        cores: cores / packages,
        cache: [0, 0, bytes(&text(d, "l3_cache")).unwrap_or(0)],
        ..Cpu::default()
    };
    vec![cpu; packages as usize]
}

/// One `_items` entry of the memory data type that is a slot.
fn slot(d: &Dictionary) -> Slot {
    // `empty` reads as nothing (see `clean`).
    let size = text(d, "dimm_size");
    let present = !size.is_empty();
    let s = |key: &str| if present { text(d, key) } else { String::new() };
    Slot {
        locator: text(d, "_name"),
        present,
        size: bytes(&size),
        kind: s("dimm_type"),
        speed: s("dimm_speed").split(' ').next().and_then(|n| n.parse().ok()).unwrap_or(0),
        vendor: maker(&s("dimm_manufacturer")),
        part: unhex(&s("dimm_part_number")),
        ..Slot::default()
    }
}

/// What `system_profiler -xml` says: a list of data types, each with its
/// `_items`.
pub fn parse(xml: &[u8]) -> Hardware {
    let Ok(Value::Array(types)) = Value::from_reader(std::io::Cursor::new(xml)) else {
        return Hardware { note: Some(Note::Failed), ..Hardware::default() };
    };
    let mut hw = Hardware { source: "system_profiler".into(), ..Hardware::default() };
    for t in types.iter().filter_map(Value::as_dictionary) {
        let items: Vec<&Dictionary> =
            t.get("_items").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_dictionary).collect();
        match t.get("_dataType").and_then(Value::as_string) {
            Some("SPHardwareDataType") => {
                let Some(d) = items.first() else { continue };
                hw.system = Part {
                    vendor: "Apple".into(),
                    name: text(d, "machine_name"),
                    version: text(d, "machine_model"),
                    sku: text(d, "model_number"),
                    ..Part::default()
                };
                hw.bios = Bios { vendor: "Apple".into(), version: text(d, "boot_rom_version"), ..Bios::default() };
                hw.cpus = cpus(d);
            }
            Some("SPMemoryDataType") => {
                for d in items {
                    let slots: Vec<&Dictionary> =
                        d.get("_items").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_dictionary).collect();
                    if slots.is_empty() {
                        // Unified memory: its size under the data type's own name.
                        hw.unified = true;
                        hw.slots.push(Slot {
                            present: true,
                            size: bytes(&text(d, "SPMemoryDataType")),
                            kind: text(d, "dimm_type"),
                            vendor: maker(&text(d, "dimm_manufacturer")),
                            ..Slot::default()
                        });
                    } else {
                        hw.slots.extend(slots.into_iter().map(slot));
                        if text(d, "global_ecc_state") == "ecc_disabled" {
                            hw.ecc = Some(Ecc::None);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    hw
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plist(types: &str) -> Vec<u8> {
        format!(r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><array>{types}</array></plist>"#).into_bytes()
    }

    #[test]
    fn apple_silicon_has_one_block_of_unified_memory() {
        let xml = plist(
            r#"<dict><key>_dataType</key><string>SPHardwareDataType</string><key>_items</key><array><dict>
                <key>machine_name</key><string>MacBook Pro</string>
                <key>machine_model</key><string>Mac14,10</string>
                <key>model_number</key><string>MPHE3J/A</string>
                <key>chip_type</key><string>Apple M2 Pro</string>
                <key>boot_rom_version</key><string>11881.1.1</string>
                <key>number_processors</key><string>proc 12:8:4</string>
                <key>serial_number</key><string>SECRET</string>
              </dict></array></dict>
              <dict><key>_dataType</key><string>SPMemoryDataType</string><key>_items</key><array><dict>
                <key>SPMemoryDataType</key><string>16 GB</string>
                <key>dimm_manufacturer</key><string>Hynix</string>
                <key>dimm_type</key><string>LPDDR5</string>
              </dict></array></dict>"#,
        );
        let hw = parse(&xml);
        assert_eq!((hw.system.name.as_str(), hw.system.version.as_str(), hw.system.sku.as_str()), ("MacBook Pro", "Mac14,10", "MPHE3J/A"));
        assert_eq!((hw.bios.vendor.as_str(), hw.bios.version.as_str(), hw.bios.uefi), ("Apple", "11881.1.1", None));
        assert!(hw.unified);
        assert_eq!(hw.slots.len(), 1);
        let s = &hw.slots[0];
        assert_eq!((s.present, s.size, s.kind.as_str(), s.vendor.as_str()), (true, Some(16 << 30), "LPDDR5", "Hynix"));
        assert_eq!((hw.installed(), hw.source.as_str(), hw.note), (Some(16 << 30), "system_profiler", None));
        assert_eq!(hw.cpus, [Cpu { vendor: "Apple".into(), name: "Apple M2 Pro".into(), cores: 12, ..Cpu::default() }]);
    }

    #[test]
    fn an_intel_mac_lists_its_slots() {
        let xml = plist(
            r#"<dict><key>_dataType</key><string>SPMemoryDataType</string><key>_items</key><array><dict>
                <key>_name</key><string>Memory Slots</string>
                <key>global_ecc_state</key><string>ecc_disabled</string>
                <key>_items</key><array>
                  <dict><key>_name</key><string>BANK 0/ChannelA-DIMM0</string><key>dimm_size</key><string>8 GB</string>
                    <key>dimm_speed</key><string>2667 MHz</string><key>dimm_type</key><string>DDR4</string>
                    <key>dimm_manufacturer</key><string>0x80CE</string><key>dimm_part_number</key><string>0x4D34373141314B3433444231</string>
                    <key>dimm_status</key><string>ok</string></dict>
                  <dict><key>_name</key><string>BANK 2/ChannelB-DIMM0</string><key>dimm_size</key><string>empty</string>
                    <key>dimm_type</key><string>empty</string><key>dimm_status</key><string>empty</string></dict>
                </array>
              </dict></array></dict>"#,
        );
        let hw = parse(&xml);
        assert!(!hw.unified);
        assert_eq!((hw.used(), hw.slots_total(), hw.ecc), (1, 2, Some(Ecc::None)));
        let s = &hw.slots[0];
        assert_eq!((s.locator.as_str(), s.size, s.speed, s.kind.as_str()), ("BANK 0/ChannelA-DIMM0", Some(8 << 30), 2667, "DDR4"));
        assert_eq!((s.vendor.as_str(), s.part.as_str()), ("Samsung", "M471A1K43DB1"));
        let e = &hw.slots[1];
        assert_eq!((e.present, e.size, e.kind.as_str()), (false, None, ""));
    }

    #[test]
    fn an_intel_mac_pro_has_a_processor_per_package() {
        let xml = plist(
            r#"<dict><key>_dataType</key><string>SPHardwareDataType</string><key>_items</key><array><dict>
                <key>cpu_type</key><string>6-Core Intel Xeon</string>
                <key>number_processors</key><integer>12</integer>
                <key>packages</key><integer>2</integer>
                <key>l3_cache</key><string>12 MB</string>
              </dict></array></dict>"#,
        );
        let cpus = parse(&xml).cpus;
        assert_eq!(cpus.len(), 2);
        assert_eq!(
            (cpus[1].vendor.as_str(), cpus[1].name.as_str(), cpus[1].cores, cpus[1].cache),
            ("Intel", "6-Core Intel Xeon", 6, [0, 0, 12 << 20])
        );
    }

    #[test]
    fn what_cannot_be_read_says_it_failed() {
        assert_eq!(parse(b"not a plist").note, Some(Note::Failed));
        assert_eq!(unhex("0x00"), "0x00");
        assert_eq!(unhex("0xZZZZ"), "0xZZZZ");
        assert_eq!(bytes("512 MB"), Some(512 << 20));
        assert_eq!(bytes("empty"), None);
    }
}
