//! Settings: `~/.config/yagura/tui.toml` (or `$XDG_CONFIG_HOME/yagura/tui.toml`, or the file `YAGURA_TUI_CONFIG` names),
//! with the environment on top. A missing or broken file means the defaults; nothing here stops the screen.

use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// A stopped guest not started for this many days is stale.
    pub stale_days: u64,
    /// The root disk of a guest is full from here (percent).
    pub disk_full: f64,
    /// A storage is full from here (percent).
    pub storage_full: f64,
    /// The hottest thermal zone is hot from here (degrees Celsius).
    pub temp_hot: f32,
    /// The host CPU use is high from here (percent); the header reads red.
    pub cpu_hot: f64,
    /// The host memory use is high from here (percent); the header reads red.
    pub mem_full: f64,
    /// Ring the terminal bell when a new alert appears.
    pub bell: bool,
    /// The cleanup keeps this many of the newest backups of each guest.
    pub keep_backups: usize,
    /// The cleanup removes snapshots older than this many days.
    pub snapshot_days: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            stale_days: 7,
            disk_full: 90.0,
            storage_full: 90.0,
            temp_hot: 85.0,
            cpu_hot: 90.0,
            mem_full: 90.0,
            bell: true,
            keep_backups: 3,
            snapshot_days: 30,
        }
    }
}

impl Config {
    /// The file's values over the defaults; unknown keys and wrong types are ignored.
    pub fn from_toml(text: &str) -> Config {
        let mut c = Config::default();
        let Ok(t) = text.parse::<toml::Table>() else { return c };
        let num = |k: &str| t.get(k).and_then(|v| v.as_float().or_else(|| v.as_integer().map(|i| i as f64)));
        if let Some(v) = num("stale_days").filter(|v| *v >= 0.0) {
            c.stale_days = v as u64;
        }
        if let Some(v) = num("disk_full").filter(|v| *v > 0.0) {
            c.disk_full = v;
        }
        if let Some(v) = num("storage_full").filter(|v| *v > 0.0) {
            c.storage_full = v;
        }
        if let Some(v) = num("temp_hot").filter(|v| *v > 0.0) {
            c.temp_hot = v as f32;
        }
        if let Some(v) = num("cpu_hot").filter(|v| *v > 0.0) {
            c.cpu_hot = v;
        }
        if let Some(v) = num("mem_full").filter(|v| *v > 0.0) {
            c.mem_full = v;
        }
        if let Some(v) = t.get("bell").and_then(|v| v.as_bool()) {
            c.bell = v;
        }
        if let Some(v) = num("keep_backups").filter(|v| *v >= 1.0) {
            c.keep_backups = v as usize;
        }
        if let Some(v) = num("snapshot_days").filter(|v| *v >= 1.0) {
            c.snapshot_days = v as u64;
        }
        c
    }

    /// The environment over this: `YAGURA_STALE_DAYS`, `YAGURA_DISK_FULL`, `YAGURA_STORAGE_FULL`, `YAGURA_TEMP_HOT`, `YAGURA_BELL=0`.
    pub fn with_env(mut self, get: impl Fn(&str) -> Option<String>) -> Config {
        let num = |k: &str| get(k).and_then(|v| v.trim().parse::<f64>().ok());
        if let Some(v) = num("YAGURA_STALE_DAYS").filter(|v| *v >= 0.0) {
            self.stale_days = v as u64;
        }
        if let Some(v) = num("YAGURA_DISK_FULL").filter(|v| *v > 0.0) {
            self.disk_full = v;
        }
        if let Some(v) = num("YAGURA_STORAGE_FULL").filter(|v| *v > 0.0) {
            self.storage_full = v;
        }
        if let Some(v) = num("YAGURA_TEMP_HOT").filter(|v| *v > 0.0) {
            self.temp_hot = v as f32;
        }
        if let Some(v) = num("YAGURA_CPU_HOT").filter(|v| *v > 0.0) {
            self.cpu_hot = v;
        }
        if let Some(v) = num("YAGURA_MEM_FULL").filter(|v| *v > 0.0) {
            self.mem_full = v;
        }
        if let Some(v) = num("YAGURA_KEEP_BACKUPS").filter(|v| *v >= 1.0) {
            self.keep_backups = v as usize;
        }
        if let Some(v) = num("YAGURA_SNAPSHOT_DAYS").filter(|v| *v >= 1.0) {
            self.snapshot_days = v as u64;
        }
        if let Some(v) = get("YAGURA_BELL") {
            self.bell = !matches!(v.trim(), "0" | "off" | "false" | "no");
        }
        self
    }
}

/// Where the file is looked for.
pub fn path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("YAGURA_TUI_CONFIG") {
        return Some(p.into());
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("yagura").join("tui.toml"))
}

/// The settings, read once.
pub fn get() -> &'static Config {
    static CFG: OnceLock<Config> = OnceLock::new();
    CFG.get_or_init(|| {
        let file = path().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| Config::from_toml(&t)).unwrap_or_default();
        file.with_env(|k| std::env::var(k).ok())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_then_environment() {
        let c = Config::from_toml("stale_days = 14\ndisk_full = 80.5\nbell = false\nbogus = 1\nstorage_full = \"x\"\n");
        assert_eq!((c.stale_days, c.disk_full, c.storage_full, c.bell), (14, 80.5, 90.0, false));
        let c = c.with_env(|k| (k == "YAGURA_STALE_DAYS").then(|| "3".to_string()));
        assert_eq!(c.stale_days, 3);
        assert!(!c.bell);
        let c = Config::default().with_env(|k| (k == "YAGURA_BELL").then(|| "0".to_string()));
        assert!(!c.bell);
        assert_eq!(Config::from_toml("not toml ["), Config::default());
        let c = Config::from_toml(
            "cpu_hot = 75
mem_full = 0
",
        );
        assert_eq!((c.cpu_hot, c.mem_full), (75.0, 90.0));
        let c = c.with_env(|k| (k == "YAGURA_MEM_FULL").then(|| "80".to_string()));
        assert_eq!(c.mem_full, 80.0);
        let c = Config::from_toml("keep_backups = 5\nsnapshot_days = 0\n");
        assert_eq!((c.keep_backups, c.snapshot_days), (5, 30));
        let c = c.with_env(|k| (k == "YAGURA_SNAPSHOT_DAYS").then(|| "14".to_string()));
        assert_eq!(c.snapshot_days, 14);
    }
}
