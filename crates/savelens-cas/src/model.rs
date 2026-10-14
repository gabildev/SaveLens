//! Records stored in the SaveLens database.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use savelens_core::{Location, ScalarValue};
use serde::{Deserialize, Serialize};

use crate::error::CasError;

/// Database id of a game profile.
pub type GameId = i64;
/// Database id of a commit.
pub type CommitId = i64;

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Human timestamp (`2026-10-14 17:03:12`) in local time.
pub fn format_ts(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms) {
        chrono::LocalResult::Single(t) => t.format("%Y-%m-%d %H:%M:%S").to_string(),
        _ => ms.to_string(),
    }
}

/// A tracked game.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GameRecord {
    pub id: GameId,
    pub name: String,
    pub app_id: Option<u32>,
    /// Directory containing the save files.
    pub save_root: PathBuf,
    /// Glob patterns (relative to `save_root`) of tracked files.
    pub patterns: Vec<String>,
    /// Free-form runtime description (`native`, `proton:<prefix>`, `windows`).
    pub runtime: String,
    pub created_at: i64,
    pub current_branch: String,
    pub watch_enabled: bool,
    /// Freeze-on-save rules active ("disk trainer").
    pub trainer_enabled: bool,
}

/// Why a commit exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommitKind {
    /// Created by the user.
    Manual,
    /// Created by the watcher when the game saved.
    Auto,
    /// Created after the disk trainer injected frozen values.
    Trainer,
    /// Created after a variable edit.
    Edit,
    /// Created after a cherry-pick.
    CherryPick,
    /// Created after checking out / restoring an older state.
    Restore,
    /// Initial import of a game.
    Import,
}

impl CommitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CommitKind::Manual => "manual",
            CommitKind::Auto => "auto",
            CommitKind::Trainer => "trainer",
            CommitKind::Edit => "edit",
            CommitKind::CherryPick => "cherry-pick",
            CommitKind::Restore => "restore",
            CommitKind::Import => "import",
        }
    }
}

impl fmt::Display for CommitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CommitKind {
    type Err = CasError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "manual" => CommitKind::Manual,
            "auto" => CommitKind::Auto,
            "trainer" => CommitKind::Trainer,
            "edit" => CommitKind::Edit,
            "cherry-pick" => CommitKind::CherryPick,
            "restore" => CommitKind::Restore,
            "import" => CommitKind::Import,
            other => return Err(CasError::Invalid(format!("unknown commit kind `{other}`"))),
        })
    }
}

/// A commit (snapshot of every tracked file of a game).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommitRecord {
    pub id: CommitId,
    pub game_id: GameId,
    pub parent: Option<CommitId>,
    pub branch: String,
    pub kind: CommitKind,
    pub message: String,
    pub created_at: i64,
    /// One-line semantic summary (`+500 Gold, +1 Level`).
    pub summary: String,
}

/// A file inside a commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitFile {
    /// Path relative to the save root, `/`-separated.
    pub rel_path: String,
    /// blake3 hex of the content.
    pub blob: String,
    pub size: u64,
    pub mtime: Option<i64>,
}

/// A change applied by a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub rel_path: String,
    /// New content, or `None` when the file was deleted.
    pub content: Option<Vec<u8>>,
    pub mtime: Option<i64>,
}

/// Semantic changelog of one file in a commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changelog {
    pub rel_path: String,
    pub summary: String,
    pub lines: Vec<String>,
}

/// A branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchRecord {
    pub name: String,
    pub head: Option<CommitId>,
    pub created_at: i64,
}

/// A tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagRecord {
    pub name: String,
    pub commit_id: CommitId,
    pub protected: bool,
    pub created_at: i64,
}

/// A pinned / aliased variable of the cheat table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheatEntry {
    pub id: i64,
    pub game_id: GameId,
    /// Save file the variable lives in.
    pub rel_path: String,
    pub alias: String,
    pub location: Location,
    /// Value re-applied on every save when `freeze_enabled`.
    pub frozen_value: Option<ScalarValue>,
    pub freeze_enabled: bool,
    pub created_at: i64,
}

