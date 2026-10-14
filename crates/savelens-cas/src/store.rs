//! The SaveLens store: SQLite metadata + content-addressed chunk files.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use savelens_core::{Location, ScalarValue};

use crate::chunks::{hash_hex, split, ChunkDir};
use crate::error::{CasError, CasResult};
use crate::model::*;

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS games (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    app_id INTEGER,
    save_root TEXT NOT NULL UNIQUE,
    patterns TEXT NOT NULL,
    runtime TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    current_branch TEXT NOT NULL DEFAULT 'main',
    watch_enabled INTEGER NOT NULL DEFAULT 1,
    trainer_enabled INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS chunks (
    hash TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    stored_size INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS blobs (
    hash TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    chunk_count INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS blob_chunks (
    blob TEXT NOT NULL REFERENCES blobs(hash) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    chunk TEXT NOT NULL REFERENCES chunks(hash),
    PRIMARY KEY (blob, seq)
);
CREATE INDEX IF NOT EXISTS blob_chunks_chunk ON blob_chunks(chunk);
CREATE TABLE IF NOT EXISTS commits (
    id INTEGER PRIMARY KEY,
    game_id INTEGER NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    parent_id INTEGER REFERENCES commits(id),
    branch TEXT NOT NULL,
    kind TEXT NOT NULL,
    message TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    summary TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS commits_game ON commits(game_id, id);
CREATE TABLE IF NOT EXISTS commit_files (
    commit_id INTEGER NOT NULL REFERENCES commits(id) ON DELETE CASCADE,
    rel_path TEXT NOT NULL,
    blob TEXT NOT NULL REFERENCES blobs(hash),
    size INTEGER NOT NULL,
    mtime INTEGER,
    PRIMARY KEY (commit_id, rel_path)
);
CREATE INDEX IF NOT EXISTS commit_files_blob ON commit_files(blob);
CREATE TABLE IF NOT EXISTS changelogs (
    commit_id INTEGER NOT NULL REFERENCES commits(id) ON DELETE CASCADE,
    rel_path TEXT NOT NULL,
    summary TEXT NOT NULL,
    lines TEXT NOT NULL,
    PRIMARY KEY (commit_id, rel_path)
);
CREATE TABLE IF NOT EXISTS branches (
    game_id INTEGER NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    head INTEGER REFERENCES commits(id),
    created_at INTEGER NOT NULL,
    PRIMARY KEY (game_id, name)
);
CREATE TABLE IF NOT EXISTS tags (
    game_id INTEGER NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    commit_id INTEGER NOT NULL REFERENCES commits(id),
    protected INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (game_id, name)
);
CREATE TABLE IF NOT EXISTS cheat_table (
    id INTEGER PRIMARY KEY,
    game_id INTEGER NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    rel_path TEXT NOT NULL,
    alias TEXT NOT NULL,
    location TEXT NOT NULL,
    frozen_value TEXT,
    freeze_enabled INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    UNIQUE (game_id, alias)
);
CREATE TABLE IF NOT EXISTS safety_backups (
    id INTEGER PRIMARY KEY,
    game_id INTEGER REFERENCES games(id) ON DELETE SET NULL,
    abs_path TEXT NOT NULL,
    blob TEXT NOT NULL REFERENCES blobs(hash),
    tag TEXT NOT NULL UNIQUE,
    reason TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    verified INTEGER NOT NULL,
    existed INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS mutations (
    id INTEGER PRIMARY KEY,
    safety_id INTEGER NOT NULL REFERENCES safety_backups(id),
    abs_path TEXT NOT NULL,
    new_blob TEXT NOT NULL,
    reason TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    restored INTEGER NOT NULL DEFAULT 0,
    is_panic INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY,
    game_id INTEGER,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    message TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS daemon_status (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    pid INTEGER NOT NULL,
    heartbeat INTEGER NOT NULL,
    info TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS watch_questions (
    id INTEGER PRIMARY KEY,
    game_id INTEGER NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    rel_path TEXT NOT NULL,
    location_key TEXT NOT NULL,
    location TEXT NOT NULL,
    old_value TEXT NOT NULL,
    new_value TEXT NOT NULL,
    guess TEXT,
    score INTEGER NOT NULL,
    flag INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL,
    category TEXT,
    alias TEXT,
    snooze_until INTEGER,
    times_seen INTEGER NOT NULL DEFAULT 1,
    commit_id INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (game_id, rel_path, location_key)
);
CREATE INDEX IF NOT EXISTS watch_questions_status ON watch_questions(status, id);
CREATE TABLE IF NOT EXISTS watch_mutes (
    id INTEGER PRIMARY KEY,
    game_id INTEGER NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    rel_path TEXT NOT NULL DEFAULT '',
    pattern TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (game_id, rel_path, pattern)
);
CREATE TABLE IF NOT EXISTS watch_volatility (
    game_id INTEGER NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    rel_path TEXT NOT NULL,
    location_key TEXT NOT NULL,
    stats TEXT NOT NULL,
    PRIMARY KEY (game_id, rel_path, location_key)
);
CREATE TABLE IF NOT EXISTS watch_saves (
    game_id INTEGER NOT NULL REFERENCES games(id) ON DELETE CASCADE,
    rel_path TEXT NOT NULL,
    saves INTEGER NOT NULL,
    PRIMARY KEY (game_id, rel_path)
);
CREATE TABLE IF NOT EXISTS requests (
    id INTEGER PRIMARY KEY,
    at INTEGER NOT NULL,
    action TEXT NOT NULL,
    payload TEXT NOT NULL,
    handled INTEGER NOT NULL DEFAULT 0
);
"#;

/// Resolves the data directory: `$SAVELENS_HOME`, else
/// `dirs::data_local_dir()/savelens`.
pub fn default_data_dir() -> CasResult<PathBuf> {
    if let Ok(home) = std::env::var("SAVELENS_HOME") {
        if !home.is_empty() {
            return Ok(PathBuf::from(home));
        }
    }
    dirs::data_local_dir()
        .map(|d| d.join("savelens"))
        .ok_or_else(|| CasError::Invalid("cannot determine the user data directory; set SAVELENS_HOME".into()))
}

pub(crate) struct Inner {
    pub(crate) conn: Connection,
}

/// Thread-safe handle to the store (cheap to clone).
#[derive(Clone)]
pub struct Store {
    pub(crate) root: PathBuf,
    pub(crate) chunks: ChunkDir,
    pub(crate) staging: PathBuf,
    pub(crate) inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("root", &self.root).finish()
    }
}

fn row_game(r: &rusqlite::Row<'_>) -> rusqlite::Result<GameRecord> {
    let patterns: String = r.get(4)?;
    Ok(GameRecord {
        id: r.get(0)?,
        name: r.get(1)?,
        app_id: r.get::<_, Option<i64>>(2)?.and_then(|v| u32::try_from(v).ok()),
        save_root: PathBuf::from(r.get::<_, String>(3)?),
        patterns: serde_json::from_str(&patterns).unwrap_or_default(),
        runtime: r.get(5)?,
        created_at: r.get(6)?,
        current_branch: r.get(7)?,
        watch_enabled: r.get::<_, i64>(8)? != 0,
        trainer_enabled: r.get::<_, i64>(9)? != 0,
    })
}

const GAME_COLS: &str =
    "id, name, app_id, save_root, patterns, runtime, created_at, current_branch, watch_enabled, trainer_enabled";

fn row_commit(r: &rusqlite::Row<'_>) -> rusqlite::Result<CommitRecord> {
    let kind: String = r.get(4)?;
    Ok(CommitRecord {
        id: r.get(0)?,
        game_id: r.get(1)?,
        parent: r.get(2)?,
        branch: r.get(3)?,
        kind: kind.parse().unwrap_or(CommitKind::Manual),
        message: r.get(5)?,
        created_at: r.get(6)?,
        summary: r.get(7)?,
    })
}

const COMMIT_COLS: &str = "id, game_id, parent_id, branch, kind, message, created_at, summary";

pub(crate) fn row_safety(r: &rusqlite::Row<'_>) -> rusqlite::Result<SafetyBackup> {
    Ok(SafetyBackup {
        id: r.get(0)?,
        game_id: r.get(1)?,
        abs_path: PathBuf::from(r.get::<_, String>(2)?),
        blob: r.get(3)?,
        tag: r.get(4)?,
        reason: r.get(5)?,
        created_at: r.get(6)?,
        verified: r.get::<_, i64>(7)? != 0,
        existed: r.get::<_, i64>(8)? != 0,
    })
}

pub(crate) const SAFETY_COLS: &str = "id, game_id, abs_path, blob, tag, reason, created_at, verified, existed";

fn row_cheat(r: &rusqlite::Row<'_>) -> rusqlite::Result<(CheatEntry, String, Option<String>)> {
    let loc: String = r.get(4)?;
    let frozen: Option<String> = r.get(5)?;
    Ok((
        CheatEntry {
            id: r.get(0)?,
            game_id: r.get(1)?,
            rel_path: r.get(2)?,
            alias: r.get(3)?,
            location: Location::Ast { path: savelens_core::NodePath::root() },
            frozen_value: None,
            freeze_enabled: r.get::<_, i64>(6)? != 0,
            created_at: r.get(7)?,
        },
        loc,
        frozen,
    ))
}

impl Store {
    /// Opens (creating if needed) a store rooted at `root`.
    pub fn open(root: impl AsRef<Path>) -> CasResult<Store> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root).map_err(|e| CasError::io(&root, e))?;
        let staging = root.join("staging");
        let chunks = ChunkDir::open(&root.join("chunks"), &staging)?;
        let db_path = root.join("savelens.db");
        let conn = Connection::open(&db_path)?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        let version: Option<String> =
            conn.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0)).optional()?;
        match version.as_deref().map(str::parse::<i64>) {
            None => {
                conn.execute(
                    "INSERT INTO meta(key, value) VALUES ('schema_version', ?1)",
                    params![SCHEMA_VERSION.to_string()],
                )?;
            }
            Some(Ok(v)) if v == SCHEMA_VERSION => {}
            Some(Ok(v)) => {
                return Err(CasError::Conflict(format!(
                    "store schema version {v} is newer than this build supports ({SCHEMA_VERSION})"
                )))
            }
            Some(Err(_)) => return Err(CasError::Integrity("corrupt schema_version in meta table".into())),
        }
        // Clean staging leftovers from crashed processes (they are ours, never game files).
        if let Ok(rd) = fs::read_dir(&staging) {
            for entry in rd.flatten() {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(Store { root, chunks, staging, inner: Arc::new(Mutex::new(Inner { conn })) })
    }

    /// Opens the store at [`default_data_dir`].
    pub fn open_default() -> CasResult<Store> {
        Store::open(default_data_dir()?)
    }

    /// Root directory of the store.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Staging directory used for atomic writes.
    pub fn staging_dir(&self) -> &Path {
        &self.staging
    }

    // ------------------------------------------------------------------
    // Blobs
    // ------------------------------------------------------------------

    fn put_blob_tx(&self, tx: &Transaction<'_>, data: &[u8]) -> CasResult<String> {
        let hash = hash_hex(data);
        let exists: bool =
            tx.query_row("SELECT 1 FROM blobs WHERE hash = ?1", params![hash], |_| Ok(())).optional()?.is_some();
        if exists {
            return Ok(hash);
        }
        let spans = split(data);
        for (seq, s) in spans.iter().enumerate() {
            let chunk = &data[s.offset..s.offset + s.length];
            let ch = hash_hex(chunk);
            let known: bool =
                tx.query_row("SELECT 1 FROM chunks WHERE hash = ?1", params![ch], |_| Ok(())).optional()?.is_some();
            if !known || !self.chunks.exists(&ch) {
                let stored = self.chunks.write(&ch, chunk)?;
                tx.execute(
                    "INSERT OR REPLACE INTO chunks(hash, size, stored_size) VALUES (?1, ?2, ?3)",
                    params![ch, chunk.len() as i64, stored as i64],
                )?;
            }
            tx.execute("INSERT INTO blob_chunks(blob, seq, chunk) VALUES (?1, ?2, ?3)", params![hash, seq as i64, ch])?;
        }
        // blob_chunks references blobs; insert the blob row first would violate
        // nothing either, but we insert it last so a partially written blob is
        // never visible outside the transaction.
        tx.execute(
            "INSERT INTO blobs(hash, size, chunk_count, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![hash, data.len() as i64, spans.len() as i64, now_ms()],
        )?;
        Ok(hash)
    }

    /// Stores bytes and returns their blake3 hex hash (deduplicated).
    pub fn put_blob(&self, data: &[u8]) -> CasResult<String> {
        let mut g = self.inner.lock();
        let tx = g.conn.transaction()?;
        tx.execute_batch("PRAGMA defer_foreign_keys = ON")?;
        let h = self.put_blob_tx(&tx, data)?;
        tx.commit()?;
        Ok(h)
    }

    /// Reads and verifies a blob.
    pub fn get_blob(&self, hash: &str) -> CasResult<Vec<u8>> {
        let (size, chunks): (i64, Vec<(String, i64)>) = {
            let g = self.inner.lock();
            let size: Option<i64> =
                g.conn.query_row("SELECT size FROM blobs WHERE hash = ?1", params![hash], |r| r.get(0)).optional()?;
            let size = size.ok_or_else(|| CasError::NotFound { kind: "blob", id: hash.to_string() })?;
            let mut stmt = g.conn.prepare(
                "SELECT bc.chunk, c.size FROM blob_chunks bc JOIN chunks c ON c.hash = bc.chunk WHERE bc.blob = ?1 ORDER BY bc.seq",
            )?;
            let rows = stmt
                .query_map(params![hash], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            (size, rows)
        };
        let mut out = Vec::with_capacity(size.max(0) as usize);
        for (ch, len) in chunks {
            out.extend_from_slice(&self.chunks.read(&ch, len.max(0) as usize)?);
        }
        if out.len() as i64 != size {
            return Err(CasError::Integrity(format!(
                "blob {hash} reassembled to {} bytes, expected {size}",
                out.len()
            )));
        }
        let actual = hash_hex(&out);
        if actual != hash {
            return Err(CasError::Integrity(format!("blob {hash} content hash is {actual}")));
        }
        Ok(out)
    }

    /// True when a blob is stored.
    pub fn has_blob(&self, hash: &str) -> CasResult<bool> {
        let g = self.inner.lock();
        Ok(g.conn.query_row("SELECT 1 FROM blobs WHERE hash = ?1", params![hash], |_| Ok(())).optional()?.is_some())
    }

    // ------------------------------------------------------------------
    // Games
    // ------------------------------------------------------------------

    /// Registers a game (or returns the existing one for the same save root).
    pub fn ensure_game(
        &self,
        name: &str,
        app_id: Option<u32>,
        save_root: &Path,
        patterns: &[String],
        runtime: &str,
    ) -> CasResult<GameRecord> {
        let root_s = save_root.to_string_lossy().to_string();
        let pats = serde_json::to_string(patterns)?;
        {
            let mut g = self.inner.lock();
            let tx = g.conn.transaction()?;
            let existing: Option<GameId> =
                tx.query_row("SELECT id FROM games WHERE save_root = ?1", params![root_s], |r| r.get(0)).optional()?;
            if existing.is_none() {
                let now = now_ms();
                tx.execute(
                    "INSERT INTO games(name, app_id, save_root, patterns, runtime, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![name, app_id.map(i64::from), root_s, pats, runtime, now],
                )?;
                let id = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO branches(game_id, name, head, created_at) VALUES (?1, 'main', NULL, ?2)",
                    params![id, now],
                )?;
            }
            tx.commit()?;
        }
        self.game_by_root(save_root)?.ok_or_else(|| CasError::NotFound { kind: "game", id: root_s })
    }

    /// Looks a game up by save root.
    pub fn game_by_root(&self, save_root: &Path) -> CasResult<Option<GameRecord>> {
        let g = self.inner.lock();
        Ok(g.conn
            .query_row(
                &format!("SELECT {GAME_COLS} FROM games WHERE save_root = ?1"),
                params![save_root.to_string_lossy().to_string()],
                row_game,
            )
            .optional()?)
    }

    /// Looks a game up by id.
    pub fn game(&self, id: GameId) -> CasResult<GameRecord> {
        let g = self.inner.lock();
        g.conn
            .query_row(&format!("SELECT {GAME_COLS} FROM games WHERE id = ?1"), params![id], row_game)
            .optional()?
            .ok_or_else(|| CasError::NotFound { kind: "game", id: id.to_string() })
    }

    /// Finds a game by id, exact name, app id or name substring (case-insensitive).
    pub fn find_game(&self, query: &str) -> CasResult<GameRecord> {
        let games = self.games()?;
        if let Ok(id) = query.parse::<i64>() {
            if let Some(g) = games.iter().find(|g| g.id == id || g.app_id.map(i64::from) == Some(id)) {
                return Ok(g.clone());
            }
        }
        let q = query.to_lowercase();
        if let Some(g) = games.iter().find(|g| g.name.to_lowercase() == q) {
            return Ok(g.clone());
        }
        let matches: Vec<&GameRecord> = games.iter().filter(|g| g.name.to_lowercase().contains(&q)).collect();
        match matches.len() {
            1 => Ok(matches[0].clone()),
            0 => Err(CasError::NotFound { kind: "game", id: query.to_string() }),
            _ => Err(CasError::Invalid(format!(
                "`{query}` matches several games: {}",
                matches.iter().map(|g| format!("{} (#{})", g.name, g.id)).collect::<Vec<_>>().join(", ")
            ))),
        }
    }

    /// All games.
    pub fn games(&self) -> CasResult<Vec<GameRecord>> {
        let g = self.inner.lock();
        let mut stmt = g.conn.prepare(&format!("SELECT {GAME_COLS} FROM games ORDER BY name"))?;
        let rows = stmt.query_map([], row_game)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Enables/disables watching.
    pub fn set_watch_enabled(&self, game: GameId, on: bool) -> CasResult<()> {
        self.update_game_flag(game, "watch_enabled", on)
    }

    /// Enables/disables the disk trainer (freeze-on-save).
    pub fn set_trainer_enabled(&self, game: GameId, on: bool) -> CasResult<()> {
        self.update_game_flag(game, "trainer_enabled", on)
    }

    fn update_game_flag(&self, game: GameId, col: &str, on: bool) -> CasResult<()> {
        let g = self.inner.lock();
        let n = g.conn.execute(&format!("UPDATE games SET {col} = ?1 WHERE id = ?2"), params![on as i64, game])?;
        if n == 0 {
            return Err(CasError::NotFound { kind: "game", id: game.to_string() });
        }
        Ok(())
    }

    /// Updates tracked patterns.
    pub fn set_patterns(&self, game: GameId, patterns: &[String]) -> CasResult<()> {
        let g = self.inner.lock();
        g.conn
            .execute("UPDATE games SET patterns = ?1 WHERE id = ?2", params![serde_json::to_string(patterns)?, game])?;
        Ok(())
    }

    /// Removes a game and its history (blobs are reclaimed by [`Store::gc`]).
    pub fn remove_game(&self, game: GameId) -> CasResult<()> {
        let mut g = self.inner.lock();
        let tx = g.conn.transaction()?;
        tx.execute("DELETE FROM tags WHERE game_id = ?1", params![game])?;
        tx.execute("DELETE FROM branches WHERE game_id = ?1", params![game])?;
        tx.execute("DELETE FROM cheat_table WHERE game_id = ?1", params![game])?;
        for table in ["watch_questions", "watch_mutes", "watch_volatility", "watch_saves"] {
            tx.execute(&format!("DELETE FROM {table} WHERE game_id = ?1"), params![game])?;
        }
        tx.execute(
            "DELETE FROM changelogs WHERE commit_id IN (SELECT id FROM commits WHERE game_id = ?1)",
            params![game],
        )?;
        tx.execute(
            "DELETE FROM commit_files WHERE commit_id IN (SELECT id FROM commits WHERE game_id = ?1)",
            params![game],
        )?;
        tx.execute("UPDATE commits SET parent_id = NULL WHERE game_id = ?1", params![game])?;
        tx.execute("DELETE FROM commits WHERE game_id = ?1", params![game])?;
        tx.execute("UPDATE safety_backups SET game_id = NULL WHERE game_id = ?1", params![game])?;
        tx.execute("DELETE FROM games WHERE id = ?1", params![game])?;
        tx.commit()?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Branches
    // ------------------------------------------------------------------

    /// Branches of a game.
    pub fn branches(&self, game: GameId) -> CasResult<Vec<BranchRecord>> {
        let g = self.inner.lock();
        let mut stmt =
            g.conn.prepare("SELECT name, head, created_at FROM branches WHERE game_id = ?1 ORDER BY name")?;
        let rows = stmt
            .query_map(params![game], |r| Ok(BranchRecord { name: r.get(0)?, head: r.get(1)?, created_at: r.get(2)? }))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Head commit of a branch.
    pub fn branch_head(&self, game: GameId, branch: &str) -> CasResult<Option<CommitId>> {
        let g = self.inner.lock();
        let head: Option<Option<CommitId>> = g
            .conn
            .query_row("SELECT head FROM branches WHERE game_id = ?1 AND name = ?2", params![game, branch], |r| {
                r.get(0)
            })
            .optional()?;
        head.ok_or_else(|| CasError::NotFound { kind: "branch", id: branch.to_string() })
    }

    /// Creates a branch at `from` (or at the current branch head).
    pub fn create_branch(&self, game: GameId, name: &str, from: Option<CommitId>) -> CasResult<BranchRecord> {
        validate_branch_name(name)?;
        let current = self.game(game)?.current_branch;
        let start = match from {
            Some(c) => {
                let commit = self.commit(c)?;
                if commit.game_id != game {
                    return Err(CasError::Invalid(format!("commit {c} belongs to another game")));
                }
                Some(c)
            }
            None => self.branch_head(game, &current)?,
        };
        let now = now_ms();
        let g = self.inner.lock();
        let n = g.conn.execute(
            "INSERT OR IGNORE INTO branches(game_id, name, head, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![game, name, start, now],
        )?;
        if n == 0 {
            return Err(CasError::Conflict(format!("branch `{name}` already exists")));
        }
        Ok(BranchRecord { name: name.to_string(), head: start, created_at: now })
    }

    /// Makes `name` the current branch (does not touch files on disk).
    pub fn set_current_branch(&self, game: GameId, name: &str) -> CasResult<()> {
        self.branch_head(game, name)?;
        let g = self.inner.lock();
        g.conn.execute("UPDATE games SET current_branch = ?1 WHERE id = ?2", params![name, game])?;
        Ok(())
    }

    /// Moves a branch head (used by checkout --reset).
    pub fn set_branch_head(&self, game: GameId, name: &str, head: Option<CommitId>) -> CasResult<()> {
        let g = self.inner.lock();
        let n = g
            .conn
            .execute("UPDATE branches SET head = ?1 WHERE game_id = ?2 AND name = ?3", params![head, game, name])?;
        if n == 0 {
            return Err(CasError::NotFound { kind: "branch", id: name.to_string() });
        }
        Ok(())
    }

    /// Deletes a branch (not the current one). Its commits stay reachable
    /// through other branches / tags or are reclaimed by `gc`.
    pub fn delete_branch(&self, game: GameId, name: &str) -> CasResult<()> {
        if self.game(game)?.current_branch == name {
            return Err(CasError::Conflict(format!("cannot delete the current branch `{name}`")));
        }
        let g = self.inner.lock();
        let n = g.conn.execute("DELETE FROM branches WHERE game_id = ?1 AND name = ?2", params![game, name])?;
        if n == 0 {
            return Err(CasError::NotFound { kind: "branch", id: name.to_string() });
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Commits
    // ------------------------------------------------------------------

    /// Files of a commit.
    pub fn commit_files(&self, commit: CommitId) -> CasResult<Vec<CommitFile>> {
        let g = self.inner.lock();
        let mut stmt = g
            .conn
            .prepare("SELECT rel_path, blob, size, mtime FROM commit_files WHERE commit_id = ?1 ORDER BY rel_path")?;
        let rows = stmt
            .query_map(params![commit], |r| {
                Ok(CommitFile {
                    rel_path: r.get(0)?,
                    blob: r.get(1)?,
                    size: r.get::<_, i64>(2)?.max(0) as u64,
                    mtime: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Content of one file in a commit.
    pub fn read_commit_file(&self, commit: CommitId, rel_path: &str) -> CasResult<Vec<u8>> {
        let rel = normalize_rel_path(rel_path)?;
        let blob: Option<String> = {
            let g = self.inner.lock();
            g.conn
                .query_row(
                    "SELECT blob FROM commit_files WHERE commit_id = ?1 AND rel_path = ?2",
                    params![commit, rel],
                    |r| r.get(0),
                )
                .optional()?
        };
        let blob = blob.ok_or_else(|| CasError::NotFound { kind: "file in commit", id: format!("{commit}:{rel}") })?;
        self.get_blob(&blob)
    }

    /// Looks a commit up.
    pub fn commit(&self, commit: CommitId) -> CasResult<CommitRecord> {
        let g = self.inner.lock();
        g.conn
            .query_row(&format!("SELECT {COMMIT_COLS} FROM commits WHERE id = ?1"), params![commit], row_commit)
            .optional()?
            .ok_or_else(|| CasError::NotFound { kind: "commit", id: commit.to_string() })
    }

    /// Records a snapshot. `changes` are applied on top of the current head of
    /// `branch`; files not mentioned are inherited. Returns `None` if the
    /// resulting file set is identical to the head.
    #[allow(clippy::too_many_arguments)]
    pub fn create_commit(
        &self,
        game: GameId,
        branch: &str,
        changes: &[FileChange],
        kind: CommitKind,
        message: &str,
        summary: &str,
        changelogs: &[Changelog],
    ) -> CasResult<Option<CommitRecord>> {
        let head = self.branch_head(game, branch)?;
        let mut files: BTreeMap<String, CommitFile> = match head {
            Some(h) => self.commit_files(h)?.into_iter().map(|f| (f.rel_path.clone(), f)).collect(),
            None => BTreeMap::new(),
        };
        let before = files.clone();
        let mut g = self.inner.lock();
        let tx = g.conn.transaction()?;
        tx.execute_batch("PRAGMA defer_foreign_keys = ON")?;
        for ch in changes {
            let rel = normalize_rel_path(&ch.rel_path)?;
            match &ch.content {
                Some(bytes) => {
                    let blob = self.put_blob_tx(&tx, bytes)?;
                    files.insert(
                        rel.clone(),
                        CommitFile { rel_path: rel, blob, size: bytes.len() as u64, mtime: ch.mtime },
                    );
                }
                None => {
                    files.remove(&rel);
                }
            }
        }
        let same =
            files.len() == before.len() && files.iter().all(|(k, f)| before.get(k).map(|b| &b.blob) == Some(&f.blob));
        if same && head.is_some() {
            // Nothing new: roll back blob inserts (they are content-addressed, harmless either way).
            tx.commit()?;
            return Ok(None);
        }
        let now = now_ms();
        tx.execute(
            "INSERT INTO commits(game_id, parent_id, branch, kind, message, created_at, summary) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![game, head, branch, kind.as_str(), message, now, summary],
        )?;
        let id = tx.last_insert_rowid();
        for f in files.values() {
            tx.execute(
                "INSERT INTO commit_files(commit_id, rel_path, blob, size, mtime) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, f.rel_path, f.blob, f.size as i64, f.mtime],
            )?;
        }
        for c in changelogs {
            tx.execute(
                "INSERT OR REPLACE INTO changelogs(commit_id, rel_path, summary, lines) VALUES (?1, ?2, ?3, ?4)",
                params![id, normalize_rel_path(&c.rel_path)?, c.summary, serde_json::to_string(&c.lines)?],
            )?;
        }
        let updated =
            tx.execute("UPDATE branches SET head = ?1 WHERE game_id = ?2 AND name = ?3", params![id, game, branch])?;
        if updated == 0 {
            return Err(CasError::NotFound { kind: "branch", id: branch.to_string() });
        }
        tx.commit()?;
        drop(g);
        self.commit(id).map(Some)
    }

    /// Commit history of a branch, newest first (follows parents).
    pub fn log(&self, game: GameId, branch: &str, limit: usize) -> CasResult<Vec<CommitRecord>> {
        let mut out = Vec::new();
        let mut cur = self.branch_head(game, branch)?;
        while let Some(id) = cur {
            if out.len() >= limit {
                break;
            }
            let c = self.commit(id)?;
            cur = c.parent;
            out.push(c);
        }
        Ok(out)
    }

    /// Every commit of a game (all branches), newest first.
    pub fn all_commits(&self, game: GameId) -> CasResult<Vec<CommitRecord>> {
        let g = self.inner.lock();
        let mut stmt =
            g.conn.prepare(&format!("SELECT {COMMIT_COLS} FROM commits WHERE game_id = ?1 ORDER BY id DESC"))?;
        let rows = stmt.query_map(params![game], row_commit)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Per-file semantic changelogs of a commit.
    pub fn changelogs(&self, commit: CommitId) -> CasResult<Vec<Changelog>> {
        let g = self.inner.lock();
        let mut stmt =
            g.conn.prepare("SELECT rel_path, summary, lines FROM changelogs WHERE commit_id = ?1 ORDER BY rel_path")?;
        let rows = stmt
            .query_map(params![commit], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(rel_path, summary, lines)| Ok(Changelog { rel_path, summary, lines: serde_json::from_str(&lines)? }))
            .collect()
    }

    /// Resolves `HEAD`, `HEAD~N`, `<branch>`, `<branch>~N`, a tag or a numeric id.
    pub fn resolve_rev(&self, game: GameId, rev: &str) -> CasResult<CommitId> {
        let (base, back) = match rev.rsplit_once('~') {
            Some((b, n)) => (b, n.parse::<usize>().map_err(|_| CasError::Invalid(format!("bad revision `{rev}`")))?),
            None => (rev, 0),
        };
        let current = self.game(game)?.current_branch;
        let start: CommitId = if base.eq_ignore_ascii_case("head") || base.is_empty() {
            self.branch_head(game, &current)?.ok_or_else(|| CasError::NotFound { kind: "commit", id: "HEAD".into() })?
        } else if let Ok(Some(h)) = self.branch_head(game, base) {
            h
        } else if let Some(t) = self.tags(game)?.into_iter().find(|t| t.name == base) {
            t.commit_id
        } else if let Ok(id) = base.trim_start_matches('#').parse::<i64>() {
            let c = self.commit(id)?;
            if c.game_id != game {
                return Err(CasError::Invalid(format!("commit {id} belongs to another game")));
            }
            id
        } else {
            return Err(CasError::NotFound { kind: "revision", id: rev.to_string() });
        };
        let mut cur = start;
        for _ in 0..back {
            cur =
                self.commit(cur)?.parent.ok_or_else(|| CasError::NotFound { kind: "revision", id: rev.to_string() })?;
        }
        Ok(cur)
    }

    // ------------------------------------------------------------------
    // Tags
    // ------------------------------------------------------------------

    /// Creates a tag. Names under `safety/` are reserved for the safety protocol.
    pub fn create_tag(&self, game: GameId, name: &str, commit: CommitId) -> CasResult<TagRecord> {
        if name.starts_with("safety/") || name.trim().is_empty() {
            return Err(CasError::Invalid(format!("tag name `{name}` is reserved or empty")));
        }
        let now = now_ms();
        let g = self.inner.lock();
        let n = g.conn.execute(
            "INSERT OR IGNORE INTO tags(game_id, name, commit_id, protected, created_at) VALUES (?1, ?2, ?3, 0, ?4)",
            params![game, name, commit, now],
        )?;
        if n == 0 {
            return Err(CasError::Conflict(format!("tag `{name}` already exists")));
        }
        Ok(TagRecord { name: name.to_string(), commit_id: commit, protected: false, created_at: now })
    }

    /// Tags of a game.
    pub fn tags(&self, game: GameId) -> CasResult<Vec<TagRecord>> {
        let g = self.inner.lock();
        let mut stmt = g.conn.prepare(
            "SELECT name, commit_id, protected, created_at FROM tags WHERE game_id = ?1 ORDER BY created_at",
        )?;
        let rows = stmt
            .query_map(params![game], |r| {
                Ok(TagRecord {
                    name: r.get(0)?,
                    commit_id: r.get(1)?,
                    protected: r.get::<_, i64>(2)? != 0,
                    created_at: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Deletes an unprotected tag.
    pub fn delete_tag(&self, game: GameId, name: &str) -> CasResult<()> {
        let g = self.inner.lock();
        let n = g
            .conn
            .execute("DELETE FROM tags WHERE game_id = ?1 AND name = ?2 AND protected = 0", params![game, name])?;
        if n == 0 {
            return Err(CasError::NotFound { kind: "unprotected tag", id: name.to_string() });
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Cheat table
    // ------------------------------------------------------------------

    fn decode_cheat(row: (CheatEntry, String, Option<String>)) -> CasResult<CheatEntry> {
        let (mut e, loc, frozen) = row;
        e.location = serde_json::from_str(&loc)?;
        e.frozen_value = frozen.map(|f| serde_json::from_str::<ScalarValue>(&f)).transpose()?;
        Ok(e)
    }

    /// Adds or replaces (same alias) a pinned variable.
    pub fn upsert_cheat(
        &self,
        game: GameId,
        rel_path: &str,
        alias: &str,
        location: &Location,
    ) -> CasResult<CheatEntry> {
        let rel = normalize_rel_path(rel_path)?;
        if alias.trim().is_empty() {
            return Err(CasError::Invalid("alias cannot be empty".into()));
        }
        let loc = serde_json::to_string(location)?;
        {
            let g = self.inner.lock();
            g.conn.execute(
                "INSERT INTO cheat_table(game_id, rel_path, alias, location, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(game_id, alias) DO UPDATE SET rel_path = excluded.rel_path, location = excluded.location",
                params![game, rel, alias.trim(), loc, now_ms()],
            )?;
        }
        self.cheat_by_alias(game, alias.trim())
    }

    /// Looks a pinned variable up by alias.
    pub fn cheat_by_alias(&self, game: GameId, alias: &str) -> CasResult<CheatEntry> {
        let row = {
            let g = self.inner.lock();
            g.conn
                .query_row(
                    "SELECT id, game_id, rel_path, alias, location, frozen_value, freeze_enabled, created_at FROM cheat_table WHERE game_id = ?1 AND alias = ?2",
                    params![game, alias],
                    row_cheat,
                )
                .optional()?
        };
        Self::decode_cheat(row.ok_or_else(|| CasError::NotFound { kind: "cheat entry", id: alias.to_string() })?)
    }

    /// Cheat table of a game.
    pub fn cheats(&self, game: GameId) -> CasResult<Vec<CheatEntry>> {
        let rows = {
            let g = self.inner.lock();
            let mut stmt = g.conn.prepare(
                "SELECT id, game_id, rel_path, alias, location, frozen_value, freeze_enabled, created_at FROM cheat_table WHERE game_id = ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map(params![game], row_cheat)?.collect::<Result<Vec<_>, _>>()?;
            rows
        };
        rows.into_iter().map(Self::decode_cheat).collect()
    }

    /// Sets / clears the frozen value of an entry.
    pub fn set_freeze(&self, game: GameId, alias: &str, value: Option<ScalarValue>, enabled: bool) -> CasResult<()> {
        let v = value.map(|v| serde_json::to_string(&v)).transpose()?;
        let g = self.inner.lock();
        let n = g.conn.execute(
            "UPDATE cheat_table SET frozen_value = ?1, freeze_enabled = ?2 WHERE game_id = ?3 AND alias = ?4",
            params![v, enabled as i64, game, alias],
        )?;
        if n == 0 {
            return Err(CasError::NotFound { kind: "cheat entry", id: alias.to_string() });
        }
        Ok(())
    }

    /// Toggles freezing without changing the stored value.
    pub fn toggle_freeze(&self, game: GameId, alias: &str, enabled: bool) -> CasResult<()> {
        let g = self.inner.lock();
        let n = g.conn.execute(
            "UPDATE cheat_table SET freeze_enabled = ?1 WHERE game_id = ?2 AND alias = ?3",
            params![enabled as i64, game, alias],
        )?;
        if n == 0 {
            return Err(CasError::NotFound { kind: "cheat entry", id: alias.to_string() });
        }
        Ok(())
    }

    /// Renames an alias.
    pub fn rename_cheat(&self, game: GameId, alias: &str, new_alias: &str) -> CasResult<()> {
        if new_alias.trim().is_empty() {
            return Err(CasError::Invalid("alias cannot be empty".into()));
        }
        let g = self.inner.lock();
        let n = g.conn.execute(
            "UPDATE cheat_table SET alias = ?1 WHERE game_id = ?2 AND alias = ?3",
            params![new_alias.trim(), game, alias],
        )?;
        if n == 0 {
            return Err(CasError::NotFound { kind: "cheat entry", id: alias.to_string() });
        }
        Ok(())
    }

    /// Removes an entry.
    pub fn delete_cheat(&self, game: GameId, alias: &str) -> CasResult<()> {
        let g = self.inner.lock();
        let n = g.conn.execute("DELETE FROM cheat_table WHERE game_id = ?1 AND alias = ?2", params![game, alias])?;
        if n == 0 {
            return Err(CasError::NotFound { kind: "cheat entry", id: alias.to_string() });
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Events, daemon status, requests (IPC through SQLite)
    // ------------------------------------------------------------------

    /// Appends an activity event.
    pub fn log_event(&self, game: Option<GameId>, kind: &str, message: &str) -> CasResult<()> {
        let g = self.inner.lock();
        g.conn.execute(
            "INSERT INTO events(game_id, at, kind, message) VALUES (?1, ?2, ?3, ?4)",
            params![game, now_ms(), kind, message],
        )?;
        // Keep the log bounded.
        g.conn.execute(
            "DELETE FROM events WHERE id <= (SELECT id FROM events ORDER BY id DESC LIMIT 1 OFFSET 5000)",
            [],
        )?;
        Ok(())
    }

    /// Most recent events (newest first), optionally after an id.
    pub fn events(&self, after_id: Option<i64>, limit: usize) -> CasResult<Vec<EventRecord>> {
        let g = self.inner.lock();
        let mut stmt = g
            .conn
            .prepare("SELECT id, game_id, at, kind, message FROM events WHERE id > ?1 ORDER BY id DESC LIMIT ?2")?;
        let rows = stmt
            .query_map(params![after_id.unwrap_or(0), limit as i64], |r| {
                Ok(EventRecord {
                    id: r.get(0)?,
                    game_id: r.get(1)?,
                    at: r.get(2)?,
                    kind: r.get(3)?,
                    message: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Records the daemon heartbeat.
    pub fn heartbeat(&self, info: &str) -> CasResult<()> {
        let g = self.inner.lock();
        g.conn.execute(
            "INSERT INTO daemon_status(id, pid, heartbeat, info) VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET pid = excluded.pid, heartbeat = excluded.heartbeat, info = excluded.info",
            params![std::process::id() as i64, now_ms(), info],
        )?;
        Ok(())
    }

    /// Clears the heartbeat (daemon stopped).
    pub fn clear_heartbeat(&self) -> CasResult<()> {
        let g = self.inner.lock();
        g.conn.execute("DELETE FROM daemon_status", [])?;
        Ok(())
    }

    /// Daemon status if a heartbeat is younger than `max_age_ms`.
    pub fn daemon_status(&self, max_age_ms: i64) -> CasResult<Option<DaemonStatus>> {
        let g = self.inner.lock();
        let st: Option<DaemonStatus> = g
            .conn
            .query_row("SELECT pid, heartbeat, info FROM daemon_status WHERE id = 1", [], |r| {
                Ok(DaemonStatus { pid: r.get::<_, i64>(0)? as u32, heartbeat: r.get(1)?, info: r.get(2)? })
            })
            .optional()?;
        Ok(st.filter(|s| now_ms() - s.heartbeat <= max_age_ms))
    }

    /// Queues a request for the daemon (e.g. from the overlay / hotkeys).
    pub fn push_request(&self, action: &str, payload: &serde_json::Value) -> CasResult<i64> {
        let g = self.inner.lock();
        g.conn.execute(
            "INSERT INTO requests(at, action, payload) VALUES (?1, ?2, ?3)",
            params![now_ms(), action, payload.to_string()],
        )?;
        Ok(g.conn.last_insert_rowid())
    }

    /// Takes the pending requests whose action is one of `actions` (oldest
    /// first, marked handled); requests for other consumers stay pending.
    pub fn take_requests_for(&self, actions: &[&str]) -> CasResult<Vec<(i64, String, serde_json::Value)>> {
        let mut g = self.inner.lock();
        let tx = g.conn.transaction()?;
        let rows: Vec<(i64, String, String)> = {
            let mut stmt = tx.prepare("SELECT id, action, payload FROM requests WHERE handled = 0 ORDER BY id")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
            rows
        };
        let mut out = Vec::new();
        for (id, action, payload) in rows {
            if actions.contains(&action.as_str()) {
                tx.execute("UPDATE requests SET handled = 1 WHERE id = ?1", params![id])?;
                out.push((id, action, serde_json::from_str(&payload)?));
            }
        }
        tx.execute("DELETE FROM requests WHERE handled = 1 AND at < ?1", params![now_ms() - 86_400_000])?;
        tx.commit()?;
        Ok(out)
    }

    /// Takes pending requests (marks them handled).
    pub fn take_requests(&self) -> CasResult<Vec<(i64, String, serde_json::Value)>> {
        let mut g = self.inner.lock();
        let tx = g.conn.transaction()?;
        let rows: Vec<(i64, String, String)> = {
            let mut stmt = tx.prepare("SELECT id, action, payload FROM requests WHERE handled = 0 ORDER BY id")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
            rows
        };
        tx.execute("UPDATE requests SET handled = 1 WHERE handled = 0", [])?;
        tx.execute("DELETE FROM requests WHERE handled = 1 AND at < ?1", params![now_ms() - 86_400_000])?;
        tx.commit()?;
        rows.into_iter().map(|(id, a, p)| Ok((id, a, serde_json::from_str(&p)?))).collect()
    }

    // ------------------------------------------------------------------
    // Stats & GC
    // ------------------------------------------------------------------

    /// Storage statistics.
    pub fn stats(&self) -> CasResult<StoreStats> {
        let g = self.inner.lock();
        let q = |sql: &str| -> CasResult<i64> {
            Ok(g.conn.query_row(sql, [], |r| r.get::<_, Option<i64>>(0))?.unwrap_or(0))
        };
        Ok(StoreStats {
            logical_bytes: (q("SELECT SUM(size) FROM commit_files")?
                + q("SELECT SUM(b.size) FROM safety_backups s JOIN blobs b ON b.hash = s.blob")?)
            .max(0) as u64,
            physical_bytes: q("SELECT SUM(stored_size) FROM chunks")?.max(0) as u64,
            blobs: q("SELECT COUNT(*) FROM blobs")?.max(0) as u64,
            chunks: q("SELECT COUNT(*) FROM chunks")?.max(0) as u64,
            commits: q("SELECT COUNT(*) FROM commits")?.max(0) as u64,
            safety_backups: q("SELECT COUNT(*) FROM safety_backups")?.max(0) as u64,
        })
    }

    /// Per-game logical size of the history.
    pub fn game_logical_bytes(&self, game: GameId) -> CasResult<u64> {
        let g = self.inner.lock();
        let v: Option<i64> = g.conn.query_row(
            "SELECT SUM(cf.size) FROM commit_files cf JOIN commits c ON c.id = cf.commit_id WHERE c.game_id = ?1",
            params![game],
            |r| r.get(0),
        )?;
        Ok(v.unwrap_or(0).max(0) as u64)
    }

    /// Deletes commits unreachable from any branch or tag, then blobs and
    /// chunks no longer referenced by commits, safety backups or mutations.
    /// Safety backups are never collected.
    pub fn gc(&self) -> CasResult<(u64, u64)> {
        let removed_chunks: Vec<String>;
        let removed_blobs: u64;
        {
            let mut g = self.inner.lock();
            let tx = g.conn.transaction()?;
            // Reachability from branch heads and tags.
            let roots: Vec<i64> = {
                let mut stmt =
                    tx.prepare("SELECT head FROM branches WHERE head IS NOT NULL UNION SELECT commit_id FROM tags")?;
                let rows = stmt.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
                rows
            };
            let parents: std::collections::HashMap<i64, Option<i64>> = {
                let mut stmt = tx.prepare("SELECT id, parent_id FROM commits")?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)))?
                    .collect::<Result<std::collections::HashMap<_, _>, _>>()?;
                rows
            };
            let mut reachable = std::collections::HashSet::new();
            for root in roots {
                let mut cur = Some(root);
                while let Some(c) = cur {
                    if !reachable.insert(c) {
                        break;
                    }
                    cur = parents.get(&c).copied().flatten();
                }
            }
            let dead_commits: Vec<i64> = parents.keys().filter(|c| !reachable.contains(c)).copied().collect();
            for c in &dead_commits {
                tx.execute("UPDATE commits SET parent_id = NULL WHERE parent_id = ?1", params![c])?;
            }
            for c in &dead_commits {
                tx.execute("DELETE FROM changelogs WHERE commit_id = ?1", params![c])?;
                tx.execute("DELETE FROM commit_files WHERE commit_id = ?1", params![c])?;
                tx.execute("DELETE FROM commits WHERE id = ?1", params![c])?;
            }
            let dead: Vec<String> = {
                let mut stmt = tx.prepare(
                    "SELECT hash FROM blobs WHERE hash NOT IN (SELECT blob FROM commit_files)
                       AND hash NOT IN (SELECT blob FROM safety_backups)
                       AND hash NOT IN (SELECT new_blob FROM mutations)",
                )?;
                let rows = stmt.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
                rows
            };
            for b in &dead {
                tx.execute("DELETE FROM blob_chunks WHERE blob = ?1", params![b])?;
                tx.execute("DELETE FROM blobs WHERE hash = ?1", params![b])?;
            }
            removed_blobs = dead.len() as u64;
            removed_chunks = {
                let mut stmt =
                    tx.prepare("SELECT hash FROM chunks WHERE hash NOT IN (SELECT chunk FROM blob_chunks)")?;
                let rows = stmt.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
                rows
            };
            for c in &removed_chunks {
                tx.execute("DELETE FROM chunks WHERE hash = ?1", params![c])?;
            }
            tx.commit()?;
        }
        for c in &removed_chunks {
            self.chunks.remove(c)?;
        }
        Ok((removed_blobs, removed_chunks.len() as u64))
    }

    /// Verifies every blob (returns the hashes that failed).
    pub fn verify_all(&self) -> CasResult<Vec<(String, String)>> {
        let blobs: Vec<String> = {
            let g = self.inner.lock();
            let mut stmt = g.conn.prepare("SELECT hash FROM blobs")?;
            let rows = stmt.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
            rows
        };
        let mut bad = Vec::new();
        for b in blobs {
            if let Err(e) = self.get_blob(&b) {
                bad.push((b, e.to_string()));
            }
        }
        Ok(bad)
    }
}
