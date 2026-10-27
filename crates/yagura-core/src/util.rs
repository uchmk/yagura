//! Test helpers.

/// A fresh directory of its own under the system's temp directory, so tests
/// never read the machine's real configuration.
#[cfg(test)]
pub fn test_dir(label: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("yagura-core-test-{}-{label}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create a test directory");
    dir
}
