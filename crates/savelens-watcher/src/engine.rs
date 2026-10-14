//! `Engine`: the single facade used by the CLI, TUI, GUI, overlay and daemon.
//!
//! It ties the codec pipeline, the content-addressable store and discovery
//! together, and is the only place that writes into game directories (always
//! through [`savelens_cas::Store::safe_write`]).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use savelens_cas::{
    Changelog, CheatEntry, CommitId, CommitKind, CommitRecord, FileChange, GameId, GameRecord, MutationRecord,
    PanicReport, PanicScope, Store,
};
use savelens_codecs::{DetectContext, EnvelopePipeline, OpenedSave};
use savelens_core::{
    cherry_pick, AliasResolver, CherryPickOptions, EditMode, Location, Node, NodePath, ScalarValue, SemanticDiff,
};
use serde::{Deserialize, Serialize};

use crate::config::SaveLensConfig;
use crate::error::{EngineError, EngineResult};

/// Revision name meaning "the files currently on disk".
pub const WORKING: &str = "WORKING";

/// A file matched by a game's tracking patterns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackedFile {
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub size: u64,
    pub mtime: Option<i64>,
}

/// Status of one file between two revisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileStatus {
    Added,
    Removed,
    Modified,
    Unchanged,
}

/// Semantic diff of one file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDiff {
    pub rel_path: String,
    pub status: FileStatus,
    pub summary: String,
    pub lines: Vec<String>,
    #[serde(skip)]
    pub semantic: Option<SemanticDiff>,
}

/// Result of a checkout / branch switch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckoutReport {
    pub target: CommitId,
    pub written: Vec<String>,
    pub unchanged: Vec<String>,
    pub safety_tags: Vec<String>,
    pub commit: Option<CommitId>,
}

/// What to edit.
#[derive(Debug, Clone)]
pub enum EditTarget {
    /// A node path and a textual value parsed with the node's type.
    Path { path: NodePath, value: String },
    /// A scanner location and a number.
    Location { location: Location, value: ScalarValue },
}

/// Result of an edit / cherry-pick / trainer injection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditReport {
    pub rel_path: String,
    pub mode: EditMode,
    pub changes: Vec<String>,
    pub safety_tag: Option<String>,
    pub commit: Option<CommitId>,
    /// Nothing had to be written (the file already had these values).
    pub no_op: bool,
}

/// A cheat-table entry with its current on-disk value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheatValue {
    pub entry: CheatEntry,
    pub current: Option<ScalarValue>,
    pub error: Option<String>,
}

/// Outcome of a freeze-on-save injection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainerReport {
    pub rel_path: String,
    pub applied: Vec<String>,
    pub elapsed_ms: f64,
    pub safety_tag: String,
    pub commit: Option<CommitId>,
}

/// Resolves aliases from the cheat table for one file.
struct CheatAliases {
    by_path: HashMap<String, String>,
}

impl AliasResolver for CheatAliases {
    fn alias(&self, path: &NodePath) -> Option<String> {
        self.by_path.get(&path.to_string()).cloned()
    }
}

/// The SaveLens engine.
#[derive(Clone)]
pub struct Engine {
    store: Store,
    pipeline: EnvelopePipeline,
    config: SaveLensConfig,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").field("store", &self.store).finish()
    }
}

fn mtime_ms(meta: &fs::Metadata) -> Option<i64> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
}

/// True for raw locations addressing the whole payload (captured in raw
/// in-place mode rather than inside a structured `Bytes` node).
pub fn is_payload_raw(location: &Location) -> bool {
    matches!(location, Location::Raw { region, .. } if region.is_root())
}

/// Files that are never tracked (editor/game temp files).
fn is_ignored_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".tmp")
        || lower.ends_with('~')
        || lower.ends_with(".swp")
        || lower.ends_with(".lock")
        || lower.ends_with(".log")
        || lower == "desktop.ini"
        || lower == ".ds_store"
        || lower == "thumbs.db"
}

impl Engine {
    /// Engine from parts.
    pub fn new(store: Store, pipeline: EnvelopePipeline, config: SaveLensConfig) -> Self {
        Engine { store, pipeline, config }
    }

    /// Opens the store in `root` with the default pipeline and `config.toml`.
    pub fn open_at(root: &Path) -> EngineResult<Engine> {
        let store = Store::open(root)?;
        let config = SaveLensConfig::load(store.root())?;
        let pipeline = EnvelopePipeline::with_defaults()?;
        Ok(Engine::new(store, pipeline, config))
    }

