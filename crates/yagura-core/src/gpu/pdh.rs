//! Windows: the GPU performance counters (PDH) and the adapters (DXGI).
//! Opened once on the sampler thread; each reading collects the query and
//! hands the counters to [`fold`].

use std::ptr::null_mut;

use ::windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIDevice, IDXGIFactory1};
use ::windows::Win32::System::Performance::{
    PDH_HCOUNTER, PDH_HQUERY, PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhOpenQueryW,
};
use ::windows::core::{Interface, PCWSTR, w};

use super::windows::{Adapter, fold, luid_name, umd_version};
use super::{Device, vendor_name};
use crate::pdh::array;

pub struct Reader {
    query: PDH_HQUERY,
    engines: PDH_HCOUNTER,
    dedicated: Option<PDH_HCOUNTER>,
    shared: Option<PDH_HCOUNTER>,
    proc_mem: Option<PDH_HCOUNTER>,
    adapters: Vec<Adapter>,
}

impl Reader {
    /// `None` when the counters are not there (before Windows 10 1709, or
    /// with the performance counters disabled).
    pub fn open() -> Option<Self> {
        let mut query = PDH_HQUERY(null_mut());
        // SAFETY: plain out-parameter calls; the handles are closed in Drop.
        unsafe {
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }
        }
        let add = |path: PCWSTR| {
            let mut h = PDH_HCOUNTER(null_mut());
            // SAFETY: as above.
            let ok = unsafe { PdhAddEnglishCounterW(query, path, 0, &mut h) } == 0;
            ok.then_some(h)
        };
        let Some(engines) = add(w!("\\GPU Engine(*)\\Utilization Percentage")) else {
            // SAFETY: the query was opened above.
            unsafe { PdhCloseQuery(query) };
            return None;
        };
        let dedicated = add(w!("\\GPU Adapter Memory(*)\\Dedicated Usage"));
        let shared = add(w!("\\GPU Adapter Memory(*)\\Shared Usage"));
        let proc_mem = add(w!("\\GPU Process Memory(*)\\Dedicated Usage"));
        // A rate counter needs a first collection to measure from.
        // SAFETY: as above.
        unsafe { PdhCollectQueryData(query) };
        Some(Self { query, engines, dedicated, shared, proc_mem, adapters: adapters() })
    }

    pub fn read(&mut self) -> Vec<Device> {
        // SAFETY: the query is open while self lives.
        if unsafe { PdhCollectQueryData(self.query) } != 0 {
            return Vec::new();
        }
        let opt = |h: Option<PDH_HCOUNTER>| h.map(array).unwrap_or_default();
        fold(&self.adapters, &array(self.engines), &opt(self.dedicated), &opt(self.shared), &opt(self.proc_mem))
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        // SAFETY: opened in `open`, closed once here (it frees the counters too).
        unsafe { PdhCloseQuery(self.query) };
    }
}

/// The adapters DXGI lists, read once (they do not change while running,
/// short of an eGPU being plugged in).
fn adapters() -> Vec<Adapter> {
    // SAFETY: DXGI's factory needs no COM initialisation.
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else { return Vec::new() };
    let mut out: Vec<Adapter> = Vec::new();
    for i in 0u32.. {
        // SAFETY: enumeration ends with an error past the last adapter.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else { break };
        // SAFETY: as above.
        let Ok(d) = (unsafe { adapter.GetDesc1() }) else { continue };
        let luid = luid_name(d.AdapterLuid.HighPart, d.AdapterLuid.LowPart);
        if out.iter().any(|a| a.luid == luid) {
            continue;
        }
        // The user-mode driver's version; DXGI answers it for IDXGIDevice.
        // SAFETY: a GUID pointer for the call to read.
        let driver = unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) }.ok().map(umd_version);
        let len = d.Description.iter().position(|&c| c == 0).unwrap_or(d.Description.len());
        out.push(Adapter {
            luid,
            name: String::from_utf16_lossy(&d.Description[..len]).trim().to_string(),
            vendor: vendor_name(d.VendorId),
            pci: Some(format!("{:04x}:{:04x}", d.VendorId, d.DeviceId)),
            dedicated: d.DedicatedVideoMemory as u64,
            shared: d.SharedSystemMemory as u64,
            driver,
            software: d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
        });
    }
    out
}
