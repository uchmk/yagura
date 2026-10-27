//! Windows: HWiNFO's shared memory (`Global\HWiNFO_SENS_SM2`), there while
//! HWiNFO runs with "Shared Memory Support" on. HWiNFO reads the board's
//! chips and the cores through its own driver; yagura only reads what it
//! publishes, so it needs no driver and no administrator.
//!
//! The layout is HWiNFO's SDK (`hwisenssm2.h`), packed: a header, then the
//! sensors (the chips) and the readings, each an array whose element size
//! the header gives, so newer versions' longer elements still read. [`parse`]
//! is a pure function over a copy of the bytes; [`read`] opens the mapping,
//! copies it and closes it again each time, so a HWiNFO that quits is not
//! kept alive and one that starts is found on the next tick.

use super::{Kind, Sensor};

/// The signature while HWiNFO is publishing; it writes `DEAD` when it stops.
const LIVE: &[u8; 4] = b"HWiS";
const HEADER: usize = 44;
const NAME: usize = 128;
const UNIT: usize = 16;

/// A reading's type, from `SENSOR_READING_TYPE`.
const TEMP: u32 = 1;
const FAN: u32 = 3;

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn f64_at(b: &[u8], at: usize) -> Option<f64> {
    Some(f64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// A NUL-terminated string of `len` bytes at `at`: UTF-8 when `utf8`, else
/// the ANSI code page, read as Latin-1 (the names are ASCII but for `°`).
fn text(b: &[u8], at: usize, len: usize, utf8: bool) -> Option<String> {
    let raw = b.get(at..at + len)?;
    let raw = &raw[..raw.iter().position(|&c| c == 0).unwrap_or(len)];
    let s = if utf8 { String::from_utf8_lossy(raw).into_owned() } else { raw.iter().map(|&c| char::from(c)).collect() };
    Some(s.trim().to_string())
}

/// The name a user gave (HWiNFO lets them rename), else the original: the
/// UTF-8 one where the element is long enough to have it.
fn name(e: &[u8], user: usize, orig: usize, utf8: usize) -> String {
    let u8name = (e.len() >= utf8 + NAME).then(|| text(e, utf8, NAME, true)).flatten().filter(|s| !s.is_empty());
    u8name.or_else(|| text(e, user, NAME, false).filter(|s| !s.is_empty())).or_else(|| text(e, orig, NAME, false)).unwrap_or_default()
}

/// The size the header says the published data takes, to copy no more than
/// that; `None` when HWiNFO is not publishing.
pub fn size(b: &[u8]) -> Option<usize> {
    if b.get(..4)? != LIVE {
        return None;
    }
    let end = |off: usize, size: usize, count: usize| -> Option<usize> {
        let (o, s, n) = (u32_at(b, off)? as usize, u32_at(b, size)? as usize, u32_at(b, count)? as usize);
        o.checked_add(s.checked_mul(n)?)
    };
    Some(end(20, 24, 28)?.max(end(32, 36, 40)?).max(HEADER))
}

/// The temperatures and fans in a copy of the shared memory, grouped by the
/// sensor (chip) they belong to; `None` when it is not HWiNFO's or HWiNFO
/// has stopped.
pub fn parse(b: &[u8]) -> Option<Vec<Sensor>> {
    if b.len() < HEADER || b.get(..4)? != LIVE {
        return None;
    }
    let (s_off, s_size, s_count) = (u32_at(b, 20)? as usize, u32_at(b, 24)? as usize, u32_at(b, 28)? as usize);
    let (r_off, r_size, r_count) = (u32_at(b, 32)? as usize, u32_at(b, 36)? as usize, u32_at(b, 40)? as usize);
    // The fields read below, with no UTF-8 names.
    if s_size < 8 + 2 * NAME || r_size < 12 + 2 * NAME + UNIT + 8 {
        return None;
    }
    let element = |off: usize, size: usize, i: usize| b.get(off + i * size..off + (i + 1) * size);
    let chips: Vec<(String, String, String)> = (0..s_count)
        .map_while(|i| element(s_off, s_size, i))
        .map(|e| {
            let id = format!("hwinfo:{:x}:{}", u32_at(e, 0).unwrap_or(0), u32_at(e, 4).unwrap_or(0));
            (id, name(e, 136, 8, 264), text(e, 8, NAME, false).unwrap_or_default())
        })
        .collect();
    let mut out = Vec::new();
    for i in 0..r_count {
        let Some(e) = element(r_off, r_size, i) else { break };
        let kind = match u32_at(e, 0)? {
            TEMP => Kind::Temp,
            FAN => Kind::Fan,
            _ => continue,
        };
        let Some((chip_id, chip, chip_orig)) = chips.get(u32_at(e, 4)? as usize) else { continue };
        let Some(v) = f64_at(e, 284).filter(|v| v.is_finite()) else { continue };
        let unit = (e.len() >= 444 + UNIT).then(|| text(e, 444, UNIT, true)).flatten().filter(|u| !u.is_empty());
        let unit = unit.or_else(|| text(e, 268, UNIT, false)).unwrap_or_default();
        // Temperatures in °F when the user chose them.
        let value = if kind == Kind::Temp && unit.ends_with('F') { (v - 32.0) * 5.0 / 9.0 } else { v } as f32;
        let label_orig = text(e, 12, NAME, false).unwrap_or_default();
        out.push(Sensor {
            id: format!("{chip_id}/{:x}", u32_at(e, 8)?),
            chip_id: chip_id.clone(),
            chip: chip.clone(),
            label: name(e, 140, 12, 316),
            kind,
            value,
            source: format!("HWiNFO: {chip_orig} / {label_orig}"),
            ..Sensor::default()
        });
    }
    // HWiNFO lists a chip's readings by type; keep each chip's together.
    let order = |s: &Sensor| chips.iter().position(|c| c.0 == s.chip_id);
    out.sort_by_key(order);
    Some(out)
}

/// The readings while HWiNFO publishes them, `None` otherwise.
#[cfg(windows)]
pub fn read() -> Option<Vec<Sensor>> {
    use ::windows::Win32::Foundation::CloseHandle;
    use ::windows::Win32::System::Memory::{
        FILE_MAP_READ, MEMORY_BASIC_INFORMATION, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery,
    };
    use ::windows::core::w;

    // SAFETY: the handle and the view are released below on every path; the
    // view is read only within the size VirtualQuery gives for it.
    unsafe {
        let map = OpenFileMappingW(FILE_MAP_READ.0, false, w!("Global\\HWiNFO_SENS_SM2")).ok()?;
        let view = MapViewOfFile(map, FILE_MAP_READ, 0, 0, 0);
        let copy = (!view.Value.is_null()).then(|| {
            let mut info = MEMORY_BASIC_INFORMATION::default();
            let got = VirtualQuery(Some(view.Value), &mut info, size_of::<MEMORY_BASIC_INFORMATION>());
            let all = if got == 0 { 0 } else { info.RegionSize };
            let bytes = std::slice::from_raw_parts(view.Value as *const u8, all);
            let want = bytes.get(..HEADER).and_then(size).unwrap_or(0).min(all);
            bytes[..want].to_vec()
        });
        if !view.Value.is_null() {
            let _ = UnmapViewOfFile(view);
        }
        let _ = CloseHandle(map);
        parse(&copy?)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A sensor element of `size` bytes: id, instance, the original name
    /// and the user's (and the UTF-8 one where the element has room).
    fn chip(size: usize, id: u32, orig: &str, user: &str) -> Vec<u8> {
        let mut e = vec![0u8; size];
        e[0..4].copy_from_slice(&id.to_le_bytes());
        e[8..8 + orig.len()].copy_from_slice(orig.as_bytes());
        e[136..136 + user.len()].copy_from_slice(user.as_bytes());
        if size >= 264 + NAME {
            e[264..264 + user.len()].copy_from_slice(user.as_bytes());
        }
        e
    }

    fn reading(size: usize, kind: u32, chip: u32, id: u32, label: &[u8], unit: &[u8], v: f64) -> Vec<u8> {
        let mut e = vec![0u8; size];
        e[0..4].copy_from_slice(&kind.to_le_bytes());
        e[4..8].copy_from_slice(&chip.to_le_bytes());
        e[8..12].copy_from_slice(&id.to_le_bytes());
        e[12..12 + label.len()].copy_from_slice(label);
        e[140..140 + label.len()].copy_from_slice(label);
        e[268..268 + unit.len()].copy_from_slice(unit);
        e[284..292].copy_from_slice(&v.to_le_bytes());
        e
    }

    /// Shared memory as HWiNFO lays it out: the header, the sensors, the readings.
    pub fn memory(chips: &[Vec<u8>], readings: &[Vec<u8>]) -> Vec<u8> {
        let (cs, rs) = (chips[0].len(), readings[0].len());
        let s_off = HEADER;
        let r_off = s_off + cs * chips.len();
        let mut b = vec![0u8; HEADER];
        b[0..4].copy_from_slice(LIVE);
        for (at, v) in [(20, s_off), (24, cs), (28, chips.len()), (32, r_off), (36, rs), (40, readings.len())] {
            b[at..at + 4].copy_from_slice(&(v as u32).to_le_bytes());
        }
        chips.iter().chain(readings).for_each(|e| b.extend_from_slice(e));
        b
    }

    #[test]
    fn temperatures_and_fans_by_chip() {
        // Version 1 elements: no UTF-8 names.
        let (cs, rs) = (264, 316);
        let chips = [chip(cs, 0xf000_0300, "CPU [#0]: AMD Ryzen 7 7800X3D", ""), chip(cs, 0xe000_0001, "Nuvoton NCT6799D", "Board")];
        let readings = [
            reading(rs, TEMP, 1, 0x1000000, b"CPU", b"\xb0C", 41.5),
            reading(rs, 2, 1, 0x2000000, b"Vcore", b"V", 1.1),
            reading(rs, FAN, 1, 0x3000000, b"Chassis1", b"RPM", 812.0),
            reading(rs, TEMP, 0, 0x1000000, b"CPU (Tctl/Tdie)", b"\xb0F", 122.0),
            reading(rs, TEMP, 9, 0x1000001, b"orphan", b"\xb0C", 30.0),
        ];
        let b = memory(&chips, &readings);
        assert_eq!(size(&b), Some(b.len()));
        let list = parse(&b).unwrap();
        let got: Vec<(&str, &str, Kind, f32)> = list.iter().map(|s| (s.chip.as_str(), s.label.as_str(), s.kind, s.value)).collect();
        assert_eq!(
            got,
            [
                ("CPU [#0]: AMD Ryzen 7 7800X3D", "CPU (Tctl/Tdie)", Kind::Temp, 50.0),
                ("Board", "CPU", Kind::Temp, 41.5),
                ("Board", "Chassis1", Kind::Fan, 812.0),
            ]
        );
        assert_eq!(list[1].chip_id, "hwinfo:e0000001:0");
        assert_eq!(list[1].id, "hwinfo:e0000001:0/1000000");
        assert_eq!(list[1].source, "HWiNFO: Nuvoton NCT6799D / CPU");
    }

    #[test]
    fn longer_elements_and_utf8_names() {
        let (cs, rs) = (264 + NAME + 8, 444 + UNIT + 4);
        let mut r = reading(rs, TEMP, 0, 7, b"Drive", b"", 35.0);
        r[316..316 + "ドライブ".len()].copy_from_slice("ドライブ".as_bytes());
        r[444..444 + "°F".len()].copy_from_slice("°F".as_bytes());
        let b = memory(&[chip(cs, 1, "S.M.A.R.T.: Samsung SSD 980", "SSD")], &[r]);
        let list = parse(&b).unwrap();
        // The UTF-8 unit wins over the empty ANSI one: 35 °F.
        assert_eq!((list[0].chip.as_str(), list[0].label.as_str()), ("SSD", "ドライブ"));
        assert!((list[0].value - 1.666).abs() < 0.01);
    }

    #[test]
    fn stopped_or_short_reads_nothing() {
        let mut b = memory(&[chip(264, 1, "a", "")], &[reading(316, TEMP, 0, 1, b"t", b"C", 1.0)]);
        // A copy cut short keeps the readings that fit.
        assert_eq!(parse(&b[..b.len() - 1]).map(|l| l.len()), Some(0));
        b[0..4].copy_from_slice(b"DEAD");
        assert_eq!(parse(&b), None);
        assert_eq!(size(&b), None);
        assert_eq!(parse(&b[..10]), None);
        // Elements too short for the fields.
        let mut b = memory(&[chip(264, 1, "a", "")], &[reading(316, TEMP, 0, 1, b"t", b"C", 1.0)]);
        b[36..40].copy_from_slice(&100u32.to_le_bytes());
        assert_eq!(parse(&b), None);
    }
}
