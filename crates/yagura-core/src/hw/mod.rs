//! The hardware that does not change while running: the system's maker and
//! model, the firmware, the board, the processors and each memory slot. Read once, on a
//! thread of its own so the first CPU reading never waits for it (macOS
//! spawns `system_profiler`, which takes a second or so).
//!
//! Windows and Linux read the firmware's SMBIOS table (`smbios.rs`): Windows
//! through `GetSystemFirmwareTable`, which needs no administrator; Linux from
//! `/sys/firmware/dmi/tables/DMI`, which only root may read, falling back to
//! the world-readable `/sys/class/dmi/id` files (no slots) and to the device
//! tree's model. macOS asks `system_profiler` for its hardware and memory.
//! The parsing is kept in pure functions so the tests run everywhere.

#[cfg(any(target_os = "linux", test))]
pub mod linux;
#[cfg(any(target_os = "macos", test))]
pub mod mac;
#[cfg(any(windows, target_os = "linux", test))]
pub mod smbios;
#[cfg(windows)]
mod windows;

use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};

use super::Msg;

/// A maker's part: the system or the board.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Part {
    pub vendor: String,
    pub name: String,
    pub version: String,
    /// The system's family (`ThinkPad X1`); empty for the board.
    pub family: String,
    /// The maker's order number, or the Mac's model number.
    pub sku: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Bios {
    pub vendor: String,
    pub version: String,
    /// As the firmware writes it, usually `MM/DD/YYYY`.
    pub date: String,
    /// The firmware's own release number, `5.27`.
    pub release: String,
    /// Whether the machine booted through UEFI; `None` where not known.
    pub uefi: Option<bool>,
    /// The firmware says it is a virtual machine's.
    pub vm: bool,
}

/// How the memory checks itself. macOS only ever says it is off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub enum Ecc {
    None,
    Parity,
    Single,
    Multi,
    Crc,
}

impl Ecc {
    pub const ALL: [Ecc; 5] = [Self::None, Self::Parity, Self::Single, Self::Multi, Self::Crc];

    pub fn key(self) -> &'static str {
        match self {
            Self::None => "sys.ecc.none",
            Self::Parity => "sys.ecc.parity",
            Self::Single => "sys.ecc.single",
            Self::Multi => "sys.ecc.multi",
            Self::Crc => "sys.ecc.crc",
        }
    }
}

/// A processor socket, filled, as the firmware describes it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Cpu {
    /// Where it sits on the board: `AM5`, `CPU 1`.
    pub socket: String,
    pub vendor: String,
    /// The model as the firmware writes it.
    pub name: String,
    /// 0 when not said, as are the rest.
    pub cores: u32,
    pub enabled: u32,
    pub threads: u32,
    /// The fastest the board lets it run, in MHz.
    pub max_mhz: u32,
    /// The caches by level (L1, L2, L3) in bytes, as the firmware adds them
    /// up: some count every core's, some one core's.
    pub cache: [u64; 3],
}

/// A memory slot, filled or not.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Slot {
    /// Where it is on the board: `DIMM A1`, `ChannelA-DIMM0`.
    pub locator: String,
    pub bank: String,
    pub present: bool,
    /// Bytes; `None` when a module is there but its size is not said.
    pub size: Option<u64>,
    /// `DDR5`, `LPDDR4`.
    pub kind: String,
    /// `DIMM`, `SODIMM`.
    pub form: String,
    /// The module's rated speed in MT/s; 0 when not said.
    pub speed: u32,
    /// The speed it runs at in MT/s; 0 when not said.
    pub configured: u32,
    pub vendor: String,
    pub part: String,
    /// 0 when not said.
    pub rank: u8,
    /// The configured voltage in millivolts; 0 when not said.
    pub mv: u16,
}

/// Why the slots are missing or the facts are few.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Note {
    /// Linux: the SMBIOS table is root's to read.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Root,
    /// No SMBIOS table: a container, a virtual machine without one, or a
    /// board described by a device tree.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    NoTable,
    /// The OS would not hand the table over, or `system_profiler` failed.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    Failed,
}

impl Note {
    pub const ALL: [Note; 3] = [Self::Root, Self::NoTable, Self::Failed];

