//! Zero-corruption mutation protocol.
//!
//! Every write to a game's save directory goes through [`Store::safe_write`]:
//!
//! 1. **Shadow backup** — the current on-disk bytes are stored in the CAS,
//!    re-read and verified against their `blake3` hash, and registered under
//!    the protected tag `safety/pre-mutation-<timestamp>`. Any failure aborts
//!    the mutation before the game file is touched.
//! 2. **Atomic replace** — the new bytes are staged in `<store>/staging`
//!    (never in the game directory), fsynced, and moved over the target with
//!    `rename`. When the save directory is on another filesystem the target is
//!    overwritten in place (`write + fsync`).
//! 3. **Verification** — the target is read back and its hash compared with
//!    the intended bytes; on mismatch the original is restored immediately.
//!
//! [`Store::panic_restore`] reverts the most recent (non-panic, not yet
//! restored) mutation to its pre-mutation bytes, bit for bit, and verifies it.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::chunks::hash_hex;
use crate::error::{CasError, CasResult};
use crate::model::{now_ms, GameId, MutationRecord, SafetyBackup};
use crate::store::{row_safety, Store, SAFETY_COLS};

static STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How a file was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WriteMethod {
    /// Staged in the store and moved with `rename` (atomic).
    AtomicRename,
    /// Overwritten in place (staging dir on another filesystem).
    InPlace,
    /// The target was deleted.
    Deleted,
}

/// One row of the mutation journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationSummary {
    pub id: i64,
    pub path: PathBuf,
    pub reason: String,
    pub created_at: i64,
    pub restored: bool,
    pub is_panic: bool,
    pub safety_tag: String,
}

/// Which mutation a panic restore reverts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanicScope {
    /// The most recent mutation anywhere.
    Latest,
    /// The most recent mutation of a game.
    Game(GameId),
    /// The most recent mutation of one file.
    Path(PathBuf),
}

/// Result of a panic restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanicReport {
    pub path: PathBuf,
    /// The safety tag that was restored.
    pub restored_tag: String,
    /// blake3 of the restored content (equals the safety blob hash).
    pub restored_hash: String,
    /// The file did not exist before the reverted mutation and was removed.
    pub removed: bool,
    /// Safety tag capturing the state that was just replaced (undo of the panic).
    pub replaced_state_tag: String,
    pub method: WriteMethod,
}

fn is_cross_device(e: &std::io::Error) -> bool {
    match e.raw_os_error() {
        // EXDEV on Unix-likes.
        #[cfg(unix)]
        Some(18) => true,
        // ERROR_NOT_SAME_DEVICE on Windows.
        #[cfg(windows)]
        Some(17) => true,
        _ => false,
    }
}

fn safety_tag() -> String {
    let now = chrono::Utc::now();
    format!("safety/pre-mutation-{}", now.format("%Y%m%dT%H%M%S%.6fZ"))
}

