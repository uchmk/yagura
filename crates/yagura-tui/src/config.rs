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
    /// Ring the terminal bell when a new alert appears.
    pub bell: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { stale_days: 7, disk_full: 90.0, storage_full: 90.0, temp_hot: 85.0, bell: true }
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
        if let Some(v) = t.get("bell").and_then(|v| v.as_bool()) {
            c.bell = v;
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
    }
}
