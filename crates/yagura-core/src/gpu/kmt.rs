//! Windows: the sensors Task Manager shows, from D3DKMT. Each adapter the
//! counters found is opened once by its LUID and asked for its caps once;
//! every tick then asks for its temperature, fan and first engine's clock.
//! What the driver does not keep stays zero and is left out.

use std::collections::HashMap;
use std::ffi::c_void;

use windows::Wdk::Graphics::Direct3D::{
    D3DKMT_ADAPTER_PERFDATA, D3DKMT_ADAPTER_PERFDATACAPS, D3DKMT_CLOSEADAPTER, D3DKMT_NODE_PERFDATA, D3DKMT_OPENADAPTERFROMLUID,
    D3DKMT_QUERYADAPTERINFO, D3DKMTCloseAdapter, D3DKMTOpenAdapterFromLuid, D3DKMTQueryAdapterInfo, KMTQAITYPE_ADAPTERPERFDATA,
    KMTQAITYPE_ADAPTERPERFDATA_CAPS, KMTQAITYPE_NODEPERFDATA, KMTQUERYADAPTERINFOTYPE,
};
use windows::Win32::Foundation::LUID;

use super::windows::{Perf, apply_perf, luid_parts};
use super::{Device, Kind};

struct Open {
    handle: u32,
    temp_max: u32,
    fan_max: u32,
}

/// The adapters opened so far, by device id; `None` for one that would not
/// open, which is not tried again.
#[derive(Default)]
pub struct Reader {
    open: HashMap<String, Option<Open>>,
}

fn query<T>(handle: u32, kind: KMTQUERYADAPTERINFOTYPE, out: &mut T) -> bool {
    let mut q = D3DKMT_QUERYADAPTERINFO {
        hAdapter: handle,
        Type: kind,
        pPrivateDriverData: (out as *mut T).cast::<c_void>(),
        PrivateDriverDataSize: size_of::<T>() as u32,
    };
    // SAFETY: an open adapter and a buffer of the size given, of the type the
    // query fills.
    unsafe { D3DKMTQueryAdapterInfo(&mut q) }.is_ok()
}

fn open(high: i32, low: u32) -> Option<Open> {
    let mut o = D3DKMT_OPENADAPTERFROMLUID { AdapterLuid: LUID { LowPart: low, HighPart: high }, hAdapter: 0 };
    // SAFETY: the handle is written on success and closed by `Reader`'s drop.
    if unsafe { D3DKMTOpenAdapterFromLuid(&mut o) }.is_err() {
        return None;
    }
    let mut caps = D3DKMT_ADAPTER_PERFDATACAPS::default();
    if !query(o.hAdapter, KMTQAITYPE_ADAPTERPERFDATA_CAPS, &mut caps) {
        caps = D3DKMT_ADAPTER_PERFDATACAPS::default();
    }
    Some(Open { handle: o.hAdapter, temp_max: caps.TemperatureMax, fan_max: caps.MaxFanRPM })
}

fn perf(o: &Open) -> Perf {
    let mut p = Perf { temp_max: o.temp_max, fan_max: o.fan_max, ..Perf::default() };
    let mut a = D3DKMT_ADAPTER_PERFDATA::default();
    if query(o.handle, KMTQAITYPE_ADAPTERPERFDATA, &mut a) {
        p.temp = a.Temperature;
        p.fan_rpm = a.FanRPM;
    }
    let mut n = D3DKMT_NODE_PERFDATA::default();
    if query(o.handle, KMTQAITYPE_NODEPERFDATA, &mut n) {
        p.freq = n.Frequency;
    }
    p
}

impl Reader {
    pub fn fill(&mut self, devs: &mut [Device]) {
        for d in devs.iter_mut().filter(|d| d.kind == Kind::Gpu) {
            let Some((high, low)) = luid_parts(&d.id) else { continue };
            let o = self.open.entry(d.id.clone()).or_insert_with(|| open(high, low));
            if let Some(o) = o {
                apply_perf(d, &perf(o));
            }
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        for o in self.open.values().flatten() {
            // SAFETY: a handle this reader opened, closed once.
            let _ = unsafe { D3DKMTCloseAdapter(&D3DKMT_CLOSEADAPTER { hAdapter: o.handle }) };
        }
    }
}