/// An immutable pre-mutation shadow backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyBackup {
    pub id: i64,
    pub game_id: Option<GameId>,
    pub abs_path: PathBuf,
    pub blob: String,
    /// `safety/pre-mutation-<timestamp>`.
    pub tag: String,
    pub reason: String,
    pub created_at: i64,
    /// The stored blob was re-read and its hash verified.
    pub verified: bool,
    /// False when the file did not exist before the mutation.
    pub existed: bool,
}

/// A mutation performed through the safety protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationRecord {
    pub id: i64,
    pub safety: SafetyBackup,
    pub abs_path: PathBuf,
    /// blake3 hex of the written content (empty when the mutation deleted the file).
    pub new_blob: String,
    pub reason: String,
    pub created_at: i64,
    /// Already reverted by a panic restore.
    pub restored: bool,
    /// This mutation *is* a panic restore.
    pub is_panic: bool,
}

/// Storage statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreStats {
    /// Sum of the sizes of every file version referenced by commits and backups.
    pub logical_bytes: u64,
    /// Bytes actually used by compressed, deduplicated chunk files.
    pub physical_bytes: u64,
    pub blobs: u64,
    pub chunks: u64,
    pub commits: u64,
    pub safety_backups: u64,
}

impl StoreStats {
    /// logical / physical.
    pub fn dedup_ratio(&self) -> f64 {
        if self.physical_bytes == 0 {
            1.0
        } else {
            self.logical_bytes as f64 / self.physical_bytes as f64
        }
    }
}

/// Activity log entry (shared by the daemon, overlay and UIs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: i64,
    pub game_id: Option<GameId>,
    pub at: i64,
    pub kind: String,
    pub message: String,
}

/// Heartbeat of the background daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub pid: u32,
    pub heartbeat: i64,
    pub info: String,
}

/// Validates a branch name.
pub fn validate_branch_name(name: &str) -> Result<(), CasError> {
    let ok = !name.is_empty()
        && name.len() <= 100
        && !name.starts_with("safety/")
        && !name.contains("..")
        && !name.starts_with('/')
        && !name.ends_with('/')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'));
    if ok {
        Ok(())
    } else {
        Err(CasError::Invalid(format!(
            "invalid branch name `{name}` (use letters, digits, - _ . /; `safety/` is reserved)"
        )))
    }
}

/// Validates and normalises a relative save path (`/` separators, no `..`).
pub fn normalize_rel_path(rel: &str) -> Result<String, CasError> {
    let s = rel.replace('\\', "/");
    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    if parts.is_empty() || parts.contains(&"..") || s.starts_with('/') || s.contains(':') {
        return Err(CasError::Invalid(format!("`{rel}` is not a relative path inside the save root")));
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names() {
        assert!(validate_branch_name("main").is_ok());
        assert!(validate_branch_name("exp/no-death_run.2").is_ok());
        assert!(validate_branch_name("safety/x").is_err());
        assert!(validate_branch_name("a..b").is_err());
        assert!(validate_branch_name("").is_err());
        assert!(validate_branch_name("bad name").is_err());
    }

    #[test]
    fn rel_paths() {
        assert_eq!(normalize_rel_path("Saves\\slot1.sav").unwrap(), "Saves/slot1.sav");
        assert_eq!(normalize_rel_path("./a//b").unwrap(), "a/b");
        assert!(normalize_rel_path("../x").is_err());
        assert!(normalize_rel_path("/etc/passwd").is_err());
        assert!(normalize_rel_path("C:/x").is_err());
        assert!(normalize_rel_path("").is_err());
    }

    #[test]
    fn kinds_roundtrip() {
        for k in [
            CommitKind::Manual,
            CommitKind::Auto,
            CommitKind::Trainer,
            CommitKind::Edit,
            CommitKind::CherryPick,
            CommitKind::Restore,
            CommitKind::Import,
        ] {
            assert_eq!(k.as_str().parse::<CommitKind>().unwrap(), k);
        }
        assert!("x".parse::<CommitKind>().is_err());
    }
}
