//! Windows performance counters (PDH), shared by the gpu and disk readers.

use ::windows::Win32::System::Performance::{
    PDH_CSTATUS_NEW_DATA, PDH_FMT, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_MORE_DATA, PdhGetFormattedCounterArrayW,
};

/// Doubles, not capped at 100 (an engine summed over processes can pass it
/// for a moment; the gpu fold caps after summing).
const FMT: PDH_FMT = PDH_FMT(PDH_FMT_DOUBLE.0 | 0x8000);

/// Every instance of a wildcard counter with its value.
pub fn array(h: PDH_HCOUNTER) -> Vec<(String, f64)> {
    // Instances come and go between the two calls; a few tries settle it.
    for _ in 0..3 {
        let (mut size, mut count) = (0u32, 0u32);
        // SAFETY: asking for the size only.
        let r = unsafe { PdhGetFormattedCounterArrayW(h, FMT, &mut size, &mut count, None) };
        if r != PDH_MORE_DATA || size == 0 {
            return Vec::new();
        }
        // u64s so the items (which hold an f64) are aligned.
        let mut buf = vec![0u64; (size as usize).div_ceil(8)];
        let items = buf.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        // SAFETY: the buffer is `size` bytes or more, as PDH asked.
        let r = unsafe { PdhGetFormattedCounterArrayW(h, FMT, &mut size, &mut count, Some(items)) };
        if r == PDH_MORE_DATA {
            continue;
        }
        if r != 0 {
            return Vec::new();
        }
        // SAFETY: PDH wrote `count` items at the start of the buffer; their
        // names point into the rest of it, which lives until we return.
        let items = unsafe { std::slice::from_raw_parts(items, count as usize) };
        return items
            .iter()
            .filter(|i| i.FmtValue.CStatus == 0 || i.FmtValue.CStatus == PDH_CSTATUS_NEW_DATA)
            .filter_map(|i| {
                // SAFETY: as above; the value was asked for as a double.
                let name = unsafe { i.szName.to_string() }.ok()?;
                Some((name, unsafe { i.FmtValue.Anonymous.doubleValue }))
            })
            .collect();
    }
    Vec::new()
}
