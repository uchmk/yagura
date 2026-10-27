//! Linux: the raw SMBIOS table in `/sys/firmware/dmi/tables` (root's to
//! read), else the world-readable summary in `/sys/class/dmi/id` (no slots),
//! else the device tree's model on boards without SMBIOS. Whether the machine
//! booted through UEFI is whether `/sys/firmware/efi` exists.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use super::{Bios, Hardware, Note, Part, clean, smbios};

/// Everything under `root` (`/`, or a test's directory).
pub fn read(root: &Path) -> Hardware {
    let table = fs::read(root.join("sys/firmware/dmi/tables/DMI")).map_err(|e| e.kind());
    from(root, table)
}

/// What there is, given how reading the table went.
fn from(root: &Path, table: Result<Vec<u8>, ErrorKind>) -> Hardware {
    let fw = root.join("sys/firmware");
    let mut hw = match table {
        Ok(table) if !table.is_empty() => {
            let mut hw = smbios::parse(&table);
            let ver = fs::read(fw.join("dmi/tables/smbios_entry_point")).ok().and_then(|ep| smbios::entry_version(&ep));
            hw.source = match ver {
                Some((major, minor)) => format!("SMBIOS {major}.{minor} (/sys/firmware/dmi/tables)"),
                None => "SMBIOS (/sys/firmware/dmi/tables)".into(),
            };
            hw
        }
        Err(ErrorKind::PermissionDenied) => {
            let mut hw = dmi_id(&root.join("sys/class/dmi/id"));
            hw.note = Some(Note::Root);
            hw
        }
        _ => {
            let mut hw = dmi_id(&root.join("sys/class/dmi/id"));
            if hw.source.is_empty()
                && let Some(model) = fs::read(fw.join("devicetree/base/model")).ok().map(|b| clean(&String::from_utf8_lossy(&b)))
                && !model.is_empty()
            {
                hw.system.name = model;
                hw.source = "/sys/firmware/devicetree".into();
            }
            hw.note = Some(Note::NoTable);
            hw
        }
    };
    hw.bios.uefi = Some(fw.join("efi").is_dir());
    hw
}

/// The kernel's summary of the table, one file per fact.
fn dmi_id(dir: &Path) -> Hardware {
    let f = |name: &str| fs::read_to_string(dir.join(name)).map(|s| clean(&s)).unwrap_or_default();
    let system = Part {
        vendor: f("sys_vendor"),
        name: f("product_name"),
        version: f("product_version"),
        family: f("product_family"),
        sku: f("product_sku"),
    };
    let board = Part { vendor: f("board_vendor"), name: f("board_name"), version: f("board_version"), ..Part::default() };
    let bios =
        Bios { vendor: f("bios_vendor"), version: f("bios_version"), date: f("bios_date"), release: f("bios_release"), ..Bios::default() };
    let mut hw = Hardware { system, board, bios, ..Hardware::default() };
    if dir.is_dir() {
        hw.source = "/sys/class/dmi/id".into();
    }
    hw
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(root: &Path, path: &str, body: &[u8]) {
        let p = root.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    #[test]
    fn the_table_gives_the_slots_and_its_version() {
        let root = crate::util::test_dir("hw-table");
        put(&root, "sys/firmware/dmi/tables/DMI", &smbios::tests::table());
        put(&root, "sys/firmware/dmi/tables/smbios_entry_point", b"_SM3_\x18\x00\x03\x05\x00");
        fs::create_dir_all(root.join("sys/firmware/efi")).unwrap();
        let hw = read(&root);
        assert_eq!(hw.source, "SMBIOS 3.5 (/sys/firmware/dmi/tables)");
        assert_eq!((hw.slots.len(), hw.note, hw.bios.uefi), (4, None, Some(true)));
        assert_eq!(hw.board.name, "ROG STRIX B650E-F");
    }

    #[test]
    fn without_root_the_summary_says_why_there_are_no_slots() {
        let root = crate::util::test_dir("hw-dmi-id");
        for (name, body) in [
            ("sys_vendor", "LENOVO\n"),
            ("product_name", "21HM\n"),
            ("product_version", "ThinkPad X1 Carbon Gen 11\n"),
            ("product_family", "ThinkPad X1 Carbon Gen 11\n"),
            ("board_vendor", "LENOVO\n"),
            ("board_name", "21HMCTO1WW\n"),
            ("board_version", "Not Defined\n"),
            ("bios_vendor", "LENOVO\n"),
            ("bios_version", "N3XET53W (1.28 )\n"),
            ("bios_date", "08/23/2024\n"),
            ("bios_release", "1.28\n"),
        ] {
            put(&root, &format!("sys/class/dmi/id/{name}"), body.as_bytes());
        }
        let hw = from(&root, Err(ErrorKind::PermissionDenied));
        assert_eq!((hw.note, hw.source.as_str(), hw.bios.uefi), (Some(Note::Root), "/sys/class/dmi/id", Some(false)));
        assert_eq!((hw.system.vendor.as_str(), hw.system.version.as_str()), ("LENOVO", "ThinkPad X1 Carbon Gen 11"));
        assert_eq!((hw.bios.version.as_str(), hw.bios.release.as_str()), ("N3XET53W (1.28 )", "1.28"));
        assert!(hw.slots.is_empty());
    }

    #[test]
    fn a_board_without_smbios_gives_its_device_tree_model() {
        let root = crate::util::test_dir("hw-dt");
        put(&root, "sys/firmware/devicetree/base/model", b"Raspberry Pi 5 Model B Rev 1.0\0");
        let hw = read(&root);
        assert_eq!((hw.system.name.as_str(), hw.source.as_str()), ("Raspberry Pi 5 Model B Rev 1.0", "/sys/firmware/devicetree"));
        assert_eq!(hw.note, Some(Note::NoTable));
        // Nothing at all: still a note, so the screen can say why.
        let hw = read(&crate::util::test_dir("hw-none"));
        assert_eq!((hw.note, hw.source.as_str(), hw.system.name.as_str()), (Some(Note::NoTable), "", ""));
    }
}
