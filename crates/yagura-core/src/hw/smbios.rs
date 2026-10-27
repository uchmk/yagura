//! The SMBIOS structure table, the firmware's own description of the
//! machine: the BIOS (type 0), the system (1), the board (2), each processor
//! (4) and its caches (7), the memory arrays (16) and each memory slot (17).
//! Pure, so the tests run everywhere.

use std::collections::HashMap;

use super::{Cpu, Ecc, Hardware, Slot, clean, maker};

/// One structure: its formatted bytes (the header included, so offsets read
/// as the specification gives them) and its strings.
struct Entry<'a> {
    kind: u8,
    data: &'a [u8],
    strings: Vec<&'a [u8]>,
}

impl Entry<'_> {
    fn byte(&self, off: usize) -> Option<u8> {
        self.data.get(off).copied()
    }

    fn word(&self, off: usize) -> Option<u16> {
        Some(u16::from_le_bytes(self.data.get(off..off + 2)?.try_into().ok()?))
    }

    fn dword(&self, off: usize) -> Option<u32> {
        Some(u32::from_le_bytes(self.data.get(off..off + 4)?.try_into().ok()?))
    }

    fn qword(&self, off: usize) -> Option<u64> {
        Some(u64::from_le_bytes(self.data.get(off..off + 8)?.try_into().ok()?))
    }

    /// The string the byte at `off` points to (1-based; 0 is none), blanks
    /// cleared.
    fn string(&self, off: usize) -> String {
        let Some(i) = self.byte(off).filter(|&i| i > 0) else { return String::new() };
        self.strings.get(usize::from(i) - 1).map(|s| clean(&String::from_utf8_lossy(s))).unwrap_or_default()
    }
}

/// The structures in order, up to the end-of-table one or the first that
/// runs past the end.
fn entries(table: &[u8]) -> Vec<Entry<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= table.len() {
        let kind = table[i];
        let len = usize::from(table[i + 1]);
        if len < 4 || i + len > table.len() {
            break;
        }
        // The strings follow, each ended by a NUL, the set by another.
        let rest = &table[i + len..];
        let Some(end) = rest.windows(2).position(|w| w == [0, 0]) else { break };
        let strings = if end == 0 { Vec::new() } else { rest[..end].split(|&b| b == 0).collect() };
        out.push(Entry { kind, data: &table[i..i + len], strings });
        i += len + end + 2;
        if kind == 127 {
            break;
        }
    }
    out
}

/// The table out of Windows' `RawSMBIOSData`: the SMBIOS version, then the
/// structures.
#[cfg(any(windows, test))]
pub fn rsmb(buf: &[u8]) -> Option<((u8, u8), &[u8])> {
    let len = u32::from_le_bytes(buf.get(4..8)?.try_into().ok()?) as usize;
    let table = buf.get(8..)?;
    Some(((buf[1], buf[2]), &table[..len.min(table.len())]))
}

/// The SMBIOS version from Linux's `smbios_entry_point`: the 64-bit `_SM3_`
/// one or the older `_SM_`.
#[cfg(any(target_os = "linux", test))]
pub fn entry_version(ep: &[u8]) -> Option<(u8, u8)> {
    if ep.starts_with(b"_SM3_") {
        Some((*ep.get(7)?, *ep.get(8)?))
    } else if ep.starts_with(b"_SM_") {
        Some((*ep.get(6)?, *ep.get(7)?))
    } else {
        None
    }
}

/// The memory type byte of a slot (type 17, offset 0x12).
fn mem_kind(b: u8) -> &'static str {
    match b {
        0x03 => "DRAM",
        0x07 => "RAM",
        0x0F => "SDRAM",
        0x11 => "RDRAM",
        0x12 => "DDR",
        0x13 => "DDR2",
        0x14 => "DDR2 FB-DIMM",
        0x18 => "DDR3",
        0x1A => "DDR4",
        0x1B => "LPDDR",
        0x1C => "LPDDR2",
        0x1D => "LPDDR3",
        0x1E => "LPDDR4",
        0x1F => "Logical non-volatile",
        0x20 => "HBM",
        0x21 => "HBM2",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        0x24 => "HBM3",
        _ => "",
    }
}

