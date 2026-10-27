//! Windows: the `Thermal Zone Information` performance counters (PDH), which
//! any user can read. The folding is in `windows.rs`.

use std::ptr::null_mut;

use ::windows::Win32::System::Performance::{
    PDH_HCOUNTER, PDH_HQUERY, PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhOpenQueryW,
};
use ::windows::core::{PCWSTR, w};

use super::Sensor;
use super::windows::fold;
use crate::pdh::array;

pub struct Reader {
    query: PDH_HQUERY,
    precise: Option<PDH_HCOUNTER>,
    whole: Option<PDH_HCOUNTER>,
}

impl Reader {
    /// `None` when the machine has no thermal zone counters.
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
        let precise = add(w!("\\Thermal Zone Information(*)\\High Precision Temperature"));
        let whole = add(w!("\\Thermal Zone Information(*)\\Temperature"));
        if precise.is_none() && whole.is_none() {
            // SAFETY: the query was opened above.
            unsafe { PdhCloseQuery(query) };
            return None;
        }
        Some(Self { query, precise, whole })
    }

    pub fn read(&mut self) -> Vec<Sensor> {
        // SAFETY: the query is open while self lives.
        if unsafe { PdhCollectQueryData(self.query) } != 0 {
            return Vec::new();
        }
        fold(&self.precise.map(array).unwrap_or_default(), &self.whole.map(array).unwrap_or_default())
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        // SAFETY: opened in `open`, closed once here (it frees the counters too).
        unsafe { PdhCloseQuery(self.query) };
    }
}