    /// Opens the default data directory (`$SAVELENS_HOME` or the user data dir).
    pub fn open_default() -> EngineResult<Engine> {
        Engine::open_at(&savelens_cas::default_data_dir()?)
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn pipeline(&self) -> &EnvelopePipeline {
        &self.pipeline
    }

    pub fn config(&self) -> &SaveLensConfig {
        &self.config
    }

    // ------------------------------------------------------------------
    // Games & discovery
    // ------------------------------------------------------------------

    /// Runs Steam / Proton / rules discovery.
    pub fn discover(&self, cfg: &savelens_discovery::DiscoveryConfig) -> EngineResult<savelens_discovery::DiscoveryReport> {
        savelens_discovery::discover(cfg).map_err(|e| EngineError::Discovery(e.to_string()))
    }

    /// Tracks one save location of a discovered game.
    pub fn track_discovered(
        &self,
        game: &savelens_discovery::DiscoveredGame,
        location: usize,
    ) -> EngineResult<(GameRecord, Option<CommitRecord>)> {
        let loc = game
            .save_locations
            .get(location)
            .ok_or_else(|| EngineError::NotFound(format!("save location #{location} of {}", game.name)))?;
        let runtime = match &game.runtime {
            savelens_discovery::GameRuntime::Native => "native".to_string(),
            savelens_discovery::GameRuntime::Proton { prefix } => format!("proton:{}", prefix.display()),
            savelens_discovery::GameRuntime::Unknown => "unknown".to_string(),
        };
        self.track(&game.name, game.app_id, &loc.root, &loc.patterns, &runtime)
    }

    /// Registers a game and imports the current state of its save files.
    pub fn track(
        &self,
        name: &str,
        app_id: Option<u32>,
        save_root: &Path,
        patterns: &[String],
        runtime: &str,
    ) -> EngineResult<(GameRecord, Option<CommitRecord>)> {
        if !save_root.is_dir() {
            return Err(EngineError::NotFound(format!("save directory {} does not exist", save_root.display())));
        }
        let root = save_root.canonicalize().map_err(|e| EngineError::io(save_root, e))?;
        let game = self.store.ensure_game(name, app_id, &root, patterns, runtime)?;
        let commit = self.snapshot(game.id, "import current saves", CommitKind::Import)?;
        self.store.log_event(Some(game.id), "track", &format!("tracking {} at {}", game.name, root.display()))?;
        Ok((game, commit))
    }

    /// Game by id.
    pub fn game(&self, id: GameId) -> EngineResult<GameRecord> {
        Ok(self.store.game(id)?)
    }

    /// Detection context for a file of a game.
    pub fn ctx_for(&self, game: &GameRecord, rel: &str) -> DetectContext {
        DetectContext {
            file_name: Path::new(rel).file_name().map(|n| n.to_string_lossy().to_string()),
            app_id: game.app_id,
        }
    }

    /// Absolute path of a tracked file (validated to stay inside the save root).
    pub fn abs_path(&self, game: &GameRecord, rel: &str) -> EngineResult<PathBuf> {
        let rel = savelens_cas::normalize_rel_path(rel)?;
        Ok(game.save_root.join(rel))
    }

    /// Files currently matching the game's patterns.
    pub fn tracked_files(&self, game: &GameRecord) -> EngineResult<Vec<TrackedFile>> {
        let patterns: Vec<String> = if game.patterns.is_empty() {
            vec!["**/*".to_string()]
        } else {
            game.patterns.clone()
        };
        let mut out: BTreeMap<String, TrackedFile> = BTreeMap::new();
        let max = self.config.max_file_size();
        for pat in patterns {
            let full = game.save_root.join(&pat);
            let pattern = full.to_string_lossy().to_string();
            let entries = glob::glob(&pattern).map_err(|e| EngineError::Invalid(format!("bad pattern `{pat}`: {e}")))?;
            for entry in entries.flatten() {
                let Ok(meta) = fs::metadata(&entry) else { continue };
                if !meta.is_file() || meta.len() > max {
                    continue;
                }
                let Ok(rel) = entry.strip_prefix(&game.save_root) else { continue };
                let rel_s = rel.to_string_lossy().replace('\\', "/");
                let name = entry.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                if is_ignored_name(&name) {
                    continue;
                }
                out.insert(
                    rel_s.clone(),
                    TrackedFile {
                        rel_path: rel_s,
                        abs_path: entry.clone(),
                        size: meta.len(),
                        mtime: mtime_ms(&meta),
                    },
                );
            }
        }
        Ok(out.into_values().collect())
    }

    /// True if `path` belongs to the game (inside its root and matching its patterns).
    pub fn rel_path_for(&self, game: &GameRecord, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(&game.save_root).ok()?;
        let rel_s = rel.to_string_lossy().replace('\\', "/");
        let name = path.file_name()?.to_string_lossy().to_string();
        if is_ignored_name(&name) || rel_s.is_empty() {
            return None;
        }
        if game.patterns.is_empty() {
            return Some(rel_s);
        }
        let opts = glob::MatchOptions {
            case_sensitive: !cfg!(windows),
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };
        game.patterns
            .iter()
            .any(|p| glob::Pattern::new(p).map(|pat| pat.matches_with(&rel_s, opts)).unwrap_or(false))
            .then_some(rel_s)
    }

    // ------------------------------------------------------------------
    // Snapshots & history
    // ------------------------------------------------------------------

    fn aliases_for(&self, game: GameId, rel: &str) -> CheatAliases {
        let mut by_path = HashMap::new();
        if let Ok(cheats) = self.store.cheats(game) {
            for c in cheats.into_iter().filter(|c| c.rel_path == rel) {
                if let Location::Ast { path } = &c.location {
                    by_path.insert(path.to_string(), c.alias.clone());
                }
            }
        }
        CheatAliases { by_path }
    }

    /// Semantic diff of two versions of a file.
    pub fn semantic_diff(&self, game: &GameRecord, rel: &str, old: &[u8], new: &[u8]) -> (Option<SemanticDiff>, String, Vec<String>) {
        let ctx = self.ctx_for(game, rel);
        let aliases = self.aliases_for(game.id, rel);
        let (a, b) = match (self.pipeline.open(old, &ctx), self.pipeline.open(new, &ctx)) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => return (None, format!("{} bytes changed (unreadable: {e})", new.len()), Vec::new()),
        };
        let (ra, rb) = if a.codec_id() == b.codec_id() {
            (a.root(), b.root())
        } else {
            // Format changed between versions: compare raw payloads.
            (a.into_raw_mode().root(), b.into_raw_mode().root())
        };
        let d = savelens_core::diff(&ra, &rb);
        let summary = d.summary(&aliases, 4);
        let mut lines = d.changelog(&aliases);
        let max = self.config.max_changelog_lines;
        if lines.len() > max {
            let extra = lines.len() - max;
            lines.truncate(max);
            lines.push(format!("… {extra} more change(s)"));
        }
        (Some(d), summary, lines)
    }