/// The form factor byte of a slot (type 17, offset 0x0E).
fn form(b: u8) -> &'static str {
    match b {
        0x03 => "SIMM",
        0x04 => "SIP",
        0x05 => "Chip",
        0x06 => "DIP",
        0x07 => "ZIP",
        0x09 => "DIMM",
        0x0A => "TSOP",
        0x0B => "RIMM",
        0x0C => "SODIMM",
        0x0D => "SRIMM",
        0x0E => "FB-DIMM",
        0x0F => "Die",
        0x10 => "CAMM",
        _ => "",
    }
}

/// A slot's size in bytes: 0 is an empty slot, `0xFFFF` a module of unknown
/// size, `0x7FFF` points to the extended size; the top bit means KiB.
fn size(e: &Entry) -> (bool, Option<u64>) {
    match e.word(0x0C) {
        None | Some(0) => (false, None),
        Some(0xFFFF) => (true, None),
        Some(0x7FFF) => (true, e.dword(0x1C).map(|mib| u64::from(mib & 0x7FFF_FFFF) << 20)),
        Some(w) if w & 0x8000 != 0 => (true, Some(u64::from(w & 0x7FFF) << 10)),
        Some(w) => (true, Some(u64::from(w) << 20)),
    }
}

/// A speed word that points to a dword further on when it is `0xFFFF`.
fn speed(e: &Entry, off: usize, ext: usize) -> u32 {
    match e.word(off) {
        Some(0xFFFF) => e.dword(ext).unwrap_or(0),
        Some(w) => u32::from(w),
        None => 0,
    }
}

fn slot(e: &Entry) -> Slot {
    let (present, size) = size(e);
    // The details of an empty slot are the firmware's leftovers.
    let s = |off: usize| if present { e.string(off) } else { String::new() };
    Slot {
        locator: e.string(0x10),
        bank: e.string(0x11),
        present,
        size,
        kind: if present { mem_kind(e.byte(0x12).unwrap_or(0)).to_string() } else { String::new() },
        form: form(e.byte(0x0E).unwrap_or(0)).to_string(),
        speed: if present { speed(e, 0x15, 0x54) } else { 0 },
        configured: if present { speed(e, 0x20, 0x58) } else { 0 },
        vendor: maker(&s(0x17)),
        part: s(0x1A),
        rank: if present { e.byte(0x1B).unwrap_or(0) & 0x0F } else { 0 },
        mv: if present { e.word(0x26).unwrap_or(0) } else { 0 },
    }
}

/// A cache's installed size in bytes (type 7): a word whose top bit means
/// 64 KiB units, else KiB; `0xFFFF` points to a dword further on, whose top
/// bit says the same.
fn cache_size(e: &Entry) -> u64 {
    match e.word(0x09) {
        Some(0xFFFF) => match e.dword(0x17) {
            Some(d) if d & 0x8000_0000 != 0 => u64::from(d & 0x7FFF_FFFF) << 16,
            Some(d) => u64::from(d) << 10,
            None => 0,
        },
        Some(w) if w & 0x8000 != 0 => u64::from(w & 0x7FFF) << 16,
        Some(w) => u64::from(w) << 10,
        None => 0,
    }
}

/// A count byte that points to a word further on when it is `0xFF`.
fn count(e: &Entry, off: usize, ext: usize) -> u32 {
    match e.byte(off) {
        Some(0xFF) => u32::from(e.word(ext).unwrap_or(0)),
        Some(b) => u32::from(b),
        None => 0,
    }
}

