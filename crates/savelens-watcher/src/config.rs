//! `config.toml` stored in the SaveLens data directory.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{EngineError, EngineResult};

/// Global hotkey bindings (syntax understood by `global-hotkey`, e.g.
/// `ctrl+shift+F5`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyConfig {
    pub enabled: bool,
    pub quick_snapshot: String,
    pub quick_rollback: String,
    pub panic_restore: String,
    pub toggle_trainer: String,
    pub toggle_overlay: String,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        HotkeyConfig {
            enabled: true,
            quick_snapshot: "ctrl+shift+F5".into(),
            quick_rollback: "ctrl+shift+F9".into(),
            panic_restore: "ctrl+shift+F12".into(),
            toggle_trainer: "ctrl+shift+F10".into(),
            toggle_overlay: "ctrl+shift+F11".into(),
        }
    }
}

/// Engine / daemon configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SaveLensConfig {
    /// Quiet period after the last filesystem event before a file is read.
    pub debounce_ms: u64,
    /// Quiet period after an `IN_CLOSE_WRITE` (shorter: the writer is done).
    pub close_write_quiet_ms: u64,
    /// Interval between the two size/mtime samples of the stability check.
    pub stability_ms: u64,
    /// After this long with the file still locked for writing, snapshot it
    /// read-only anyway (but never write to it).
    pub lock_timeout_ms: u64,
    /// Create commits automatically when a game saves.
    pub auto_commit: bool,
    /// Files bigger than this are not tracked.
    pub max_file_size_mb: u64,
    /// Maximum lines of semantic changelog stored per file and commit.
    pub max_changelog_lines: usize,
    pub hotkeys: HotkeyConfig,
}

impl Default for SaveLensConfig {
    fn default() -> Self {
        SaveLensConfig {
            debounce_ms: 1500,
            close_write_quiet_ms: 250,
            stability_ms: 120,
            lock_timeout_ms: 15_000,
            auto_commit: true,
            max_file_size_mb: 512,
            max_changelog_lines: 200,
            hotkeys: HotkeyConfig::default(),
        }
    }
}

impl SaveLensConfig {
    /// File name inside the data directory.
    pub const FILE: &'static str = "config.toml";

    /// Loads `<dir>/config.toml`, or defaults when it does not exist.
    pub fn load(dir: &Path) -> EngineResult<SaveLensConfig> {
        let path = dir.join(Self::FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| EngineError::Config(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SaveLensConfig::default()),
            Err(e) => Err(EngineError::Io { path: path.clone(), source: e }),
        }
    }

    /// Writes `<dir>/config.toml`.
    pub fn save(&self, dir: &Path) -> EngineResult<()> {
        let path = dir.join(Self::FILE);
        let text = toml::to_string_pretty(self).map_err(|e| EngineError::Config(e.to_string()))?;
        std::fs::write(&path, text).map_err(|e| EngineError::Io { path, source: e })
    }

    /// Maximum tracked file size in bytes.
    pub fn max_file_size(&self) -> u64 {
        self.max_file_size_mb.saturating_mul(1024 * 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let c = SaveLensConfig::load(dir.path()).unwrap();
        assert_eq!(c, SaveLensConfig::default());
        let mut c2 = c.clone();
        c2.debounce_ms = 10;
        c2.hotkeys.panic_restore = "ctrl+alt+F12".into();
        c2.save(dir.path()).unwrap();
        assert_eq!(SaveLensConfig::load(dir.path()).unwrap(), c2);
    }

    #[test]
    fn partial_file_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "debounce_ms = 42\n[hotkeys]\nenabled = false\n").unwrap();
        let c = SaveLensConfig::load(dir.path()).unwrap();
        assert_eq!(c.debounce_ms, 42);
        assert!(!c.hotkeys.enabled);
        assert_eq!(c.hotkeys.quick_snapshot, "ctrl+shift+F5");
        std::fs::write(dir.path().join("config.toml"), "debounce_ms = \"x\"").unwrap();
        assert!(SaveLensConfig::load(dir.path()).is_err());
    }
}