    fn changelog(&self, game: &GameRecord, rel: &str, old: Option<&[u8]>, new: &[u8]) -> Changelog {
        match old {
            None => {
                let codec = self
                    .pipeline
                    .open(new, &self.ctx_for(game, rel))
                    .map(|o| o.codec_id().to_string())
                    .unwrap_or_else(|_| "raw".into());
                Changelog {
                    rel_path: rel.to_string(),
                    summary: format!("new file ({codec}, {} bytes)", new.len()),
                    lines: Vec::new(),
                }
            }
            Some(old) => {
                let (_, summary, lines) = self.semantic_diff(game, rel, old, new);
                Changelog {
                    rel_path: rel.to_string(),
                    summary,
                    lines,
                }
            }
        }
    }

    fn head_bytes(&self, game: &GameRecord, rel: &str) -> EngineResult<Option<Vec<u8>>> {
        let Some(head) = self.store.branch_head(game.id, &game.current_branch)? else {
            return Ok(None);
        };
        match self.store.read_commit_file(head, rel) {
            Ok(b) => Ok(Some(b)),
            Err(savelens_cas::CasError::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn commit_changes(
        &self,
        game: &GameRecord,
        changes: Vec<(String, Vec<u8>, Option<i64>)>,
        kind: CommitKind,
        message: &str,
    ) -> EngineResult<Option<CommitRecord>> {
        let mut file_changes = Vec::new();
        let mut logs = Vec::new();
        let mut summaries = Vec::new();
        for (rel, bytes, mtime) in changes {
            let old = self.head_bytes(game, &rel)?;
            if old.as_deref() == Some(bytes.as_slice()) {
                continue;
            }
            let log = self.changelog(game, &rel, old.as_deref(), &bytes);
            summaries.push(if log.summary.is_empty() { rel.clone() } else { log.summary.clone() });
            logs.push(log);
            file_changes.push(FileChange {
                rel_path: rel,
                content: Some(bytes),
                mtime,
            });
        }
        if file_changes.is_empty() {
            return Ok(None);
        }
        let summary = summaries.join("; ");
        let commit = self
            .store
            .create_commit(game.id, &game.current_branch, &file_changes, kind, message, &summary, &logs)?;
        if let Some(c) = &commit {
            self.store
                .log_event(Some(game.id), "commit", &format!("#{} [{}] {}: {}", c.id, kind, message, summary))?;
        }
        Ok(commit)
    }

    /// Snapshots every tracked file of a game.
    pub fn snapshot(&self, game_id: GameId, message: &str, kind: CommitKind) -> EngineResult<Option<CommitRecord>> {
        let game = self.store.game(game_id)?;
        let mut changes = Vec::new();
        for f in self.tracked_files(&game)? {
            let bytes = fs::read(&f.abs_path).map_err(|e| EngineError::io(&f.abs_path, e))?;
            changes.push((f.rel_path, bytes, f.mtime));
        }
        self.commit_changes(&game, changes, kind, message)
    }

    /// Commits one file's bytes (used by the watcher).
    pub fn commit_file(&self, game_id: GameId, rel: &str, bytes: Vec<u8>, kind: CommitKind, message: &str) -> EngineResult<Option<CommitRecord>> {
        let game = self.store.game(game_id)?;
        let rel = savelens_cas::normalize_rel_path(rel)?;
        self.commit_changes(&game, vec![(rel, bytes, None)], kind, message)
    }

    /// History of a branch (current branch by default).
    pub fn log(&self, game_id: GameId, branch: Option<&str>, limit: usize) -> EngineResult<Vec<CommitRecord>> {
        let game = self.store.game(game_id)?;
        Ok(self.store.log(game_id, branch.unwrap_or(&game.current_branch), limit)?)
    }

    /// Reads one file at a revision (`WORKING` = disk).
    pub fn read_version(&self, game: &GameRecord, rev: &str, rel: &str) -> EngineResult<Option<Vec<u8>>> {
        if rev.eq_ignore_ascii_case(WORKING) {
            let p = self.abs_path(game, rel)?;
            return match fs::read(&p) {
                Ok(b) => Ok(Some(b)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(EngineError::io(p, e)),
            };
        }
        let id = self.store.resolve_rev(game.id, rev)?;
        match self.store.read_commit_file(id, rel) {
            Ok(b) => Ok(Some(b)),
            Err(savelens_cas::CasError::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Files present at a revision.
    pub fn files_at(&self, game: &GameRecord, rev: &str) -> EngineResult<Vec<String>> {
        if rev.eq_ignore_ascii_case(WORKING) {
            return Ok(self.tracked_files(game)?.into_iter().map(|f| f.rel_path).collect());
        }
        let id = self.store.resolve_rev(game.id, rev)?;
        Ok(self.store.commit_files(id)?.into_iter().map(|f| f.rel_path).collect())
    }

    /// Opens a file version through the codec pipeline.
    pub fn open_version(&self, game_id: GameId, rev: &str, rel: &str) -> EngineResult<OpenedSave> {
        let game = self.store.game(game_id)?;
        let bytes = self
            .read_version(&game, rev, rel)?
            .ok_or_else(|| EngineError::NotFound(format!("{rel} at {rev}")))?;
        Ok(self.pipeline.open(&bytes, &self.ctx_for(&game, rel))?)
    }

    /// Semantic tree of a file version (for scans and the AST explorer).
    pub fn root_at(&self, game_id: GameId, rev: &str, rel: &str) -> EngineResult<Node> {
        Ok(self.open_version(game_id, rev, rel)?.root())
    }

    /// Semantic diff between two revisions (either may be `WORKING`).
    pub fn diff(&self, game_id: GameId, from: &str, to: &str, only: Option<&str>) -> EngineResult<Vec<FileDiff>> {
        let game = self.store.game(game_id)?;
        let mut names: Vec<String> = self.files_at(&game, from)?;
        for n in self.files_at(&game, to)? {
            if !names.contains(&n) {
                names.push(n);
            }
        }
        names.sort();
        if let Some(o) = only {
            let o = savelens_cas::normalize_rel_path(o)?;
            names.retain(|n| *n == o);
        }
        let mut out = Vec::new();
        for rel in names {
            let a = self.read_version(&game, from, &rel)?;
            let b = self.read_version(&game, to, &rel)?;
            let fd = match (a, b) {
                (None, Some(b)) => FileDiff {
                    rel_path: rel,
                    status: FileStatus::Added,
                    summary: format!("added ({} bytes)", b.len()),
                    lines: Vec::new(),
                    semantic: None,
                },
                (Some(a), None) => FileDiff {
                    rel_path: rel,
                    status: FileStatus::Removed,
                    summary: format!("removed ({} bytes)", a.len()),
                    lines: Vec::new(),
                    semantic: None,
                },
                (Some(a), Some(b)) if a == b => FileDiff {
                    rel_path: rel,
                    status: FileStatus::Unchanged,
                    summary: "unchanged".into(),
                    lines: Vec::new(),
                    semantic: None,
                },
                (Some(a), Some(b)) => {
                    let (semantic, summary, lines) = self.semantic_diff(&game, &rel, &a, &b);
                    FileDiff {
                        rel_path: rel,
                        status: FileStatus::Modified,
                        summary,
                        lines,
                        semantic,
                    }
                }
                (None, None) => continue,
            };
            out.push(fd);
        }
        Ok(out)
    }

    // ------------------------------------------------------------------
    // Mutations (all go through Store::safe_write)
    // ------------------------------------------------------------------

    fn ensure_not_busy(&self, path: &Path) -> EngineResult<()> {
        if path.exists() && savelens_discovery::process::is_locked_for_write(path) {
            return Err(EngineError::Busy(path.to_path_buf()));
        }
        Ok(())
    }

    /// Atomic hot-swap of a tracked file (safety backup + staged rename + verify).
    pub fn hot_swap(&self, game: &GameRecord, rel: &str, bytes: &[u8], reason: &str) -> EngineResult<MutationRecord> {
        let path = self.abs_path(game, rel)?;
        self.ensure_not_busy(&path)?;
        Ok(self.store.safe_write(Some(game.id), &path, bytes, reason)?)
    }

    /// Restores files of a revision onto disk and records a `restore` commit.
    pub fn checkout(&self, game_id: GameId, rev: &str, files: Option<&[String]>) -> EngineResult<CheckoutReport> {
        let game = self.store.game(game_id)?;
        let target = self.store.resolve_rev(game.id, rev)?;
        let mut written = Vec::new();
        let mut unchanged = Vec::new();
        let mut tags = Vec::new();
        let mut changes = Vec::new();
        for f in self.store.commit_files(target)? {
            if let Some(sel) = files {
                if !sel.iter().any(|s| savelens_cas::normalize_rel_path(s).ok().as_deref() == Some(f.rel_path.as_str())) {
                    continue;
                }
            }
            let want = self.store.get_blob(&f.blob)?;
            let path = self.abs_path(&game, &f.rel_path)?;
            let current = fs::read(&path).ok();
            if current.as_deref() == Some(want.as_slice()) {
                unchanged.push(f.rel_path);
                continue;
            }
            let m = self.hot_swap(&game, &f.rel_path, &want, &format!("checkout {rev}"))?;
            tags.push(m.safety.tag);
            written.push(f.rel_path.clone());
            changes.push((f.rel_path, want, None));
        }
        let commit = self
            .commit_changes(&game, changes, CommitKind::Restore, &format!("restored {rev} (#{target})"))?
            .map(|c| c.id);
        Ok(CheckoutReport {
            target,
            written,
            unchanged,
            safety_tags: tags,
            commit,
        })
    }

    /// Creates a branch (from a revision or the current head).
    pub fn create_branch(&self, game_id: GameId, name: &str, from: Option<&str>) -> EngineResult<savelens_cas::BranchRecord> {
        let from_id = match from {
            Some(r) => Some(self.store.resolve_rev(game_id, r)?),
            None => None,
        };
        Ok(self.store.create_branch(game_id, name, from_id)?)
    }

    /// Switches branch: the branch head's files are written to disk.
    pub fn switch_branch(&self, game_id: GameId, name: &str) -> EngineResult<CheckoutReport> {
        let game = self.store.game(game_id)?;
        let head = self
            .store
            .branch_head(game.id, name)?
            .ok_or_else(|| EngineError::Invalid(format!("branch `{name}` has no commits yet")))?;
        let mut written = Vec::new();
        let mut unchanged = Vec::new();
        let mut tags = Vec::new();
        for f in self.store.commit_files(head)? {
            let want = self.store.get_blob(&f.blob)?;
            let path = self.abs_path(&game, &f.rel_path)?;
            if fs::read(&path).ok().as_deref() == Some(want.as_slice()) {
                unchanged.push(f.rel_path);
                continue;
            }
            let m = self.hot_swap(&game, &f.rel_path, &want, &format!("switch to branch {name}"))?;
            tags.push(m.safety.tag);
            written.push(f.rel_path);
        }
        self.store.set_current_branch(game.id, name)?;
        self.store
            .log_event(Some(game.id), "branch", &format!("switched to `{name}` ({} file(s) written)", written.len()))?;
        Ok(CheckoutReport {
            target: head,
            written,
            unchanged,
            safety_tags: tags,
            commit: None,
        })
    }

    fn open_for_edit(&self, game: &GameRecord, rel: &str, allow_raw: bool, force_raw: bool) -> EngineResult<(PathBuf, Vec<u8>, DetectContext, OpenedSave)> {
        let path = self.abs_path(game, rel)?;
        let bytes = fs::read(&path).map_err(|e| EngineError::io(&path, e))?;
        let ctx = self.ctx_for(game, rel);
        let opened = self.pipeline.open(&bytes, &ctx)?;
        if force_raw {
            return Ok((path, bytes, ctx, opened.into_raw_mode()));
        }
        let opened = match opened.edit_mode() {
            EditMode::Structural => opened,
            EditMode::RawInPlace if allow_raw => opened.into_raw_mode(),
            EditMode::RawInPlace => {
                return Err(EngineError::GateBlocked(format!(
                    "{rel}: the {} codec does not reproduce the file bit for bit; enable raw in-place mode to edit offsets",
                    opened.codec_id()
                )))
            }
            EditMode::BackupRestoreOnly => {
                return Err(EngineError::GateBlocked(format!(
                    "{rel}: round-trip failed ({:?}); only full backup/restore is allowed",
                    opened.envelope().round_trip
                )))
            }
        };
        Ok((path, bytes, ctx, opened))
    }

    fn finish_edit(
        &self,
        game: &GameRecord,
        rel: &str,
        path: &Path,
        original: &[u8],
        ctx: &DetectContext,
        opened: &OpenedSave,
        kind: CommitKind,
        reason: &str,
    ) -> EngineResult<EditReport> {
        let mode = opened.edit_mode();
        let new = opened.encode_verified(&self.pipeline, ctx)?;
        if new == original {
            return Ok(EditReport {
                rel_path: rel.to_string(),
                mode,
                changes: Vec::new(),
                safety_tag: None,
                commit: None,
                no_op: true,
            });
        }
        let (_, _, changes) = self.semantic_diff(game, rel, original, &new);
        self.ensure_not_busy(path)?;
        let m = self.store.safe_write(Some(game.id), path, &new, reason)?;
        let commit = self.commit_changes(game, vec![(rel.to_string(), new, None)], kind, reason)?;
        Ok(EditReport {
            rel_path: rel.to_string(),
            mode,
            changes,
            safety_tag: Some(m.safety.tag),
            commit: commit.map(|c| c.id),
            no_op: false,
        })
    }

    /// Edits one value on disk (re-encrypting / re-compressing / re-signing).
    pub fn edit_value(&self, game_id: GameId, rel: &str, target: &EditTarget, allow_raw: bool) -> EngineResult<EditReport> {
        let game = self.store.game(game_id)?;
        let rel = savelens_cas::normalize_rel_path(rel)?;
        let force_raw = matches!(target, EditTarget::Location { location, .. } if is_payload_raw(location));
        let (path, original, ctx, mut opened) = self.open_for_edit(&game, &rel, allow_raw, force_raw)?;
        let reason = match target {
            EditTarget::Path { path: p, value } => {
                let root = opened.root();
                let current = root.resolve(p)?;
                let new = current.parse_like(value, p)?;
                opened.set(p, new)?;
                format!("edit {p} = {value}")
            }
            EditTarget::Location { location, value } => {
                opened.write_location(location, *value)?;
                format!("edit {location} = {value}")
            }
        };
        self.finish_edit(&game, &rel, &path, &original, &ctx, &opened, CommitKind::Edit, &reason)
    }

    /// Replaces the whole semantic tree of a file (GUI tree editor).
    pub fn apply_root(&self, game_id: GameId, rel: &str, root: &Node, reason: &str) -> EngineResult<EditReport> {
        let game = self.store.game(game_id)?;
        let rel = savelens_cas::normalize_rel_path(rel)?;
        let raw = matches!(root, Node::RawRegion(_));
        let (path, original, ctx, mut opened) = self.open_for_edit(&game, &rel, raw, raw)?;
        opened.apply_ast(root)?;
        self.finish_edit(&game, &rel, &path, &original, &ctx, &opened, CommitKind::Edit, reason)
    }

    /// Surgical cherry-pick of sub-trees from an older revision into the file on disk.
    pub fn cherry_pick(&self, game_id: GameId, from_rev: &str, rel: &str, paths: &[NodePath], opts: &CherryPickOptions) -> EngineResult<EditReport> {
        let game = self.store.game(game_id)?;
        let rel = savelens_cas::normalize_rel_path(rel)?;
        let source = self.root_at(game_id, from_rev, &rel)?;
        let (path, original, ctx, mut opened) = self.open_for_edit(&game, &rel, false, false)?;
        let mut root = opened.root();
        let report = cherry_pick(&mut root, &source, paths, opts)?;
        if report.applied.is_empty() {
            return Ok(EditReport {
                rel_path: rel,
                mode: opened.edit_mode(),
                changes: Vec::new(),
                safety_tag: None,
                commit: None,
                no_op: true,
            });
        }
        opened.apply_ast(&root)?;
        let names: Vec<String> = report.applied.iter().map(|p| p.to_string()).collect();
        let reason = format!("cherry-pick {} from {from_rev}", names.join(", "));
        self.finish_edit(&game, &rel, &path, &original, &ctx, &opened, CommitKind::CherryPick, &reason)
    }

    /// Panic restore + a `restore` commit recording the restored state.
    pub fn panic_restore(&self, scope: &PanicScope) -> EngineResult<PanicReport> {
        let report = self.store.panic_restore(scope)?;
        // Record the restored state in the history of the owning game, if any.
        for g in self.store.games()? {
            if let Some(rel) = self.rel_path_for(&g, &report.path) {
                if !report.removed {
                    let bytes = fs::read(&report.path).map_err(|e| EngineError::io(&report.path, e))?;
                    self.commit_changes(&g, vec![(rel, bytes, None)], CommitKind::Restore, &format!("panic restore to {}", report.restored_tag))?;
                }
                break;
            }
        }
        Ok(report)
    }

    /// Quick rollback: restores the previous commit of the current branch.
    pub fn quick_rollback(&self, game_id: GameId) -> EngineResult<CheckoutReport> {
        self.checkout(game_id, "HEAD~1", None)
    }

    // ------------------------------------------------------------------
    // Cheat table & disk trainer
    // ------------------------------------------------------------------

    /// Pins a variable under an alias.
    pub fn pin(&self, game_id: GameId, rel: &str, alias: &str, location: &Location) -> EngineResult<CheatEntry> {
        let game = self.store.game(game_id)?;
        let rel = savelens_cas::normalize_rel_path(rel)?;
        // Validate that the location resolves in the current file.
        let root = self.open_version(game.id, WORKING, &rel)?.root();
        if location.read(&root).is_none() {
            return Err(EngineError::Invalid(format!("{location} does not hold a number in {rel}")));
        }
        Ok(self.store.upsert_cheat(game_id, &rel, alias, location)?)
    }

    /// Cheat table with live values read from disk.
    pub fn cheat_values(&self, game_id: GameId) -> EngineResult<Vec<CheatValue>> {
        let entries = self.store.cheats(game_id)?;
        let mut roots: HashMap<String, Result<Node, String>> = HashMap::new();
        let mut out = Vec::new();
        for e in entries {
            let root = roots
                .entry(e.rel_path.clone())
                .or_insert_with(|| self.root_at(game_id, WORKING, &e.rel_path).map_err(|err| err.to_string()));
            let (current, error) = match root {
                Ok(r) => match e.location.read(r) {
                    Some(v) => (Some(v), None),
                    None => (None, Some(format!("{} not found", e.location))),
                },
                Err(err) => (None, Some(err.clone())),
            };
            out.push(CheatValue { entry: e, current, error });
        }
        Ok(out)
    }

    /// Writes a cheat value to disk ("Apply & Sign Save").
    pub fn set_cheat(&self, game_id: GameId, alias: &str, value: ScalarValue) -> EngineResult<EditReport> {
        let e = self.store.cheat_by_alias(game_id, alias)?;
        let allow_raw = matches!(e.location, Location::Raw { .. });
        self.edit_value(
            game_id,
            &e.rel_path,
            &EditTarget::Location {
                location: e.location.clone(),
                value,
            },
            allow_raw,
        )
    }

    /// Sets a freeze rule.
    pub fn freeze(&self, game_id: GameId, alias: &str, value: Option<ScalarValue>, enabled: bool) -> EngineResult<()> {
        let value = match value {
            Some(v) => Some(v),
            None => self
                .cheat_values(game_id)?
                .into_iter()
                .find(|c| c.entry.alias == alias)
                .and_then(|c| c.current),
        };
        Ok(self.store.set_freeze(game_id, alias, value, enabled)?)
    }

    /// Applies the frozen values of `rel` to `bytes`. Returns the new bytes and
    /// the applied aliases, or `None` when nothing had to change.
    pub fn apply_trainer_bytes(&self, game: &GameRecord, rel: &str, bytes: &[u8]) -> EngineResult<Option<(Vec<u8>, Vec<String>)>> {
        let rules: Vec<CheatEntry> = self
            .store
            .cheats(game.id)?
            .into_iter()
            .filter(|c| c.rel_path == rel && c.freeze_enabled && c.frozen_value.is_some())
            .collect();
        if rules.is_empty() {
            return Ok(None);
        }
        let ctx = self.ctx_for(game, rel);
        let mut opened = self.pipeline.open(bytes, &ctx)?;
        if rules.iter().all(|r| is_payload_raw(&r.location)) {
            // Offsets were captured on the raw payload: edit in place.
            opened = opened.into_raw_mode();
        } else if opened.edit_mode() != EditMode::Structural {
            return Err(EngineError::GateBlocked(format!(
                "{rel}: structural freeze rules need a lossless codec (round-trip {:?})",
                opened.envelope().round_trip
            )));
        }
        let mut root = opened.root();
        let mut applied = Vec::new();
        for r in &rules {
            let Some(v) = r.frozen_value else { continue };
            let pin = savelens_core::PinnedVariable {
                alias: r.alias.clone(),
                location: r.location.clone(),
                frozen_value: Some(v),
            };
            if pin.apply_freeze(&mut root)? {
                applied.push(r.alias.clone());
            }
        }
        if applied.is_empty() {
            return Ok(None);
        }
        opened.apply_ast(&root)?;
        let new = opened.encode_verified(&self.pipeline, &ctx)?;
        Ok(Some((new, applied)))
    }

    /// Freeze-on-save injection into the file on disk.
    pub fn inject_trainer(&self, game_id: GameId, rel: &str) -> EngineResult<Option<TrainerReport>> {
        let start = Instant::now();
        let game = self.store.game(game_id)?;
        let rel = savelens_cas::normalize_rel_path(rel)?;
        let path = self.abs_path(&game, &rel)?;
        let bytes = fs::read(&path).map_err(|e| EngineError::io(&path, e))?;
        let Some((new, applied)) = self.apply_trainer_bytes(&game, &rel, &bytes)? else {
            return Ok(None);
        };
        self.ensure_not_busy(&path)?;
        let m = self.store.safe_write(Some(game.id), &path, &new, &format!("trainer: {}", applied.join(", ")))?;
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        let commit = self
            .commit_changes(&game, vec![(rel.clone(), new, None)], CommitKind::Trainer, &format!("freeze-on-save: {}", applied.join(", ")))?
            .map(|c| c.id);
        Ok(Some(TrainerReport {
            rel_path: rel,
            applied,
            elapsed_ms,
            safety_tag: m.safety.tag,
            commit,
        }))
    }

    /// Opens an arbitrary file (not necessarily tracked) for inspection.
    pub fn inspect_path(&self, path: &Path) -> EngineResult<OpenedSave> {
        let bytes = fs::read(path).map_err(|e| EngineError::io(path, e))?;
        let ctx = DetectContext {
            file_name: path.file_name().map(|n| n.to_string_lossy().to_string()),
            app_id: None,
        };
        Ok(self.pipeline.open(&bytes, &ctx)?)
    }
}