impl Store {
    /// Writes `bytes` to `target` atomically (staging + rename) or in place
    /// when rename crosses filesystems. Never creates files next to `target`.
    pub fn atomic_replace(&self, target: &Path, bytes: &[u8]) -> CasResult<WriteMethod> {
        let name = target.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
        let n = STAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = self.staging.join(format!("stage-{}-{n}-{name}", std::process::id()));
        {
            let mut f = fs::File::create(&tmp).map_err(|e| CasError::io(&tmp, e))?;
            f.write_all(bytes).map_err(|e| CasError::io(&tmp, e))?;
            f.sync_all().map_err(|e| CasError::io(&tmp, e))?;
        }
        if let Ok(meta) = fs::metadata(target) {
            // Keep the original permissions (read-only saves stay read-only after
            // the move, which is what the game expects).
            if let Err(e) = fs::set_permissions(&tmp, meta.permissions()) {
                let _ = fs::remove_file(&tmp);
                return Err(CasError::io(&tmp, e));
            }
        }
        if let Some(parent) = target.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                fs::create_dir_all(parent).map_err(|e| CasError::io(parent, e))?;
            }
        }
        match fs::rename(&tmp, target) {
            Ok(()) => {
                #[cfg(unix)]
                if let Some(parent) = target.parent() {
                    if let Ok(d) = fs::File::open(parent) {
                        let _ = d.sync_all();
                    }
                }
                Ok(WriteMethod::AtomicRename)
            }
            Err(e) if is_cross_device(&e) => {
                let result = (|| -> std::io::Result<()> {
                    let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).open(target)?;
                    f.write_all(bytes)?;
                    f.sync_all()
                })();
                let _ = fs::remove_file(&tmp);
                result.map_err(|e| CasError::io(target, e))?;
                Ok(WriteMethod::InPlace)
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(CasError::io(target, e))
            }
        }
    }

    /// Captures and verifies the immutable pre-mutation shadow backup of `path`.
    pub fn capture_safety(&self, game: Option<GameId>, path: &Path, reason: &str) -> CasResult<SafetyBackup> {
        let (bytes, existed) = match fs::read(path) {
            Ok(b) => (b, true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), false),
            Err(e) => return Err(CasError::io(path, e)),
        };
        let expected = hash_hex(&bytes);
        let blob = self.put_blob(&bytes).map_err(|e| CasError::SafetyBackupFailed {
            path: path.to_path_buf(),
            reason: format!("cannot store backup: {e}"),
        })?;
        // Verify by reading the stored copy back from the chunk files.
        let stored = self.get_blob(&blob).map_err(|e| CasError::SafetyBackupFailed {
            path: path.to_path_buf(),
            reason: format!("cannot re-read backup: {e}"),
        })?;
        if blob != expected || hash_hex(&stored) != expected {
            return Err(CasError::SafetyBackupFailed {
                path: path.to_path_buf(),
                reason: "stored backup hash differs from the on-disk file".into(),
            });
        }
        // The file must not have changed while we were backing it up.
        let now_bytes = match fs::read(path) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(CasError::io(path, e)),
        };
        let unchanged = match (&now_bytes, existed) {
            (Some(b), true) => hash_hex(b) == expected,
            (None, false) => true,
            _ => false,
        };
        if !unchanged {
            return Err(CasError::SafetyBackupFailed {
                path: path.to_path_buf(),
                reason: "the file changed while it was being backed up (is the game writing it?)".into(),
            });
        }
        let base_tag = safety_tag();
        let created_at = now_ms();
        let g = self.inner.lock();
        let mut tag = base_tag.clone();
        let mut attempt = 1;
        loop {
            let res = g.conn.execute(
                "INSERT INTO safety_backups(game_id, abs_path, blob, tag, reason, created_at, verified, existed) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7)",
                params![game, path.to_string_lossy().to_string(), blob, tag, reason, created_at, existed as i64],
            );
            match res {
                Ok(_) => break,
                Err(rusqlite::Error::SqliteFailure(err, _))
                    if err.code == rusqlite::ErrorCode::ConstraintViolation && attempt < 1000 =>
                {
                    attempt += 1;
                    tag = format!("{base_tag}-{attempt}");
                }
                Err(e) => return Err(e.into()),
            }
        }
        let id = g.conn.last_insert_rowid();
        Ok(SafetyBackup {
            id,
            game_id: game,
            abs_path: path.to_path_buf(),
            blob,
            tag,
            reason: reason.to_string(),
            created_at,
            verified: true,
            existed,
        })
    }

    fn record_mutation(
        &self,
        safety: &SafetyBackup,
        path: &Path,
        new_blob: &str,
        reason: &str,
        is_panic: bool,
    ) -> CasResult<MutationRecord> {
        let created_at = now_ms();
        let g = self.inner.lock();
        g.conn.execute(
            "INSERT INTO mutations(safety_id, abs_path, new_blob, reason, created_at, restored, is_panic) VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
            params![safety.id, path.to_string_lossy().to_string(), new_blob, reason, created_at, is_panic as i64],
        )?;
        Ok(MutationRecord {
            id: g.conn.last_insert_rowid(),
            safety: safety.clone(),
            abs_path: path.to_path_buf(),
            new_blob: new_blob.to_string(),
            reason: reason.to_string(),
            created_at,
            restored: false,
            is_panic,
        })
    }

    fn write_verified(&self, path: &Path, bytes: &[u8], safety: &SafetyBackup) -> CasResult<WriteMethod> {
        let method = match self.atomic_replace(path, bytes) {
            Ok(m) => m,
            Err(e) => {
                // A failed in-place write may have truncated the file: restore.
                self.restore_from_backup(path, safety)?;
                return Err(e);
            }
        };
        let readback = fs::read(path).map_err(|e| CasError::io(path, e))?;
        if hash_hex(&readback) != hash_hex(bytes) {
            self.restore_from_backup(path, safety)?;
            return Err(CasError::WriteVerificationFailed {
                path: path.to_path_buf(),
                reason: "read-back hash differs from the written bytes".into(),
                tag: safety.tag.clone(),
            });
        }
        Ok(method)
    }

    fn restore_from_backup(&self, path: &Path, safety: &SafetyBackup) -> CasResult<WriteMethod> {
        if !safety.existed {
            return match fs::remove_file(path) {
                Ok(()) => Ok(WriteMethod::Deleted),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(WriteMethod::Deleted),
                Err(e) => Err(CasError::io(path, e)),
            };
        }
        let original = self.get_blob(&safety.blob)?;
        let m = self.atomic_replace(path, &original)?;
        let back = fs::read(path).map_err(|e| CasError::io(path, e))?;
        if hash_hex(&back) != safety.blob {
            return Err(CasError::Integrity(format!(
                "restoring {} from {} did not reproduce the original hash",
                path.display(),
                safety.tag
            )));
        }
        Ok(m)
    }

    /// Mutates a file in a game directory following the full safety protocol.
    pub fn safe_write(
        &self,
        game: Option<GameId>,
        path: &Path,
        bytes: &[u8],
        reason: &str,
    ) -> CasResult<MutationRecord> {
        let safety = self.capture_safety(game, path, reason)?;
        self.write_verified(path, bytes, &safety)?;
        let new_blob = self.put_blob(bytes)?;
        let rec = self.record_mutation(&safety, path, &new_blob, reason, false)?;
        self.log_event(game, "mutation", &format!("{reason}: {} (backup {})", path.display(), safety.tag))?;
        Ok(rec)
    }

    /// The mutation a panic restore would revert.
    pub fn last_mutation(&self, scope: &PanicScope) -> CasResult<Option<MutationRecord>> {
        let base = format!(
            "SELECT m.id, m.abs_path, m.new_blob, m.reason, m.created_at, m.restored, m.is_panic, {} FROM mutations m JOIN safety_backups s ON s.id = m.safety_id WHERE m.restored = 0 AND m.is_panic = 0",
            SAFETY_COLS.split(", ").map(|c| format!("s.{c}")).collect::<Vec<_>>().join(", ")
        );
        let g = self.inner.lock();
        let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<MutationRecord> {
            let safety = SafetyBackup {
                id: r.get(7)?,
                game_id: r.get(8)?,
                abs_path: PathBuf::from(r.get::<_, String>(9)?),
                blob: r.get(10)?,
                tag: r.get(11)?,
                reason: r.get(12)?,
                created_at: r.get(13)?,
                verified: r.get::<_, i64>(14)? != 0,
                existed: r.get::<_, i64>(15)? != 0,
            };
            Ok(MutationRecord {
                id: r.get(0)?,
                abs_path: PathBuf::from(r.get::<_, String>(1)?),
                new_blob: r.get(2)?,
                reason: r.get(3)?,
                created_at: r.get(4)?,
                restored: r.get::<_, i64>(5)? != 0,
                is_panic: r.get::<_, i64>(6)? != 0,
                safety,
            })
        };
        let rec = match scope {
            PanicScope::Latest => {
                g.conn.query_row(&format!("{base} ORDER BY m.id DESC LIMIT 1"), [], map).optional()?
            }
            PanicScope::Game(id) => g
                .conn
                .query_row(&format!("{base} AND s.game_id = ?1 ORDER BY m.id DESC LIMIT 1"), params![id], map)
                .optional()?,
            PanicScope::Path(p) => g
                .conn
                .query_row(
                    &format!("{base} AND m.abs_path = ?1 ORDER BY m.id DESC LIMIT 1"),
                    params![p.to_string_lossy().to_string()],
                    map,
                )
                .optional()?,
        };
        Ok(rec)
    }

    /// Reverts the most recent mutation in `scope` to its pre-mutation bytes.
    /// Repeated calls walk further back through the mutation history.
    pub fn panic_restore(&self, scope: &PanicScope) -> CasResult<PanicReport> {
        let m = self
            .last_mutation(scope)?
            .ok_or_else(|| CasError::NothingToRestore("no SaveLens mutation to revert".into()))?;
        let path = m.abs_path.clone();
        // Even a panic restore is a mutation: back up what is there now.
        let replaced = self.capture_safety(m.safety.game_id, &path, "panic-restore (state replaced)")?;
        let method = match self.restore_from_backup(&path, &m.safety) {
            Ok(method) => method,
            Err(e) => {
                // Put back what we just replaced, then report.
                let _ = self.restore_from_backup(&path, &replaced);
                return Err(e);
            }
        };
        let removed = method == WriteMethod::Deleted;
        let restored_hash = if removed {
            String::new()
        } else {
            let bytes = fs::read(&path).map_err(|e| CasError::io(&path, e))?;
            hash_hex(&bytes)
        };
        if !removed && restored_hash != m.safety.blob {
            return Err(CasError::Integrity(format!(
                "panic restore of {} produced hash {restored_hash}, expected {}",
                path.display(),
                m.safety.blob
            )));
        }
        {
            let g = self.inner.lock();
            g.conn.execute("UPDATE mutations SET restored = 1 WHERE id = ?1", params![m.id])?;
        }
        self.record_mutation(&replaced, &path, &m.safety.blob, "panic-restore", true)?;
        self.log_event(m.safety.game_id, "panic-restore", &format!("{} restored to {}", path.display(), m.safety.tag))?;
        Ok(PanicReport {
            path,
            restored_tag: m.safety.tag.clone(),
            restored_hash,
            removed,
            replaced_state_tag: replaced.tag,
            method,
        })
    }

    /// Safety backups, newest first.
    pub fn safety_backups(&self, game: Option<GameId>, limit: usize) -> CasResult<Vec<SafetyBackup>> {
        let g = self.inner.lock();
        let rows = match game {
            Some(id) => {
                let mut stmt = g.conn.prepare(&format!(
                    "SELECT {SAFETY_COLS} FROM safety_backups WHERE game_id = ?1 ORDER BY id DESC LIMIT ?2"
                ))?;
                let rows = stmt.query_map(params![id, limit as i64], row_safety)?.collect::<Result<Vec<_>, _>>()?;
                rows
            }
            None => {
                let mut stmt =
                    g.conn.prepare(&format!("SELECT {SAFETY_COLS} FROM safety_backups ORDER BY id DESC LIMIT ?1"))?;
                let rows = stmt.query_map(params![limit as i64], row_safety)?.collect::<Result<Vec<_>, _>>()?;
                rows
            }
        };
        Ok(rows)
    }

    /// Looks a safety backup up by tag.
    pub fn safety_by_tag(&self, tag: &str) -> CasResult<SafetyBackup> {
        let g = self.inner.lock();
        g.conn
            .query_row(&format!("SELECT {SAFETY_COLS} FROM safety_backups WHERE tag = ?1"), params![tag], row_safety)
            .optional()?
            .ok_or_else(|| CasError::NotFound { kind: "safety backup", id: tag.to_string() })
    }

    /// Restores a specific safety backup (itself protected by a new backup).
    pub fn restore_safety_tag(&self, tag: &str) -> CasResult<MutationRecord> {
        let s = self.safety_by_tag(tag)?;
        if !s.existed {
            return Err(CasError::Invalid(format!(
                "{tag} records that {} did not exist; use panic-restore to remove it",
                s.abs_path.display()
            )));
        }
        let bytes = self.get_blob(&s.blob)?;
        self.safe_write(s.game_id, &s.abs_path, &bytes, &format!("restore {tag}"))
    }

    /// Recent mutations, newest first.
    pub fn mutations(&self, limit: usize) -> CasResult<Vec<MutationSummary>> {
        let g = self.inner.lock();
        let mut stmt = g.conn.prepare(
            "SELECT m.id, m.abs_path, m.reason, m.created_at, m.restored, m.is_panic, s.tag FROM mutations m JOIN safety_backups s ON s.id = m.safety_id ORDER BY m.id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |r| {
                Ok(MutationSummary {
                    id: r.get(0)?,
                    path: PathBuf::from(r.get::<_, String>(1)?),
                    reason: r.get(2)?,
                    created_at: r.get(3)?,
                    restored: r.get::<_, i64>(4)? != 0,
                    is_panic: r.get::<_, i64>(5)? != 0,
                    safety_tag: r.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
