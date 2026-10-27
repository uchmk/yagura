//! macOS: the calls to the `AppleSMC` user client. The keys are listed once
//! when the reader opens (each by its index), the named temperatures and the
//! fans kept, and only those read every tick.

use std::mem::size_of;

use objc2_core_foundation::{CFDictionary, CFRetained};
use objc2_io_kit::{
    IOConnectCallStructMethod, IOObjectRelease, IOServiceClose, IOServiceGetMatchingService, IOServiceMatching, IOServiceOpen,
};

use super::mac::{self, KeyData, KeyInfo};
use super::{Kind, Sensor};

/// `kIOMainPortDefault`.
const MAIN_PORT: u32 = 0;
/// More fans than any Mac has; a bad `FNum` lists no more.
const MOST_FANS: u32 = 8;

#[derive(Clone, Copy)]
struct Key {
    code: u32,
    info: KeyInfo,
}

struct Temp {
    key: Key,
    raw: String,
    name: String,
}

struct Fan {
    raw: String,
    actual: Key,
    min: Option<Key>,
    max: Option<Key>,
}

pub struct Smc {
    conn: u32,
    temps: Vec<Temp>,
    fans: Vec<Fan>,
}

impl Smc {
    pub fn open() -> Option<Self> {
        // SAFETY: a C string.
        let matching = unsafe { IOServiceMatching(c"AppleSMC".as_ptr()) }?;
        // SAFETY: the dictionary is consumed; the service is released below.
        let service = unsafe { IOServiceGetMatchingService(MAIN_PORT, Some(CFRetained::<CFDictionary>::from(&matching))) };
        if service == 0 {
            return None;
        }
        let mut conn = 0;
        // SAFETY: reads this task's port, which needs nothing.
        #[allow(deprecated)]
        let task = unsafe { libc::mach_task_self() };
        // SAFETY: a live service and this task; the connection is written on
        // success and closed when the reader drops.
        let ok = unsafe { IOServiceOpen(service, task, 0, &mut conn) } == 0;
        IOObjectRelease(service);
        if !ok {
            return None;
        }
        let mut smc = Self { conn, temps: Vec::new(), fans: Vec::new() };
        let count = smc.value("#KEY")? as u32;
        let keys: Vec<String> = (0..count).filter_map(|i| smc.key_at(i)).collect();
        for (raw, name) in mac::temp_names(&keys) {
            let Some(key) = smc.key(&raw) else { continue };
            if smc.read(key).is_some_and(mac::plausible) {
                smc.temps.push(Temp { key, raw, name });
            }
        }
        let fans = smc.value("FNum").unwrap_or(0.0) as u32;
        for i in 0..fans.min(MOST_FANS) {
            let [actual, min, max] = mac::fan_keys(i);
            if let Some(key) = smc.key(&actual) {
                smc.fans.push(Fan { actual: key, min: smc.key(&min), max: smc.key(&max), raw: actual });
            }
        }
        Some(smc)
    }

    /// Whether `id` is a key this reader lists.
    pub fn has(&self, id: &str) -> bool {
        self.temps.iter().any(|t| t.raw == id)
    }

    pub fn read_all(&self) -> Vec<Sensor> {
        let base = Sensor { chip_id: "smc".into(), chip: "SMC".into(), ..Sensor::default() };
        let mut out = Vec::new();
        for t in &self.temps {
            let Some(value) = self.read(t.key).filter(|v| mac::plausible(*v)) else { continue };
            out.push(Sensor {
                id: format!("smc/{}", t.raw),
                label: t.name.clone(),
                value,
                source: format!("SMC {}", t.raw),
                ..base.clone()
            });
        }
        for (i, f) in self.fans.iter().enumerate() {
            let Some(value) = self.read(f.actual) else { continue };
            out.push(Sensor {
                id: format!("smc/{}", f.raw),
                label: format!("Fan {}", i + 1),
                kind: Kind::Fan,
                value,
                high: f.max.and_then(|k| self.read(k)),
                min: f.min.and_then(|k| self.read(k)),
                source: format!("SMC {}", f.raw),
                ..base.clone()
            });
        }
        out
    }

    fn call(&self, input: &KeyData) -> Option<KeyData> {
        let mut out = KeyData::default();
        let mut len = size_of::<KeyData>();
        // SAFETY: an open connection and two parameter blocks of the size the
        // SMC takes; the output's length is written back.
        let r = unsafe {
            IOConnectCallStructMethod(
                self.conn,
                mac::SELECTOR,
                (input as *const KeyData).cast(),
                size_of::<KeyData>(),
                (&mut out as *mut KeyData).cast(),
                &mut len,
            )
        };
        (r == 0 && out.result == 0).then_some(out)
    }

    fn key(&self, name: &str) -> Option<Key> {
        let code = mac::code(name);
        let out = self.call(&KeyData { key: code, data8: mac::READ_KEYINFO, ..KeyData::default() })?;
        (1..=32).contains(&out.info.size).then_some(Key { code, info: out.info })
    }

    fn key_at(&self, index: u32) -> Option<String> {
        let out = self.call(&KeyData { data8: mac::READ_INDEX, data32: index, ..KeyData::default() })?;
        Some(mac::name(out.key))
    }

    fn read(&self, key: Key) -> Option<f32> {
        let mut input = KeyData { key: key.code, data8: mac::READ_BYTES, ..KeyData::default() };
        input.info.size = key.info.size;
        let out = self.call(&input)?;
        mac::decode(key.info.ty, &out.bytes[..key.info.size as usize])
    }

    fn value(&self, name: &str) -> Option<f32> {
        self.read(self.key(name)?)
    }
}

impl Drop for Smc {
    fn drop(&mut self) {
        IOServiceClose(self.conn);
    }
}