/// A processor (type 4); `caches` maps each cache's handle to its size.
fn cpu(e: &Entry, caches: &HashMap<u16, u64>) -> Cpu {
    let cache = |off: usize| e.word(off).and_then(|h| caches.get(&h)).copied().unwrap_or(0);
    Cpu {
        socket: e.string(0x04),
        vendor: e.string(0x07),
        name: e.string(0x10),
        cores: count(e, 0x23, 0x2A),
        enabled: count(e, 0x24, 0x2C),
        threads: count(e, 0x25, 0x2E),
        max_mhz: u32::from(e.word(0x14).unwrap_or(0)),
        cache: [cache(0x1A), cache(0x1C), cache(0x1E)],
    }
}

fn ecc(b: u8) -> Option<Ecc> {
    match b {
        3 => Some(Ecc::None),
        4 => Some(Ecc::Parity),
        5 => Some(Ecc::Single),
        6 => Some(Ecc::Multi),
        7 => Some(Ecc::Crc),
        _ => None,
    }
}

/// What the table says. `source` and the notes are the caller's.
pub fn parse(table: &[u8]) -> Hardware {
    let list = entries(table);
    let mut hw = Hardware::default();
    // The arrays that hold the system's memory, not a cache's or a flash's.
    let mut arrays = Vec::new();
    for e in list.iter().filter(|e| e.kind == 16 && e.byte(0x05) == Some(3)) {
        arrays.extend(e.word(0x02));
        hw.slot_count += u32::from(e.word(0x0D).unwrap_or(0));
        hw.max_mem += match e.dword(0x07) {
            Some(0x8000_0000) => e.qword(0x0F).unwrap_or(0),
            Some(kib) => u64::from(kib) << 10,
            None => 0,
        };
        hw.ecc = hw.ecc.or(e.byte(0x06).and_then(ecc));
    }
    let caches: HashMap<u16, u64> =
        list.iter().filter(|e| e.kind == 7).filter_map(|e| Some((e.word(0x02)?, cache_size(e)))).filter(|&(_, b)| b > 0).collect();
    for e in &list {
        match e.kind {
            0 => {
                let b = &mut hw.bios;
                b.vendor = e.string(0x04);
                b.version = e.string(0x05);
                b.date = e.string(0x08);
                if let (Some(major), Some(minor)) = (e.byte(0x14), e.byte(0x15))
                    && major != 0xFF
                {
                    b.release = format!("{major}.{minor}");
                }
                // Characteristics extension byte 2: UEFI supported, a VM.
                if let Some(x) = e.byte(0x13) {
                    b.uefi = Some(x & 0x08 != 0);
                    b.vm = x & 0x10 != 0;
                }
            }
            1 => {
                let p = &mut hw.system;
                (p.vendor, p.name, p.version) = (e.string(0x04), e.string(0x05), e.string(0x06));
                (p.sku, p.family) = (e.string(0x19), e.string(0x1A));
            }
            2 if hw.board.name.is_empty() => {
                let p = &mut hw.board;
                (p.vendor, p.name, p.version) = (e.string(0x04), e.string(0x05), e.string(0x06));
            }
            // A central processor in a filled socket.
            4 if e.byte(0x05) == Some(3) && e.byte(0x18).is_some_and(|b| b & 0x40 != 0) => hw.cpus.push(cpu(e, &caches)),
            17 if e.data.len() >= 0x15 => {
                let array = e.word(0x04).unwrap_or(0);
                if arrays.is_empty() || arrays.contains(&array) {
                    hw.slots.push(slot(e));
                }
            }
            _ => {}
        }
    }
    hw
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A structure: its type, handle and the formatted bytes after the
    /// 4-byte header, then its strings.
    pub fn entry(kind: u8, handle: u16, body: &[u8], strings: &[&str]) -> Vec<u8> {
        let mut v = vec![kind, (body.len() + 4) as u8];
        v.extend(handle.to_le_bytes());
        v.extend(body);
        for s in strings {
            v.extend(s.as_bytes());
            v.push(0);
        }
        if strings.is_empty() {
            v.push(0);
        }
        v.push(0);
        v
    }

    /// Bytes from offset 4 up, with `put` writing at the specification's
    /// offsets.
    pub fn body(len: usize, put: &[(usize, &[u8])]) -> Vec<u8> {
        let mut b = vec![0u8; len - 4];
        for &(off, bytes) in put {
            b[off - 4..off - 4 + bytes.len()].copy_from_slice(bytes);
        }
        b
    }

    /// A small desktop: BIOS, system, board, one array of four slots, two
    /// of them filled (one with an extended size and speed).
    pub fn table() -> Vec<u8> {
        let mut t = Vec::new();
        t.extend(entry(
            0,
            0,
            &body(0x1A, &[(0x04, &[1]), (0x05, &[2]), (0x08, &[3]), (0x13, &[0x08]), (0x14, &[5, 27])]),
            &["American Megatrends International, LLC.", "1663", "04/01/2024"],
        ));
        t.extend(entry(
            1,
            1,
            &body(0x1B, &[(0x04, &[1]), (0x05, &[2]), (0x06, &[3]), (0x19, &[4]), (0x1A, &[5])]),
            &["ASUS", "System Product Name", "1.0", "SKU", "Desktop"],
        ));
        t.extend(entry(
            2,
            2,
            &body(0x0F, &[(0x04, &[1]), (0x05, &[2]), (0x06, &[3])]),
            &["ASUSTeK COMPUTER INC.", "ROG STRIX B650E-F", "Rev 1.xx"],
        ));
        // 128 GiB most, single-bit ECC, four slots.
        t.extend(entry(
            16,
            0x30,
            &body(0x17, &[(0x05, &[3]), (0x06, &[5]), (0x07, &(128u32 << 20).to_le_bytes()), (0x0D, &4u16.to_le_bytes())]),
            &[],
        ));
        // A cache's array, whose slots do not count.
        t.extend(entry(16, 0x31, &body(0x17, &[(0x05, &[5]), (0x0D, &1u16.to_le_bytes())]), &[]));
        let dimm = |handle: u16, array: u16, size: u16, ext: u32, speed: u16, ext_speed: u32, strings: &[&str]| {
            let mut put: Vec<(usize, Vec<u8>)> = vec![
                (0x04, array.to_le_bytes().to_vec()),
                (0x0C, size.to_le_bytes().to_vec()),
                (0x0E, vec![0x09]),
                (0x10, vec![1]),
                (0x11, vec![2]),
                (0x12, vec![0x22]),
                (0x15, speed.to_le_bytes().to_vec()),
                (0x17, vec![3]),
                (0x1A, vec![4]),
                (0x1B, vec![0x02]),
                (0x1C, ext.to_le_bytes().to_vec()),
                (0x20, 4800u16.to_le_bytes().to_vec()),
                (0x26, 1100u16.to_le_bytes().to_vec()),
                (0x54, ext_speed.to_le_bytes().to_vec()),
            ];
            put.sort();
            let put: Vec<(usize, &[u8])> = put.iter().map(|(o, b)| (*o, b.as_slice())).collect();
            entry(17, handle, &body(0x5C, &put), strings)
        };
        t.extend(dimm(0x40, 0x30, 0x7FFF, 32 << 10, 0xFFFF, 6000, &["DIMM_A1", "BANK 0", "80CE000080CE", "M323R4GA3DB0-CWM   "]));
        t.extend(dimm(0x41, 0x30, 0, 0, 0, 0, &["DIMM_A2", "BANK 1", "Unknown", "NO DIMM"]));
        t.extend(dimm(0x42, 0x30, 16 << 10, 0, 5600, 0, &["DIMM_B1", "BANK 2", "Kingston", "KF556C40-16"]));
        t.extend(dimm(0x43, 0x30, 0, 0, 0, 0, &["DIMM_B2", "BANK 3"]));
        t.extend(dimm(0x44, 0x31, 1, 0, 0, 0, &["CACHE"]));
        // The processor and its caches: 1 MiB of L1, 16 MiB of L2 in 64K
        // units, 64 MiB of L3 through the extended size.
        let cache = |handle: u16, level: u8, size: u16, ext: u32| {
            entry(
                7,
                handle,
                &body(0x1B, &[(0x05, &[0x80 | (level - 1), 0x01]), (0x09, &size.to_le_bytes()), (0x17, &ext.to_le_bytes())]),
                &[],
            )
        };
        t.extend(cache(0x50, 1, 1024, 0));
        t.extend(cache(0x51, 2, 0x8000 | 256, 0));
        t.extend(cache(0x52, 3, 0xFFFF, 0x8000_0000 | 1024));
        let handles: Vec<u8> = [0x50u16, 0x51, 0x52].iter().flat_map(|h| h.to_le_bytes()).collect();
        t.extend(entry(
            4,
            0x60,
            &body(
                0x30,
                &[
                    (0x04, &[1]),
                    (0x05, &[3]),
                    (0x07, &[2]),
                    (0x10, &[3]),
                    (0x14, &5700u16.to_le_bytes()),
                    (0x16, &4500u16.to_le_bytes()),
                    (0x18, &[0x41]),
                    (0x1A, &handles),
                    (0x23, &[16, 16, 32]),
                ],
            ),
            &["AM5", "Advanced Micro Devices, Inc.", "AMD Ryzen 9 7950X 16-Core Processor            "],
        ));
        // An empty second socket.
        t.extend(entry(4, 0x61, &body(0x30, &[(0x04, &[1]), (0x05, &[3]), (0x18, &[0x00])]), &["CPU 2"]));
        t.extend(entry(127, 0xFFFF, &[], &[]));
        t
    }

    #[test]
    fn a_desktop_table_reads_its_bios_system_board_and_slots() {
        let hw = parse(&table());
        assert_eq!(hw.bios.vendor, "American Megatrends International, LLC.");
        assert_eq!((hw.bios.version.as_str(), hw.bios.date.as_str(), hw.bios.release.as_str()), ("1663", "04/01/2024", "5.27"));
        assert_eq!((hw.bios.uefi, hw.bios.vm), (Some(true), false));
        // The board maker's blank product name is dropped.
        assert_eq!((hw.system.vendor.as_str(), hw.system.name.as_str(), hw.system.family.as_str()), ("ASUS", "", "Desktop"));
        assert_eq!((hw.board.vendor.as_str(), hw.board.name.as_str()), ("ASUSTeK COMPUTER INC.", "ROG STRIX B650E-F"));
        assert_eq!((hw.slot_count, hw.max_mem, hw.ecc), (4, 128 << 30, Some(Ecc::Single)));
        let names: Vec<&str> = hw.slots.iter().map(|s| s.locator.as_str()).collect();
        assert_eq!(names, ["DIMM_A1", "DIMM_A2", "DIMM_B1", "DIMM_B2"]);
        let a1 = &hw.slots[0];
        assert_eq!((a1.present, a1.size, a1.kind.as_str(), a1.form.as_str()), (true, Some(32 << 30), "DDR5", "DIMM"));
        assert_eq!((a1.speed, a1.configured, a1.rank, a1.mv), (6000, 4800, 2, 1100));
        assert_eq!((a1.vendor.as_str(), a1.part.as_str()), ("Samsung", "M323R4GA3DB0-CWM"));
        // An empty slot keeps its place and form but none of the leftovers.
        let a2 = &hw.slots[1];
        assert_eq!((a2.present, a2.size, a2.kind.as_str(), a2.vendor.as_str(), a2.speed), (false, None, "", "", 0));
        assert_eq!(hw.slots[2].size, Some(16 << 30));
        assert_eq!(hw.slots[2].speed, 5600);
        assert_eq!((hw.used(), hw.installed()), (2, Some(48 << 30)));
    }

    #[test]
    fn processors_read_their_counts_and_caches() {
        let hw = parse(&table());
        assert_eq!(hw.cpus.len(), 1);
        let c = &hw.cpus[0];
        assert_eq!(
            (c.socket.as_str(), c.vendor.as_str(), c.name.as_str()),
            ("AM5", "Advanced Micro Devices, Inc.", "AMD Ryzen 9 7950X 16-Core Processor")
        );
        assert_eq!((c.cores, c.enabled, c.threads, c.max_mhz), (16, 16, 32, 5700));
        assert_eq!(c.cache, [1 << 20, 16 << 20, 64 << 20]);
    }

    #[test]
    fn big_counts_and_short_processors() {
        // More than 255 cores: the byte says 0xFF and the word holds them.
        let mut put: Vec<(usize, &[u8])> = vec![(0x05, &[3]), (0x18, &[0x40]), (0x1A, &[0xFF; 6]), (0x23, &[0xFF, 0xFF, 0xFF])];
        let words: Vec<u8> = [288u16, 288, 576].iter().flat_map(|n| n.to_le_bytes()).collect();
        put.push((0x2A, &words));
        let mut t = entry(4, 1, &body(0x30, &put), &[]);
        // SMBIOS 2.0: no caches, no counts.
        t.extend(entry(4, 2, &body(0x1A, &[(0x05, &[3]), (0x18, &[0x41]), (0x14, &3000u16.to_le_bytes())]), &[]));
        // Not a central processor.
        t.extend(entry(4, 3, &body(0x30, &[(0x05, &[4]), (0x18, &[0x41])]), &[]));
        t.extend(entry(127, 4, &[], &[]));
        let cpus = parse(&t).cpus;
        assert_eq!(cpus.len(), 2);
        assert_eq!((cpus[0].cores, cpus[0].enabled, cpus[0].threads, cpus[0].cache), (288, 288, 576, [0; 3]));
        assert_eq!((cpus[1].cores, cpus[1].threads, cpus[1].max_mhz), (0, 0, 3000));
    }

    #[test]
    fn sizes_in_kib_unknown_and_short_entries() {
        let one = |size: u16, len: usize| {
            let mut t = entry(17, 1, &body(len, &[(0x0C, &size.to_le_bytes())]), &[]);
            t.extend(entry(127, 2, &[], &[]));
            parse(&t).slots
        };
        assert_eq!(one(0x8000 | 512, 0x15)[0].size, Some(512 << 10));
        assert_eq!(one(0xFFFF, 0x15)[0], Slot { present: true, ..Slot::default() });
        // Too short for a slot (before SMBIOS 2.1).
        assert!(one(1024, 0x13).is_empty());
    }

    #[test]
    fn a_cut_table_keeps_what_came_before_the_cut() {
        let t = table();
        let cut = &t[..t.len() / 2];
        let hw = parse(cut);
        assert_eq!(hw.bios.version, "1663");
        assert!(hw.slots.len() < 4);
        assert_eq!(parse(&[]), Hardware::default());
        assert_eq!(parse(&[1, 2, 3]), Hardware::default());
    }

    #[test]
    fn a_virtual_machine_says_so() {
        let mut t = entry(0, 0, &body(0x18, &[(0x13, &[0x10])]), &[]);
        t.extend(entry(127, 1, &[], &[]));
        let b = parse(&t).bios;
        assert_eq!((b.vm, b.uefi, b.release.as_str()), (true, Some(false), "0.0"));
    }

    #[test]
    fn the_headers_give_the_version() {
        let mut buf = vec![0, 3, 4, 0];
        let t = table();
        buf.extend((t.len() as u32).to_le_bytes());
        buf.extend(&t);
        buf.extend([0xAA; 8]);
        let (ver, table) = rsmb(&buf).unwrap();
        assert_eq!((ver, table.len()), ((3, 4), t.len()));
        assert!(rsmb(&[0, 3]).is_none());
        assert_eq!(entry_version(b"_SM3_\x18\x00\x03\x07\x00"), Some((3, 7)));
        assert_eq!(entry_version(b"_SM_\x00\x1f\x02\x08"), Some((2, 8)));
        assert_eq!(entry_version(b"junk"), None);
    }
}