    pub fn key(self) -> &'static str {
        match self {
            Self::Root => "sys.note.root",
            Self::NoTable => "sys.note.no_table",
            Self::Failed => "sys.note.failed",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Hardware {
    pub system: Part,
    pub board: Part,
    pub bios: Bios,
    /// The filled processor sockets; empty where not said.
    pub cpus: Vec<Cpu>,
    pub slots: Vec<Slot>,
    /// The slots the board has, as its memory arrays say; 0 when not said.
    pub slot_count: u32,
    /// The most memory the board takes, in bytes; 0 when not said.
    pub max_mem: u64,
    pub ecc: Option<Ecc>,
    /// Where it was read: `SMBIOS 3.3 (GetSystemFirmwareTable)`.
    pub source: String,
    pub note: Option<Note>,
    /// The memory sits in the chip's package (Apple silicon), not in slots.
    pub unified: bool,
}

impl Hardware {
    /// The filled slots.
    pub fn used(&self) -> usize {
        self.slots.iter().filter(|s| s.present).count()
    }

    /// What the filled slots add up to, where every size is known.
    pub fn installed(&self) -> Option<u64> {
        self.slots.iter().filter(|s| s.present).map(|s| s.size).sum::<Option<u64>>().filter(|&b| b > 0)
    }

    /// The cores and threads of every socket added up; 0 where any is not said.
    pub fn cpu_counts(&self) -> (u32, u32) {
        let sum = |f: fn(&Cpu) -> u32| self.cpus.iter().map(|c| Some(f(c)).filter(|&n| n > 0)).sum::<Option<u32>>().unwrap_or(0);
        (sum(|c| c.cores), sum(|c| c.threads))
    }

    /// The slots the board has: the arrays' count, else the slots listed.
    pub fn slots_total(&self) -> usize {
        (self.slot_count as usize).max(self.slots.len())
    }
}

/// Read it once on a thread of its own and send it as [`Msg::Hardware`].
pub fn spawn(tx: Sender<Msg>, wake: crate::Wake) {
    let _ = std::thread::Builder::new().name("yagura-hw".into()).spawn(move || {
        let hw = read();
        if tx.send(Msg::Hardware(Box::new(hw))).is_ok()
            && let Some(w) = wake
        {
            w();
        }
    });
}

#[cfg(target_os = "linux")]
fn read() -> Hardware {
    linux::read(std::path::Path::new("/"))
}

#[cfg(windows)]
fn read() -> Hardware {
    windows::read()
}

#[cfg(target_os = "macos")]
fn read() -> Hardware {
    mac::read()
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
fn read() -> Hardware {
    Hardware { note: Some(Note::Failed), ..Hardware::default() }
}

/// Firmware strings that say nothing: the board makers' blanks.
pub fn clean(s: &str) -> String {
    const BLANK: [&str; 23] = [
        "to be filled by o.e.m.",
        "to be filled by oem",
        "default string",
        "not specified",
        "not available",
        "not applicable",
        "not defined",
        "n/a",
        "none",
        "null",
        "unknown",
        "undefined",
        "empty",
        "no dimm",
        "not installed",
        "system manufacturer",
        "system product name",
        "system version",
        "system serial number",
        "base board manufacturer",
        "o.e.m.",
        "oem",
        "0123456789",
    ];
    let t = s.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    let low = t.to_ascii_lowercase();
    if BLANK.contains(&low.as_str()) || t.chars().all(|c| c == '0' || c == 'x' || c == 'X' || c == '.' || c == '-') {
        String::new()
    } else {
        t.to_string()
    }
}

/// A memory maker from its JEDEC code, which some firmware writes instead of
/// the name (`80CE`, `CE00000000000000`); anything else as it is.
pub fn maker(s: &str) -> String {
    let t = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    let hex = t.len() >= 4 && t.chars().all(|c| c.is_ascii_hexdigit());
    if !hex {
        return s.trim().to_string();
    }
    let code = t[..4].to_ascii_uppercase();
    let name = match code.as_str() {
        "80CE" | "CE00" | "00CE" => "Samsung",
        "80AD" | "AD00" | "00AD" => "SK hynix",
        "802C" | "2C00" | "002C" => "Micron",
        "859B" | "9B85" => "Crucial",
        "0198" | "9801" => "Kingston",
        "04CD" | "CD04" => "G.Skill",
        "029E" | "9E02" => "Corsair",
        "830B" | "0B83" => "Nanya",
        "04CB" | "CB04" => "ADATA",
        "04EF" | "EF04" => "Team Group",
        "02FE" | "FE02" => "Elpida",
        "8551" | "5185" => "Qimonda",
        "8A45" | "458A" => "CXMT",
        _ => return s.trim().to_string(),
    };
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_firmware_strings_read_as_nothing() {
        for s in ["To Be Filled By O.E.M.", " Default string ", "Not Specified", "0123456789", "00000000", "NO DIMM", "\0"] {
            assert_eq!(clean(s), "", "{s:?}");
        }
        assert_eq!(clean(" ASUSTeK COMPUTER INC. "), "ASUSTeK COMPUTER INC.");
        assert_eq!(clean("1.0"), "1.0");
    }

    #[test]
    fn jedec_codes_name_their_maker() {
        assert_eq!(maker("80CE"), "Samsung");
        assert_eq!(maker("0x80AD"), "SK hynix");
        assert_eq!(maker("2C00000000000000"), "Micron");
        assert_eq!(maker("Kingston"), "Kingston");
        assert_eq!(maker("1234"), "1234");
    }

    #[test]
    fn installed_memory_needs_every_size() {
        let slot = |size| Slot { present: true, size, ..Slot::default() };
        let mut hw = Hardware { slots: vec![slot(Some(8 << 30)), slot(Some(8 << 30)), Slot::default()], ..Hardware::default() };
        assert_eq!((hw.used(), hw.installed(), hw.slots_total()), (2, Some(16 << 30), 3));
        hw.slots.push(slot(None));
        hw.slot_count = 4;
        assert_eq!((hw.installed(), hw.slots_total()), (None, 4));
    }

    #[test]
    fn sockets_add_up_only_when_each_is_known() {
        let cpu = |cores, threads| Cpu { cores, threads, ..Cpu::default() };
        let mut hw = Hardware { cpus: vec![cpu(16, 32), cpu(16, 32)], ..Hardware::default() };
        assert_eq!(hw.cpu_counts(), (32, 64));
        hw.cpus.push(cpu(0, 8));
        assert_eq!(hw.cpu_counts(), (0, 72));
        assert_eq!(Hardware::default().cpu_counts(), (0, 0));
    }
}
