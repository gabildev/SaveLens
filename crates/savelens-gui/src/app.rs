//! Application state, background message handling and every user action of
//! the SaveLens desktop app. Drawing lives in [`crate::views`]; this module is
//! the headless-testable "view-model".

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use savelens_cas::{
    BranchRecord, Changelog, CommitId, CommitKind, CommitRecord, DaemonStatus, EventRecord, GameId, GameRecord,
    PanicScope, StoreStats, TagRecord,
};
use savelens_core::{CherryPickOptions, EditMode, Endianness, Location, Node, NodePath, ScalarValue, ScanType};
use savelens_discovery::hunter::{HuntOptions, HuntReport};
use savelens_discovery::{DiscoveryConfig, DiscoveryReport};
use savelens_watcher::{
    CheatValue, Daemon, DaemonEvent, DaemonHandle, DaemonOptions, EditTarget, Engine, EngineError, FileDiff,
    TrackedFile, WORKING,
};

use crate::graph::{layout, GraphLayout};
use crate::jobs::{Job, Worker};
use crate::scanner::{default_alias, run_scan, CheatTableModel, ScanOutcome, ScannerModel};
use crate::toast::Toasts;
use crate::treediff::{visible_paths, TreeDiffModel};

/// Commits shown in the timeline (newest first).
pub const TIMELINE_LIMIT: usize = 300;
/// Duration of an auto-hunt.
pub const HUNT_DURATION: Duration = Duration::from_secs(20);
/// Activity entries kept in memory.
const ACTIVITY_LIMIT: usize = 500;
const ACTIVITY_POLL: Duration = Duration::from_secs(2);
const CHEAT_POLL: Duration = Duration::from_secs(5);

/// Main views.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum View {
    Timeline,
    Scanner,
    TreeDiff,
    Editor,
    Activity,
}

impl View {
    pub const ALL: [View; 5] = [View::Timeline, View::Scanner, View::TreeDiff, View::Editor, View::Activity];

    pub fn label(self) -> &'static str {
        match self {
            View::Timeline => "Timeline",
            View::Scanner => "Scanner Studio",
            View::TreeDiff => "Tree Diff & Cherry-Pick",
            View::Editor => "AST Editor",
            View::Activity => "Activity",
        }
    }
}

/// What to reload after an action.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Refresh {
    pub games: bool,
    pub history: bool,
    pub files: bool,
    pub cheats: bool,
    pub trees: bool,
}

impl Refresh {
    pub const NONE: Refresh = Refresh { games: false, history: false, files: false, cheats: false, trees: false };
    pub const ALL: Refresh = Refresh { games: true, history: true, files: true, cheats: true, trees: true };
    pub const HISTORY: Refresh = Refresh { history: true, ..Refresh::NONE };
    pub const CHEATS: Refresh = Refresh { cheats: true, ..Refresh::NONE };
}

/// Integrity information of the selected file.
#[derive(Debug, Clone)]
pub struct IntegrityInfo {
    pub rel: String,
    pub badges: Vec<String>,
    pub edit_mode: EditMode,
    pub codec: String,
}

/// History of the selected game.
#[derive(Debug, Clone)]
pub struct HistoryData {
    pub game: GameId,
    pub commits: Vec<CommitRecord>,
    pub branches: Vec<BranchRecord>,
    pub tags: Vec<TagRecord>,
    pub changelogs: HashMap<CommitId, Vec<Changelog>>,
    pub layout: GraphLayout,
    pub current_branch: String,
}

/// Loaded trees of the diff view.
#[derive(Debug, Clone)]
pub struct DiffData {
    pub rel: String,
    pub from_rev: String,
    pub model: TreeDiffModel,
    pub files: Vec<FileDiff>,
}

/// Loaded tree of the AST editor.
#[derive(Debug, Clone)]
pub struct EditorData {
    pub rel: String,
    pub root: Node,
    pub edit_mode: EditMode,
    pub gate_reason: Option<String>,
}

/// One line of the activity log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityEntry {
    pub at: i64,
    pub source: &'static str,
    pub kind: String,
    pub message: String,
}

/// Panic restore confirmation.
#[derive(Debug, Clone)]
pub struct PanicPrompt {
    pub scope: PanicScope,
    pub tag: String,
    pub path: PathBuf,
    pub reason: String,
    pub created_at: i64,
}

/// Messages from the engine worker.
pub(crate) enum WorkerMsg {
    Action {
        label: String,
        result: Result<String, String>,
        refresh: Refresh,
        /// Safety tag produced by the action, if any.
        safety_tag: Option<String>,
    },
    History(Result<HistoryData, String>),
    Files {
        game: GameId,
        result: Result<Vec<TrackedFile>, String>,
    },
    Integrity {
        game: GameId,
        result: Result<IntegrityInfo, String>,
    },
    Cheats {
        game: GameId,
        result: Result<Vec<CheatValue>, String>,
    },
    Diff {
        seq: u64,
        result: Result<DiffData, String>,
    },
    Editor {
        seq: u64,
        result: Result<EditorData, String>,
    },
    Activity(Result<(Vec<EventRecord>, Option<DaemonStatus>, StoreStats), String>),
}

fn worker_panic(msg: String) -> WorkerMsg {
    WorkerMsg::Action {
        label: "background task".into(),
        result: Err(format!("internal error: {msg}")),
        refresh: Refresh::NONE,
        safety_tag: None,
    }
}

/// Outcome of an action closure.
pub(crate) struct ActionOk {
    pub message: String,
    pub safety_tag: Option<String>,
}

impl From<String> for ActionOk {
    fn from(message: String) -> Self {
        ActionOk { message, safety_tag: None }
    }
}

/// UI intents collected while drawing and executed afterwards.
#[derive(Debug, Clone)]
pub(crate) enum Intent {
    SelectGame(GameId),
    SelectFile(String),
    SetView(View),
    Checkout(CommitId),
    OpenBranchDialog(CommitId),
    OpenTagDialog(CommitId),
    SwitchBranch(String),
    DiffVsWorking(CommitId),
    QuickSnapshot,
    QuickRollback,
    FirstScan,
    NextScan,
    ResetScan,
    AddCandidate(usize),
    ApplyCheats,
    Freeze { alias: String, enabled: bool },
    DeleteCheat(String),
    SetTrainer(bool),
    SetWatch(GameId, bool),
    RequestPick(Vec<NodePath>),
    LoadDiff,
    LoadEditor,
    EditValue { path: NodePath, value: String },
    RawWrite,
    RequestPanic,
    ToggleDaemon,
    Discover,
    Hunt,
    TrackDiscovered { game: usize, location: usize },
    TrackCandidate(usize),
    TrackFolder,
}

/// Text dialog for a new branch / tag.
#[derive(Debug, Clone)]
pub(crate) struct NameDialog {
    pub commit: CommitId,
    pub is_tag: bool,
    pub name: String,
}

/// State of the tree diff view.
#[derive(Debug, Default)]
pub(crate) struct DiffUi {
    pub source_rev: String,
    pub data: Option<DiffData>,
    pub loading: bool,
    pub seq: u64,
    pub search: String,
    pub applied_search: String,
    pub visible_left: Option<HashSet<String>>,
    pub visible_right: Option<HashSet<String>>,
    pub selected: BTreeSet<String>,
    pub pending_pick: Option<Vec<NodePath>>,
    pub preview: Vec<String>,
    pub error: Option<String>,
}

/// State of the AST editor.
#[derive(Debug)]
pub(crate) struct EditorUi {
    pub data: Option<EditorData>,
    pub loading: bool,
    pub seq: u64,
    pub edits: HashMap<String, String>,
    pub gate_error: Option<String>,
    pub raw_mode: bool,
    pub raw_offset: String,
    pub raw_type: ScanType,
    pub raw_endian: Endianness,
    pub raw_value: String,
    pub search: String,
    pub applied_search: String,
    pub visible: Option<HashSet<String>>,
    pub error: Option<String>,
}

impl Default for EditorUi {
    fn default() -> Self {
        EditorUi {
            data: None,
            loading: false,
            seq: 0,
            edits: HashMap::new(),
            gate_error: None,
            raw_mode: false,
            raw_offset: "0x0".into(),
            raw_type: ScanType::I32,
            raw_endian: Endianness::Little,
            raw_value: String::new(),
            search: String::new(),
            applied_search: String::new(),
            visible: None,
            error: None,
        }
    }
}

/// State of the sidebar (discovery, hunt, track form).
#[derive(Default)]
pub(crate) struct SidebarUi {
    pub discover_job: Option<Job<Result<DiscoveryReport, String>>>,
    pub discovery: Option<DiscoveryReport>,
    pub hunt_job: Option<Job<Result<HuntReport, String>>>,
    pub hunt: Option<HuntReport>,
    pub track_name: String,
    pub track_dir: String,
    pub track_patterns: String,
}

/// The SaveLens desktop application.
pub struct SaveLensApp {
    pub(crate) engine: Engine,
    worker: Worker<WorkerMsg>,
    pub(crate) view: View,
    theme_applied: bool,
    pub(crate) toasts: Toasts,
    pub(crate) games: Vec<GameRecord>,
    pub(crate) selected_game: Option<GameId>,
    pub(crate) files: Vec<TrackedFile>,
    pub(crate) selected_file: Option<String>,
    pub(crate) integrity: Option<IntegrityInfo>,
    pub(crate) integrity_error: Option<String>,
    pub(crate) history: Option<HistoryData>,
    pub(crate) history_loading: bool,
    pub(crate) expanded_commits: HashSet<CommitId>,
    pub(crate) name_dialog: Option<NameDialog>,
    pub(crate) scanner: ScannerModel,
    pub(crate) scan_job: Option<Job<ScanOutcome>>,
    pub(crate) cheats: CheatTableModel,
    pub(crate) diff: DiffUi,
    pub(crate) editor: EditorUi,
    pub(crate) activity: Vec<ActivityEntry>,
    last_event_id: Option<i64>,
    pub(crate) external_daemon: Option<DaemonStatus>,
    pub(crate) stats: Option<StoreStats>,
    pub(crate) daemon: Option<DaemonHandle>,
    pub(crate) daemon_hotkeys: bool,
    pub(crate) sidebar: SidebarUi,
    pub(crate) panic: Option<PanicPrompt>,
    pub(crate) panic_game_scope: bool,
    pub(crate) intents: Vec<Intent>,
    pub(crate) last_safety_tag: Option<String>,
    last_activity_poll: Option<Instant>,
    last_cheat_poll: Option<Instant>,
    activity_inflight: bool,
}

fn err_string(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl SaveLensApp {
    /// Creates the app: loads the games and starts the engine worker.
    pub fn new(engine: Engine) -> anyhow::Result<Self> {
        let worker = Worker::spawn(engine.clone(), worker_panic)?;
        let games = engine.store().games()?;
        let mut app = SaveLensApp {
            engine,
            worker,
            view: View::Timeline,
            theme_applied: false,
            toasts: Toasts::default(),
            games,
            selected_game: None,
            files: Vec::new(),
            selected_file: None,
            integrity: None,
            integrity_error: None,
            history: None,
            history_loading: false,
            expanded_commits: HashSet::new(),
            name_dialog: None,
            scanner: ScannerModel::default(),
            scan_job: None,
            cheats: CheatTableModel::default(),
            diff: DiffUi { source_rev: "HEAD~1".into(), ..DiffUi::default() },
            editor: EditorUi::default(),
            activity: Vec::new(),
            last_event_id: None,
            external_daemon: None,
            stats: None,
            daemon: None,
            daemon_hotkeys: true,
            sidebar: SidebarUi { track_patterns: "*".into(), ..SidebarUi::default() },
            panic: None,
            panic_game_scope: false,
            intents: Vec::new(),
            last_safety_tag: None,
            last_activity_poll: None,
            last_cheat_poll: None,
            activity_inflight: false,
        };
        if let Some(first) = app.games.first().map(|g| g.id) {
            app.select_game(first);
        }
        Ok(app)
    }

    /// The engine.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Draws one frame (the whole UI).
    pub fn ui(&mut self, ctx: &egui::Context) {
        if !self.theme_applied {
            crate::theme::apply(ctx);
            self.theme_applied = true;
        }
        self.poll();
        self.periodic();
        crate::views::draw(self, ctx);
        let intents = std::mem::take(&mut self.intents);
        for i in intents {
            self.handle_intent(i);
        }
        self.toasts.show(ctx);
        if self.is_busy() {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    // ------------------------------------------------------------------
    // Accessors
    // ------------------------------------------------------------------

    pub fn view(&self) -> View {
        self.view
    }

    pub fn set_view(&mut self, view: View) {
        self.view = view;
        match view {
            View::TreeDiff if self.diff.data.is_none() && !self.diff.loading => self.load_diff(),
            View::Editor if self.editor.data.is_none() && !self.editor.loading => self.load_editor(),
            _ => {}
        }
    }

    pub fn toasts(&self) -> &Toasts {
        &self.toasts
    }

    pub fn games(&self) -> &[GameRecord] {
        &self.games
    }

    pub fn selected_game(&self) -> Option<GameId> {
        self.selected_game
    }

    pub(crate) fn selected_game_record(&self) -> Option<&GameRecord> {
        self.selected_game.and_then(|id| self.games.iter().find(|g| g.id == id))
    }

    pub fn files(&self) -> &[TrackedFile] {
        &self.files
    }

    pub fn selected_file(&self) -> Option<&str> {
        self.selected_file.as_deref()
    }

    pub fn integrity(&self) -> Option<&IntegrityInfo> {
        self.integrity.as_ref()
    }

    pub fn history(&self) -> Option<&HistoryData> {
        self.history.as_ref()
    }

    pub fn scanner(&self) -> &ScannerModel {
        &self.scanner
    }

    pub fn scanner_mut(&mut self) -> &mut ScannerModel {
        &mut self.scanner
    }

    pub fn cheats(&self) -> &CheatTableModel {
        &self.cheats
    }

    pub fn cheats_mut(&mut self) -> &mut CheatTableModel {
        &mut self.cheats
    }

    pub fn diff_model(&self) -> Option<&TreeDiffModel> {
        self.diff.data.as_ref().map(|d| &d.model)
    }

    pub fn diff_source(&self) -> &str {
        &self.diff.source_rev
    }

    pub fn editor_root(&self) -> Option<&Node> {
        self.editor.data.as_ref().map(|d| &d.root)
    }

    pub fn editor_gate_error(&self) -> Option<&str> {
        self.editor.gate_error.as_deref()
    }

    pub fn activity(&self) -> &[ActivityEntry] {
        &self.activity
    }

    pub fn panic_prompt(&self) -> Option<&PanicPrompt> {
        self.panic.as_ref()
    }

    pub fn pick_preview(&self) -> Option<(&[NodePath], &[String])> {
        self.diff.pending_pick.as_deref().map(|p| (p, self.diff.preview.as_slice()))
    }

    /// Safety tag of the last mutation performed from the GUI.
    pub fn last_safety_tag(&self) -> Option<&str> {
        self.last_safety_tag.as_deref()
    }

    pub fn daemon_running(&self) -> bool {
        self.daemon.is_some()
    }

    /// Whether the embedded daemon registers global hotkeys.
    pub fn set_daemon_hotkeys(&mut self, on: bool) {
        self.daemon_hotkeys = on;
    }

    /// True while any background work is pending.
    pub fn is_busy(&self) -> bool {
        self.worker.pending() > 0
            || self.scan_job.is_some()
            || self.sidebar.discover_job.is_some()
            || self.sidebar.hunt_job.is_some()
    }

    /// Processes background results until nothing is pending or `timeout` elapses.
    /// Returns true when idle.
    pub fn wait_idle(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            self.poll();
            if !self.is_busy() {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let slice = (deadline - now).min(Duration::from_millis(20));
            if self.worker.pending() > 0 {
                if let Some(m) = self.worker.recv_timeout(slice) {
                    self.handle_msg(m);
                }
            } else if let Some(job) = self.scan_job.as_mut() {
                if let Some(r) = job.wait(slice) {
                    self.scan_job = None;
                    self.handle_scan(r);
                }
            } else {
                std::thread::sleep(slice);
            }
        }
    }

    // ------------------------------------------------------------------
    // Background plumbing
    // ------------------------------------------------------------------

    fn submit(&mut self, task: impl FnOnce(&Engine) -> WorkerMsg + Send + 'static) {
        if !self.worker.submit(task) {
            self.toasts.error("the engine worker stopped; restart SaveLens");
        }
    }

    /// Runs a (mutating) engine operation on the worker.
    pub(crate) fn run_action(
        &mut self,
        label: impl Into<String>,
        refresh: Refresh,
        f: impl FnOnce(&Engine) -> Result<ActionOk, String> + Send + 'static,
    ) {
        let label = label.into();
        self.submit(move |engine| {
            let (result, safety_tag) = match f(engine) {
                Ok(ok) => (Ok(ok.message), ok.safety_tag),
                Err(e) => (Err(e), None),
            };
            WorkerMsg::Action { label, result, refresh, safety_tag }
        });
    }

    /// Processes finished background work (non-blocking).
    pub fn poll(&mut self) {
        while let Some(m) = self.worker.try_recv() {
            self.handle_msg(m);
        }
        if let Some(job) = self.scan_job.as_mut() {
            if let Some(r) = job.poll() {
                self.scan_job = None;
                self.handle_scan(r);
            }
        }
        if let Some(job) = self.sidebar.discover_job.as_mut() {
            if let Some(r) = job.poll() {
                self.sidebar.discover_job = None;
                match r.and_then(|x| x) {
                    Ok(rep) => {
                        self.toasts.success(format!(
                            "Discovery finished: {} game(s), {} warning(s)",
                            rep.games.len(),
                            rep.warnings.len()
                        ));
                        self.sidebar.discovery = Some(rep);
                    }
                    Err(e) => self.toasts.error(format!("Discovery failed: {e}")),
                }
            }
        }
        if let Some(job) = self.sidebar.hunt_job.as_mut() {
            if let Some(r) = job.poll() {
                self.sidebar.hunt_job = None;
                match r.and_then(|x| x) {
                    Ok(rep) => {
                        self.toasts.success(format!(
                            "Auto-hunt finished: {} candidate(s) in {} watched root(s)",
                            rep.candidates.len(),
                            rep.roots_watched.len()
                        ));
                        self.sidebar.hunt = Some(rep);
                    }
                    Err(e) => self.toasts.error(format!("Auto-hunt failed: {e}")),
                }
            }
        }
        self.poll_daemon();
    }

    fn periodic(&mut self) {
        let now = Instant::now();
        if !self.activity_inflight && self.last_activity_poll.is_none_or(|t| now.duration_since(t) >= ACTIVITY_POLL) {
            self.last_activity_poll = Some(now);
            self.request_activity();
        }
        if self.view == View::Scanner && self.last_cheat_poll.is_none_or(|t| now.duration_since(t) >= CHEAT_POLL) {
            self.last_cheat_poll = Some(now);
            self.request_cheats();
        }
    }

    fn handle_msg(&mut self, msg: WorkerMsg) {
        match msg {
            WorkerMsg::Action { label, result, refresh, safety_tag } => {
                match result {
                    Ok(m) => self.toasts.success(m),
                    Err(e) => {
                        if label == "edit" && e.contains("Safe Edit Gate") {
                            self.editor.gate_error = Some(e.clone());
                        }
                        self.toasts.error(format!("{label}: {e}"));
                    }
                }
                if let Some(t) = safety_tag {
                    self.cheats.last_safety_tag = Some(t.clone());
                    self.last_safety_tag = Some(t);
                }
                self.apply_refresh(refresh);
            }
            WorkerMsg::History(r) => {
                self.history_loading = false;
                match r {
                    Ok(h) if Some(h.game) == self.selected_game => self.history = Some(h),
                    Ok(_) => {}
                    Err(e) => self.toasts.error(format!("Cannot load history: {e}")),
                }
            }
            WorkerMsg::Files { game, result } => {
                if Some(game) != self.selected_game {
                    return;
                }
                match result {
                    Ok(files) => {
                        self.files = files;
                        let keep =
                            self.selected_file.as_ref().is_some_and(|s| self.files.iter().any(|f| &f.rel_path == s));
                        if !keep {
                            if let Some(first) = self.files.first().map(|f| f.rel_path.clone()) {
                                self.select_file(&first);
                            } else {
                                self.selected_file = None;
                                self.integrity = None;
                            }
                        } else {
                            self.request_integrity();
                        }
                    }
                    Err(e) => self.toasts.error(format!("Cannot list tracked files: {e}")),
                }
            }
            WorkerMsg::Integrity { game, result } => {
                if Some(game) != self.selected_game {
                    return;
                }
                match result {
                    Ok(info) if Some(info.rel.as_str()) == self.selected_file.as_deref() => {
                        self.integrity = Some(info);
                        self.integrity_error = None;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        self.integrity = None;
                        self.integrity_error = Some(e);
                    }
                }
            }
            WorkerMsg::Cheats { game, result } => {
                if Some(game) != self.selected_game {
                    return;
                }
                match result {
                    Ok(rows) => {
                        self.cheats.set_rows(rows);
                        self.cheats.error = None;
                    }
                    Err(e) => self.cheats.error = Some(e),
                }
            }
            WorkerMsg::Diff { seq, result } => {
                if seq != self.diff.seq {
                    return;
                }
                self.diff.loading = false;
                match result {
                    Ok(d) => {
                        self.diff.error = None;
                        self.diff.selected.clear();
                        self.diff.data = Some(d);
                        self.refilter_diff();
                    }
                    Err(e) => {
                        self.diff.data = None;
                        self.diff.error = Some(e);
                    }
                }
            }
            WorkerMsg::Editor { seq, result } => {
                if seq != self.editor.seq {
                    return;
                }
                self.editor.loading = false;
                match result {
                    Ok(d) => {
                        self.editor.error = None;
                        self.editor.edits.clear();
                        if d.edit_mode == EditMode::RawInPlace {
                            self.editor.raw_mode = true;
                        }
                        self.editor.data = Some(d);
                        self.refilter_editor();
                    }
                    Err(e) => {
                        self.editor.data = None;
                        self.editor.error = Some(e);
                    }
                }
            }
            WorkerMsg::Activity(r) => {
                self.activity_inflight = false;
                match r {
                    Ok((events, status, stats)) => {
                        self.external_daemon = status;
                        self.stats = Some(stats);
                        if let Some(max) = events.iter().map(|e| e.id).max() {
                            self.last_event_id = Some(max);
                        }
                        // `events` is newest first; prepend preserving that order.
                        let mut fresh: Vec<ActivityEntry> = events
                            .into_iter()
                            .map(|e| ActivityEntry { at: e.at, source: "store", kind: e.kind, message: e.message })
                            .collect();
                        if !fresh.is_empty() {
                            fresh.append(&mut self.activity);
                            self.activity = fresh;
                            self.activity.sort_by_key(|a| std::cmp::Reverse(a.at));
                            self.activity.truncate(ACTIVITY_LIMIT);
                        }
                    }
                    Err(e) => tracing::warn!("activity poll failed: {e}"),
                }
            }
        }
    }

    fn apply_refresh(&mut self, r: Refresh) {
        if r.games {
            self.reload_games();
        }
        if r.files {
            self.request_files();
        } else if r.trees {
            self.request_integrity();
        }
        if r.history {
            self.request_history();
        }
        if r.cheats {
            self.request_cheats();
        }
        if r.trees {
            if self.diff.data.is_some() || self.view == View::TreeDiff {
                self.load_diff();
            }
            if self.editor.data.is_some() || self.view == View::Editor {
                self.load_editor();
            }
        }
    }

    fn reload_games(&mut self) {
        match self.engine.store().games() {
            Ok(g) => {
                self.games = g;
                let still = self.selected_game.is_some_and(|id| self.games.iter().any(|g| g.id == id));
                if !still {
                    if let Some(first) = self.games.first().map(|g| g.id) {
                        self.select_game(first);
                    } else {
                        self.selected_game = None;
                    }
                }
            }
            Err(e) => self.toasts.error(format!("Cannot list games: {e}")),
        }
    }

    fn request_history(&mut self) {
        let Some(game) = self.selected_game else { return };
        self.history_loading = true;
        self.submit(move |engine| WorkerMsg::History(load_history(engine, game)));
    }

    fn request_files(&mut self) {
        let Some(game) = self.selected_game else { return };
        self.submit(move |engine| {
            let result = engine.game(game).and_then(|g| engine.tracked_files(&g)).map_err(err_string);
            WorkerMsg::Files { game, result }
        });
    }

    fn request_integrity(&mut self) {
        let (Some(game), Some(rel)) = (self.selected_game, self.selected_file.clone()) else {
            return;
        };
        self.submit(move |engine| {
            let result = engine
                .open_version(game, WORKING, &rel)
                .map(|o| IntegrityInfo {
                    rel: rel.clone(),
                    badges: o.envelope().badges(),
                    edit_mode: o.edit_mode(),
                    codec: o.codec_id().to_string(),
                })
                .map_err(err_string);
            WorkerMsg::Integrity { game, result }
        });
    }

    /// Reloads the cheat table (live values) in the background.
    pub fn request_cheats(&mut self) {
        let Some(game) = self.selected_game else { return };
        self.submit(move |engine| WorkerMsg::Cheats { game, result: engine.cheat_values(game).map_err(err_string) });
    }

    fn request_activity(&mut self) {
        let after = self.last_event_id;
        self.activity_inflight = true;
        self.submit(move |engine| {
            let store = engine.store();
            let r = store
                .events(after, 200)
                .and_then(|ev| Ok((ev, store.daemon_status(5_000)?, store.stats()?)))
                .map_err(err_string);
            WorkerMsg::Activity(r)
        });
    }

    // ------------------------------------------------------------------
    // Selection
    // ------------------------------------------------------------------

    /// Selects a game and reloads everything that depends on it.
    pub fn select_game(&mut self, id: GameId) {
        if self.selected_game != Some(id) {
            self.scanner.reset();
            self.cheats = CheatTableModel::default();
            self.history = None;
            self.files.clear();
            self.selected_file = None;
            self.integrity = None;
            self.diff.data = None;
            self.editor.data = None;
            self.expanded_commits.clear();
        }
        self.selected_game = Some(id);
        self.request_files();
        self.request_history();
        self.request_cheats();
    }

    /// Selects a tracked file.
    pub fn select_file(&mut self, rel: &str) {
        let changed = self.selected_file.as_deref() != Some(rel);
        self.selected_file = Some(rel.to_string());
        self.integrity = None;
        self.integrity_error = None;
        self.request_integrity();
        if changed {
            if self.diff.data.is_some() || self.view == View::TreeDiff {
                self.load_diff();
            }
            if self.editor.data.is_some() || self.view == View::Editor {
                self.load_editor();
            }
        }
    }

    // ------------------------------------------------------------------
    // Intents
    // ------------------------------------------------------------------

    fn report(&mut self, r: Result<(), String>) {
        if let Err(e) = r {
            self.toasts.error(e);
        }
    }

    pub(crate) fn handle_intent(&mut self, intent: Intent) {
        match intent {
            Intent::SelectGame(id) => self.select_game(id),
            Intent::SelectFile(f) => self.select_file(&f),
            Intent::SetView(v) => self.set_view(v),
            Intent::Checkout(c) => {
                let r = self.checkout(c);
                self.report(r)
            }
            Intent::OpenBranchDialog(c) => {
                self.name_dialog = Some(NameDialog { commit: c, is_tag: false, name: String::new() })
            }
            Intent::OpenTagDialog(c) => {
                self.name_dialog = Some(NameDialog { commit: c, is_tag: true, name: String::new() })
            }
            Intent::SwitchBranch(b) => {
                let r = self.switch_branch(&b);
                self.report(r)
            }
            Intent::DiffVsWorking(c) => self.diff_vs_working(c),
            Intent::QuickSnapshot => {
                let r = self.quick_snapshot();
                self.report(r)
            }
            Intent::QuickRollback => {
                let r = self.quick_rollback();
                self.report(r)
            }
            Intent::FirstScan => {
                let r = self.start_first_scan();
                self.report(r)
            }
            Intent::NextScan => {
                let r = self.start_next_scan();
                self.report(r)
            }
            Intent::ResetScan => self.reset_scan(),
            Intent::AddCandidate(i) => {
                let r = self.add_candidate_to_cheats(i, None);
                self.report(r)
            }
            Intent::ApplyCheats => {
                let r = self.apply_and_sign();
                self.report(r)
            }
            Intent::Freeze { alias, enabled } => {
                let r = self.set_freeze(&alias, enabled);
                self.report(r)
            }
            Intent::DeleteCheat(a) => {
                let r = self.delete_cheat(&a);
                self.report(r)
            }
            Intent::SetTrainer(on) => {
                let r = self.set_trainer(on);
                self.report(r)
            }
            Intent::SetWatch(g, on) => self.set_watch(g, on),
            Intent::RequestPick(p) => self.request_cherry_pick(p),
            Intent::LoadDiff => self.load_diff(),
            Intent::LoadEditor => self.load_editor(),
            Intent::EditValue { path, value } => {
                let r = self.edit_value(path, value);
                self.report(r)
            }
            Intent::RawWrite => {
                let r = self.raw_write();
                self.report(r)
            }
            Intent::RequestPanic => {
                let scope_game = self.panic_game_scope;
                let r = self.request_panic(scope_game);
                self.report(r)
            }
            Intent::ToggleDaemon => self.toggle_daemon(),
            Intent::Discover => {
                let r = self.start_discovery();
                self.report(r)
            }
            Intent::Hunt => {
                let r = self.start_hunt(HUNT_DURATION);
                self.report(r)
            }
            Intent::TrackDiscovered { game, location } => {
                let r = self.track_discovered(game, location);
                self.report(r)
            }
            Intent::TrackCandidate(i) => {
                let r = self.track_candidate(i);
                self.report(r)
            }
            Intent::TrackFolder => {
                let name = self.sidebar.track_name.clone();
                let dir = self.sidebar.track_dir.clone();
                let pats = self.sidebar.track_patterns.clone();
                let r = self.track_folder(&name, Path::new(dir.trim()), &pats);
                self.report(r)
            }
        }
    }

    fn require_game(&self) -> Result<GameId, String> {
        self.selected_game.ok_or_else(|| "select or track a game first".to_string())
    }

    fn require_file(&self) -> Result<(GameId, String), String> {
        let g = self.require_game()?;
        let f = self.selected_file.clone().ok_or_else(|| "the game has no tracked file selected".to_string())?;
        Ok((g, f))
    }

    // ------------------------------------------------------------------
    // Timeline actions
    // ------------------------------------------------------------------

    /// Restores every file of a commit onto disk.
    pub fn checkout(&mut self, commit: CommitId) -> Result<(), String> {
        let game = self.require_game()?;
        self.run_action("checkout", Refresh::ALL, move |e| {
            let r = e.checkout(game, &format!("#{commit}"), None).map_err(err_string)?;
            Ok(ActionOk {
                message: format!(
                    "Checked out #{commit}: {} file(s) written, {} unchanged",
                    r.written.len(),
                    r.unchanged.len()
                ),
                safety_tag: r.safety_tags.last().cloned(),
            })
        });
        Ok(())
    }

    /// Creates a branch at a commit.
    pub fn create_branch_at(&mut self, commit: CommitId, name: &str) -> Result<(), String> {
        let game = self.require_game()?;
        let name = name.trim().to_string();
        savelens_cas::validate_branch_name(&name).map_err(err_string)?;
        self.run_action("branch", Refresh::HISTORY, move |e| {
            e.create_branch(game, &name, Some(&format!("#{commit}"))).map_err(err_string)?;
            Ok(format!("Branch `{name}` created at #{commit}").into())
        });
        Ok(())
    }

    /// Tags a commit.
    pub fn tag_commit(&mut self, commit: CommitId, name: &str) -> Result<(), String> {
        let game = self.require_game()?;
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err("tag name is empty".into());
        }
        self.run_action("tag", Refresh::HISTORY, move |e| {
            e.store().create_tag(game, &name, commit).map_err(err_string)?;
            Ok(format!("Tagged #{commit} as `{name}`").into())
        });
        Ok(())
    }

    /// Switches branch (writes the branch head's files).
    pub fn switch_branch(&mut self, name: &str) -> Result<(), String> {
        let game = self.require_game()?;
        let name = name.to_string();
        self.run_action("switch branch", Refresh::ALL, move |e| {
            let r = e.switch_branch(game, &name).map_err(err_string)?;
            Ok(ActionOk {
                message: format!("Switched to `{name}` ({} file(s) written)", r.written.len()),
                safety_tag: r.safety_tags.last().cloned(),
            })
        });
        Ok(())
    }

    /// Snapshots the tracked files now.
    pub fn quick_snapshot(&mut self) -> Result<(), String> {
        let game = self.require_game()?;
        self.run_action("snapshot", Refresh::HISTORY, move |e| {
            let c = e.snapshot(game, "quick snapshot (GUI)", CommitKind::Manual).map_err(err_string)?;
            Ok(match c {
                Some(c) => format!("Snapshot #{}: {}", c.id, c.summary),
                None => "Nothing changed since the last commit".to_string(),
            }
            .into())
        });
        Ok(())
    }

    /// Restores the previous commit of the current branch.
    pub fn quick_rollback(&mut self) -> Result<(), String> {
        let game = self.require_game()?;
        self.run_action("rollback", Refresh::ALL, move |e| {
            let r = e.quick_rollback(game).map_err(err_string)?;
            Ok(ActionOk {
                message: format!("Rolled back to #{} ({} file(s) written)", r.target, r.written.len()),
                safety_tag: r.safety_tags.last().cloned(),
            })
        });
        Ok(())
    }

    /// Opens the tree diff of the selected file between a commit and WORKING.
    pub fn diff_vs_working(&mut self, commit: CommitId) {
        self.diff.source_rev = format!("#{commit}");
        self.view = View::TreeDiff;
        self.load_diff();
    }

    pub fn toggle_commit_expanded(&mut self, commit: CommitId) {
        if !self.expanded_commits.remove(&commit) {
            self.expanded_commits.insert(commit);
        }
    }

    // ------------------------------------------------------------------
    // Scanner
    // ------------------------------------------------------------------

    /// Starts a first scan over the selected file at the scanner's source revision.
    pub fn start_first_scan(&mut self) -> Result<(), String> {
        if self.scan_job.is_some() {
            return Err("a scan is already running".into());
        }
        let (game, rel) = self.require_file()?;
        let (req, session) = self.scanner.prepare_first(game, &rel)?;
        self.spawn_scan(req, session)
    }

    /// Starts a next scan.
    pub fn start_next_scan(&mut self) -> Result<(), String> {
        if self.scan_job.is_some() {
            return Err("a scan is already running".into());
        }
        let game = self.require_game()?;
        let (req, session) = self.scanner.prepare_next(game)?;
        self.spawn_scan(req, session)
    }

    fn spawn_scan(
        &mut self,
        req: crate::scanner::ScanRequest,
        session: savelens_core::ScanSession,
    ) -> Result<(), String> {
        let engine = self.engine.clone();
        let label = if req.first { "first scan" } else { "next scan" };
        let job = Job::spawn(label, None, move || run_scan(&engine, &req, session)).map_err(err_string)?;
        self.scan_job = Some(job);
        Ok(())
    }

    fn handle_scan(&mut self, r: Result<ScanOutcome, String>) {
        match r {
            Ok(out) => match self.scanner.finish(out) {
                Ok(n) => self.toasts.info(format!("Scan finished: {n} candidate(s)")),
                Err(e) => self.toasts.error(format!("Scan failed: {e}")),
            },
            Err(e) => {
                // The session was lost with the thread.
                self.scanner.reset();
                self.toasts.error(format!("Scan failed: {e}"));
            }
        }
    }

    pub fn reset_scan(&mut self) {
        self.scanner.reset();
    }

    pub fn scan_running(&self) -> bool {
        self.scan_job.is_some()
    }

    /// Pins scan result `index` in the cheat table.
    pub fn add_candidate_to_cheats(&mut self, index: usize, alias: Option<&str>) -> Result<(), String> {
        let game = self.require_game()?;
        let cand = self.scanner.results().get(index).cloned().ok_or_else(|| format!("no scan result #{index}"))?;
        let rel = self.scanner.scan_file.clone().ok_or_else(|| "no scan session".to_string())?;
        let alias = match alias {
            Some(a) if !a.trim().is_empty() => a.trim().to_string(),
            _ => default_alias(&cand.location, &self.cheats.aliases()),
        };
        let location = cand.location.clone();
        self.run_action("pin", Refresh::CHEATS, move |e| {
            e.pin(game, &rel, &alias, &location).map_err(err_string)?;
            Ok(format!("Pinned {location} as `{alias}`").into())
        });
        Ok(())
    }

    /// Writes every edited cheat value to disk ("Apply & Sign Save").
    pub fn apply_and_sign(&mut self) -> Result<(), String> {
        let game = self.require_game()?;
        let edits = self.cheats.pending_edits()?;
        if edits.is_empty() {
            return Err("no edited values to apply".into());
        }
        for (alias, _) in &edits {
            self.cheats.edits.remove(alias);
        }
        let refresh = Refresh { history: true, cheats: true, trees: true, ..Refresh::NONE };
        self.run_action("apply & sign", refresh, move |e| {
            let mut tags = Vec::new();
            let mut lines = Vec::new();
            for (alias, value) in &edits {
                let r = e.set_cheat(game, alias, *value).map_err(|err| format!("{alias}: {err}"))?;
                if r.no_op {
                    lines.push(format!("{alias} already {value}"));
                } else {
                    lines.push(format!("{alias} = {value}"));
                }
                if let Some(t) = r.safety_tag {
                    tags.push(t);
                }
            }
            let tag = tags.last().cloned();
            let msg = match &tag {
                Some(t) => format!("Applied & signed: {} (safety backup {t})", lines.join(", ")),
                None => format!("Nothing to write: {}", lines.join(", ")),
            };
            Ok(ActionOk { message: msg, safety_tag: tag })
        });
        Ok(())
    }

    /// Enables / disables freeze-on-save for a cheat.
    pub fn set_freeze(&mut self, alias: &str, enabled: bool) -> Result<(), String> {
        let game = self.require_game()?;
        let value = self.cheats.freeze_value(alias)?;
        let alias = alias.to_string();
        self.run_action("freeze", Refresh::CHEATS, move |e| {
            e.freeze(game, &alias, value, enabled).map_err(err_string)?;
            Ok(if enabled { format!("`{alias}` frozen") } else { format!("`{alias}` unfrozen") }.into())
        });
        Ok(())
    }

    pub fn delete_cheat(&mut self, alias: &str) -> Result<(), String> {
        let game = self.require_game()?;
        let alias = alias.to_string();
        self.run_action("delete cheat", Refresh::CHEATS, move |e| {
            e.store().delete_cheat(game, &alias).map_err(err_string)?;
            Ok(format!("Removed `{alias}` from the cheat table").into())
        });
        Ok(())
    }

    /// Toggles the game's disk trainer (freeze-on-save).
    pub fn set_trainer(&mut self, on: bool) -> Result<(), String> {
        let game = self.require_game()?;
        let refresh = Refresh { games: true, ..Refresh::NONE };
        self.run_action("trainer", refresh, move |e| {
            e.store().set_trainer_enabled(game, on).map_err(err_string)?;
            Ok(format!("Trainer (freeze-on-save) {}", if on { "enabled" } else { "disabled" }).into())
        });
        Ok(())
    }

    fn set_watch(&mut self, game: GameId, on: bool) {
        let refresh = Refresh { games: true, ..Refresh::NONE };
        self.run_action("watch", refresh, move |e| {
            e.store().set_watch_enabled(game, on).map_err(err_string)?;
            Ok(format!("Watching {}", if on { "enabled" } else { "disabled" }).into())
        });
    }

    // ------------------------------------------------------------------
    // Tree diff & cherry-pick
    // ------------------------------------------------------------------

    /// Sets the source revision of the diff view (and reloads).
    pub fn set_diff_source(&mut self, rev: &str) {
        self.diff.source_rev = rev.to_string();
        self.load_diff();
    }

    /// Loads both trees of the selected file in the background.
    pub fn load_diff(&mut self) {
        let Ok((game, rel)) = self.require_file() else {
            self.diff.data = None;
            self.diff.error = Some("select a tracked file".into());
            return;
        };
        self.diff.seq += 1;
        self.diff.loading = true;
        self.diff.pending_pick = None;
        let seq = self.diff.seq;
        let from = self.diff.source_rev.clone();
        self.submit(move |engine| {
            let result = (|| -> Result<DiffData, EngineError> {
                let left = engine.root_at(game, &from, &rel)?;
                let right = engine.root_at(game, WORKING, &rel)?;
                let files = engine.diff(game, &from, WORKING, None)?;
                Ok(DiffData {
                    rel: rel.clone(),
                    from_rev: from.clone(),
                    model: TreeDiffModel::build(left, right),
                    files,
                })
            })()
            .map_err(err_string);
            WorkerMsg::Diff { seq, result }
        });
    }

    /// Applies the search box to the diff trees.
    pub fn set_diff_search(&mut self, q: &str) {
        self.diff.search = q.to_string();
        self.refilter_diff();
    }

    pub(crate) fn refilter_diff(&mut self) {
        self.diff.applied_search = self.diff.search.clone();
        match &self.diff.data {
            Some(d) => {
                self.diff.visible_left = visible_paths(&d.model.left, &self.diff.search);
                self.diff.visible_right = visible_paths(&d.model.right, &self.diff.search);
            }
            None => {
                self.diff.visible_left = None;
                self.diff.visible_right = None;
            }
        }
    }

    /// Toggles the multi-selection of a left-tree section.
    pub fn toggle_pick_selection(&mut self, path: &NodePath) {
        let k = path.to_string();
        if !self.diff.selected.remove(&k) {
            self.diff.selected.insert(k);
        }
    }

    /// Paths currently multi-selected.
    pub fn pick_selection(&self) -> Vec<NodePath> {
        self.diff.selected.iter().filter_map(|s| NodePath::parse(s).ok()).collect()
    }

    /// Opens the cherry-pick confirmation with a preview of the changes.
    pub fn request_cherry_pick(&mut self, paths: Vec<NodePath>) {
        let Some(d) = &self.diff.data else {
            self.toasts.error("load the diff first");
            return;
        };
        if paths.is_empty() {
            self.toasts.warning("select at least one section");
            return;
        }
        self.diff.preview = d.model.preview_pick(&paths);
        self.diff.pending_pick = Some(paths);
    }

    pub fn cancel_cherry_pick(&mut self) {
        self.diff.pending_pick = None;
        self.diff.preview.clear();
    }

    /// Executes the pending cherry-pick.
    pub fn confirm_cherry_pick(&mut self) -> Result<(), String> {
        let paths = self.diff.pending_pick.take().ok_or_else(|| "no cherry-pick pending".to_string())?;
        self.diff.preview.clear();
        let d = self.diff.data.as_ref().ok_or_else(|| "load the diff first".to_string())?;
        let game = self.require_game()?;
        let (rel, from) = (d.rel.clone(), d.from_rev.clone());
        self.diff.selected.clear();
        let refresh = Refresh { history: true, cheats: true, trees: true, ..Refresh::NONE };
        self.run_action("cherry-pick", refresh, move |e| {
            let r = e.cherry_pick(game, &from, &rel, &paths, &CherryPickOptions::default()).map_err(err_string)?;
            let names: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
            if r.no_op {
                return Ok(format!("{} already identical to {from}", names.join(", ")).into());
            }
            Ok(ActionOk {
                message: format!(
                    "Cherry-picked {} from {from} ({} change(s)){}",
                    names.join(", "),
                    r.changes.len(),
                    r.safety_tag.as_ref().map(|t| format!(", safety backup {t}")).unwrap_or_default()
                ),
                safety_tag: r.safety_tag,
            })
        });
        Ok(())
    }

    // ------------------------------------------------------------------
    // AST editor
    // ------------------------------------------------------------------

    /// Loads the WORKING tree of the selected file.
    pub fn load_editor(&mut self) {
        let Ok((game, rel)) = self.require_file() else {
            self.editor.data = None;
            self.editor.error = Some("select a tracked file".into());
            return;
        };
        self.editor.seq += 1;
        self.editor.loading = true;
        let seq = self.editor.seq;
        self.submit(move |engine| {
            let result = engine
                .open_version(game, WORKING, &rel)
                .map(|o| {
                    let mode = o.edit_mode();
                    let gate_reason = match mode {
                        EditMode::Structural => None,
                        EditMode::RawInPlace => Some(format!(
                            "Safe Edit Gate: the {} codec does not reproduce this file bit for bit. Structural edits are blocked; raw in-place edits with checksum recalculation are allowed.",
                            o.codec_id()
                        )),
                        EditMode::BackupRestoreOnly => Some(format!(
                            "Safe Edit Gate: round-trip failed ({:?}). Only full backup/restore is allowed for this file.",
                            o.envelope().round_trip
                        )),
                    };
                    EditorData {
                        rel: rel.clone(),
                        root: o.root(),
                        edit_mode: mode,
                        gate_reason,
                    }
                })
                .map_err(err_string);
            WorkerMsg::Editor { seq, result }
        });
    }

    pub fn set_editor_search(&mut self, q: &str) {
        self.editor.search = q.to_string();
        self.refilter_editor();
    }

    pub(crate) fn refilter_editor(&mut self) {
        self.editor.applied_search = self.editor.search.clone();
        self.editor.visible = self.editor.data.as_ref().and_then(|d| visible_paths(&d.root, &self.editor.search));
    }

    /// Edits one scalar leaf of the WORKING file.
    pub fn edit_value(&mut self, path: NodePath, value: String) -> Result<(), String> {
        let (game, rel) = self.require_file()?;
        if let Some(d) = &self.editor.data {
            if d.edit_mode != EditMode::Structural {
                let reason =
                    d.gate_reason.clone().unwrap_or_else(|| "Safe Edit Gate: structural edits are blocked".into());
                self.editor.gate_error = Some(reason.clone());
                return Err(reason);
            }
        }
        self.editor.gate_error = None;
        self.editor.edits.remove(&path.to_string());
        let refresh = Refresh { history: true, cheats: true, trees: true, ..Refresh::NONE };
        self.run_action("edit", refresh, move |e| {
            let r = e
                .edit_value(game, &rel, &EditTarget::Path { path: path.clone(), value: value.clone() }, false)
                .map_err(|err| match err {
                    EngineError::GateBlocked(m) => format!("Safe Edit Gate: {m}"),
                    other => other.to_string(),
                })?;
            if r.no_op {
                return Ok(format!("{path} already {value}").into());
            }
            Ok(ActionOk {
                message: format!(
                    "{path} = {value}{}",
                    r.safety_tag.as_ref().map(|t| format!(" (safety backup {t})")).unwrap_or_default()
                ),
                safety_tag: r.safety_tag,
            })
        });
        Ok(())
    }

    /// Raw in-place write (offset/type/endianness of the payload).
    pub fn raw_write(&mut self) -> Result<(), String> {
        let (game, rel) = self.require_file()?;
        let off_text = self.editor.raw_offset.trim();
        let offset = match off_text.strip_prefix("0x").or_else(|| off_text.strip_prefix("0X")) {
            Some(h) => u64::from_str_radix(h, 16),
            None => off_text.parse::<u64>(),
        }
        .map_err(|e| format!("offset `{off_text}`: {e}"))?;
        let value = ScalarValue::parse(&self.editor.raw_value).map_err(|e| format!("value: {e}"))?;
        let location = Location::Raw {
            region: NodePath::root(),
            offset,
            ty: self.editor.raw_type,
            endian: self.editor.raw_endian,
        };
        let refresh = Refresh { history: true, cheats: true, trees: true, ..Refresh::NONE };
        self.run_action("raw write", refresh, move |e| {
            let r = e
                .edit_value(game, &rel, &EditTarget::Location { location: location.clone(), value }, true)
                .map_err(err_string)?;
            Ok(ActionOk {
                message: format!(
                    "Raw write {location} = {value}{}",
                    r.safety_tag.as_ref().map(|t| format!(" (safety backup {t})")).unwrap_or_default()
                ),
                safety_tag: r.safety_tag,
            })
        });
        Ok(())
    }

    // ------------------------------------------------------------------
    // Panic restore
    // ------------------------------------------------------------------

    /// Prepares the panic-restore confirmation (latest mutation, or the
    /// selected game's latest mutation when `game_scope`).
    pub fn request_panic(&mut self, game_scope: bool) -> Result<(), String> {
        let scope = match (game_scope, self.selected_game) {
            (true, Some(g)) => PanicScope::Game(g),
            _ => PanicScope::Latest,
        };
        let m = self
            .engine
            .store()
            .last_mutation(&scope)
            .map_err(err_string)?
            .ok_or_else(|| "Nothing to restore: SaveLens has not modified any save file yet".to_string())?;
        self.panic = Some(PanicPrompt {
            scope,
            tag: m.safety.tag,
            path: m.abs_path,
            reason: m.reason,
            created_at: m.created_at,
        });
        Ok(())
    }

    pub fn cancel_panic(&mut self) {
        self.panic = None;
    }

    /// Executes the confirmed panic restore.
    pub fn confirm_panic(&mut self) -> Result<(), String> {
        let p = self.panic.take().ok_or_else(|| "no panic restore pending".to_string())?;
        self.run_action("panic restore", Refresh::ALL, move |e| {
            let r = e.panic_restore(&p.scope).map_err(err_string)?;
            let hash: String = r.restored_hash.chars().take(12).collect();
            Ok(ActionOk {
                message: format!(
                    "PANIC RESTORE: {} reverted to {} (blake3 {hash}… verified)",
                    r.path.display(),
                    r.restored_tag
                ),
                safety_tag: Some(r.replaced_state_tag),
            })
        });
        Ok(())
    }

    // ------------------------------------------------------------------
    // Daemon
    // ------------------------------------------------------------------

    /// Starts or stops the embedded watcher daemon.
    pub fn toggle_daemon(&mut self) {
        match self.daemon.take() {
            Some(h) => {
                let spawned = std::thread::Builder::new().name("savelens-gui-daemon-stop".into()).spawn(move || {
                    if let Err(e) = h.stop() {
                        tracing::warn!("daemon stopped with an error: {e}");
                    }
                });
                match spawned {
                    Ok(_) => self.toasts.info("Watcher stopping…"),
                    Err(e) => self.toasts.error(format!("cannot stop the watcher thread cleanly: {e}")),
                }
                self.push_activity("daemon", "stopped", "embedded watcher stopped");
            }
            None => {
                let opts = DaemonOptions { hotkeys: self.daemon_hotkeys, ..DaemonOptions::default() };
                match Daemon::spawn(self.engine.clone(), opts) {
                    Ok(h) => {
                        self.daemon = Some(h);
                        self.toasts.success("Watcher started");
                    }
                    Err(e) => self.toasts.error(format!("Cannot start the watcher: {e}")),
                }
            }
        }
    }

    fn push_activity(&mut self, source: &'static str, kind: &str, message: &str) {
        self.activity.insert(
            0,
            ActivityEntry { at: savelens_cas::now_ms(), source, kind: kind.to_string(), message: message.to_string() },
        );
        self.activity.truncate(ACTIVITY_LIMIT);
    }

    fn poll_daemon(&mut self) {
        let mut events = Vec::new();
        if let Some(h) = &self.daemon {
            while let Ok(ev) = h.events().try_recv() {
                events.push(ev);
            }
        }
        for ev in events {
            self.handle_daemon_event(ev);
        }
    }

    fn handle_daemon_event(&mut self, ev: DaemonEvent) {
        let (kind, msg) = describe_daemon_event(&ev);
        self.push_activity("daemon", kind, &msg);
        match ev {
            DaemonEvent::Committed { game_id, .. } if Some(game_id) == self.selected_game => {
                self.toasts.info(msg);
                self.apply_refresh(Refresh { history: true, cheats: true, trees: true, ..Refresh::NONE });
            }
            DaemonEvent::TrainerApplied { .. } | DaemonEvent::Rollback { .. } | DaemonEvent::PanicRestored { .. } => {
                self.toasts.success(msg);
                self.apply_refresh(Refresh::ALL);
            }
            DaemonEvent::TrainerToggled { .. } => {
                self.toasts.info(msg);
                self.reload_games();
            }
            DaemonEvent::Snapshot { .. } => {
                self.toasts.info(msg);
                self.request_history();
            }
            DaemonEvent::Warning { .. } | DaemonEvent::Locked { .. } => self.toasts.warning(msg),
            DaemonEvent::Error { .. } => self.toasts.error(msg),
            DaemonEvent::Stopped => self.daemon = None,
            _ => {}
        }
    }

    // ------------------------------------------------------------------
    // Discovery, hunt, tracking
    // ------------------------------------------------------------------

    /// Starts Steam/Proton discovery in the background.
    pub fn start_discovery(&mut self) -> Result<(), String> {
        if self.sidebar.discover_job.is_some() {
            return Err("discovery is already running".into());
        }
        let engine = self.engine.clone();
        let job =
            Job::spawn("discovery", None, move || engine.discover(&DiscoveryConfig::from_env()).map_err(err_string))
                .map_err(err_string)?;
        self.sidebar.discover_job = Some(job);
        Ok(())
    }

    /// Starts the live process file hunter for `duration`.
    pub fn start_hunt(&mut self, duration: Duration) -> Result<(), String> {
        if self.sidebar.hunt_job.is_some() {
            return Err("a hunt is already running".into());
        }
        let report = self.sidebar.discovery.clone();
        let job = Job::spawn("auto-hunt", Some(duration), move || {
            let opts = HuntOptions { duration, ..HuntOptions::default() };
            savelens_discovery::hunter::hunt(&DiscoveryConfig::from_env(), report.as_ref(), &opts).map_err(err_string)
        })
        .map_err(err_string)?;
        self.sidebar.hunt_job = Some(job);
        Ok(())
    }

    /// Discovered games whose save locations are not tracked yet.
    pub fn untracked_discovered(&self) -> Vec<usize> {
        let Some(rep) = &self.sidebar.discovery else { return Vec::new() };
        rep.games
            .iter()
            .enumerate()
            .filter(|(_, g)| {
                !g.save_locations.iter().any(|l| {
                    let root = l.root.canonicalize().unwrap_or_else(|_| l.root.clone());
                    self.games.iter().any(|t| t.save_root == root)
                })
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn after_track_refresh() -> Refresh {
        Refresh::ALL
    }

    pub fn track_discovered(&mut self, game: usize, location: usize) -> Result<(), String> {
        let g = self
            .sidebar
            .discovery
            .as_ref()
            .and_then(|r| r.games.get(game))
            .cloned()
            .ok_or_else(|| "unknown discovered game".to_string())?;
        self.run_action("track", Self::after_track_refresh(), move |e| {
            let (rec, commit) = e.track_discovered(&g, location).map_err(err_string)?;
            Ok(format!(
                "Tracking {} at {}{}",
                rec.name,
                rec.save_root.display(),
                commit.map(|c| format!(" (import #{})", c.id)).unwrap_or_default()
            )
            .into())
        });
        Ok(())
    }

    /// Tracks the directory of hunt candidate `index`.
    pub fn track_candidate(&mut self, index: usize) -> Result<(), String> {
        let c = self
            .sidebar
            .hunt
            .as_ref()
            .and_then(|r| r.candidates.get(index))
            .cloned()
            .ok_or_else(|| "unknown hunt candidate".to_string())?;
        let (name, root, pattern) = candidate_track_params(&c.path, c.process_name.as_deref())?;
        self.track_folder(&name, &root, &pattern)
    }

    /// Tracks a folder with comma-separated glob patterns.
    pub fn track_folder(&mut self, name: &str, dir: &Path, patterns: &str) -> Result<(), String> {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err("enter a game name".into());
        }
        if !dir.is_dir() {
            return Err(format!("{} is not a directory", dir.display()));
        }
        let pats: Vec<String> = patterns.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
        let dir = dir.to_path_buf();
        self.run_action("track", Self::after_track_refresh(), move |e| {
            let (rec, commit) = e.track(&name, None, &dir, &pats, "native").map_err(err_string)?;
            Ok(format!(
                "Tracking {} at {}{}",
                rec.name,
                rec.save_root.display(),
                commit.map(|c| format!(" (import #{})", c.id)).unwrap_or_default()
            )
            .into())
        });
        Ok(())
    }

    /// Game id of the most recently tracked game with that name.
    pub fn game_by_name(&self, name: &str) -> Option<GameId> {
        self.games.iter().filter(|g| g.name == name).map(|g| g.id).max()
    }
}

/// Name, root and pattern used to track a hunted file.
pub fn candidate_track_params(path: &Path, process: Option<&str>) -> Result<(String, PathBuf, String), String> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let pattern = match path.extension().and_then(|e| e.to_str()) {
        Some(ext) if !ext.is_empty() => format!("*.{ext}"),
        _ => path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .ok_or_else(|| format!("{} has no file name", path.display()))?,
    };
    let name = process
        .map(str::to_string)
        .filter(|p| !p.is_empty())
        .or_else(|| parent.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "Hunted game".to_string());
    Ok((name, parent.to_path_buf(), pattern))
}

/// Kind and human message of a daemon event.
pub fn describe_daemon_event(ev: &DaemonEvent) -> (&'static str, String) {
    match ev {
        DaemonEvent::Started { games, hotkeys } => (
            "started",
            format!(
                "watcher started on {games} game(s){}",
                if hotkeys.is_empty() { String::new() } else { format!(", hotkeys {}", hotkeys.join(", ")) }
            ),
        ),
        DaemonEvent::Watching { game, root } => ("watching", format!("watching {game} at {}", root.display())),
        DaemonEvent::Unwatched { game } => ("unwatched", format!("stopped watching {game}")),
        DaemonEvent::Committed { game, rel_path, commit, summary, .. } => {
            ("commit", format!("{game}: {rel_path} saved ➡ #{commit} {summary}"))
        }
        DaemonEvent::Unchanged { game, rel_path } => {
            ("unchanged", format!("{game}: {rel_path} rewritten without changes"))
        }
        DaemonEvent::TrainerApplied { game, rel_path, applied, elapsed_ms } => {
            ("trainer", format!("{game}: froze {} in {rel_path} ({elapsed_ms:.1} ms)", applied.join(", ")))
        }
        DaemonEvent::Locked { path } => ("locked", format!("{} is held open for writing; waiting", path.display())),
        DaemonEvent::PanicRestored { path, tag } => ("panic", format!("panic restore of {} to {tag}", path.display())),
        DaemonEvent::Snapshot { game, commit } => (
            "snapshot",
            match commit {
                Some(c) => format!("{game}: snapshot #{c}"),
                None => format!("{game}: snapshot (no changes)"),
            },
        ),
        DaemonEvent::Rollback { game, files } => ("rollback", format!("{game}: rolled back {}", files.join(", "))),
        DaemonEvent::TrainerToggled { game, enabled } => {
            ("trainer", format!("{game}: trainer {}", if *enabled { "enabled" } else { "disabled" }))
        }
        DaemonEvent::OverlayToggleRequested => ("overlay", "overlay toggle requested".to_string()),
        DaemonEvent::Warning { message } => ("warning", message.clone()),
        DaemonEvent::Error { message } => ("error", message.clone()),
        DaemonEvent::Stopped => ("stopped", "watcher stopped".to_string()),
    }
}

fn load_history(engine: &Engine, game: GameId) -> Result<HistoryData, String> {
    let store = engine.store();
    let g = store.game(game).map_err(err_string)?;
    let mut commits = store.all_commits(game).map_err(err_string)?;
    commits.truncate(TIMELINE_LIMIT);
    let branches = store.branches(game).map_err(err_string)?;
    let tags = store.tags(game).map_err(err_string)?;
    let mut changelogs = HashMap::new();
    for c in &commits {
        changelogs.insert(c.id, store.changelogs(c.id).map_err(err_string)?);
    }
    let layout = layout(&commits, &branches, &g.current_branch);
    Ok(HistoryData { game, commits, branches, tags, changelogs, layout, current_branch: g.current_branch })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunt_candidate_params() {
        let (n, r, p) = candidate_track_params(Path::new("/games/Foo/Saves/slot1.sav"), None).unwrap();
        assert_eq!((n.as_str(), r, p.as_str()), ("Saves", PathBuf::from("/games/Foo/Saves"), "*.sav"));
        let (n, _, p) = candidate_track_params(Path::new("/g/SAVEDATA"), Some("game.exe")).unwrap();
        assert_eq!((n.as_str(), p.as_str()), ("game.exe", "SAVEDATA"));
        assert!(candidate_track_params(Path::new("x"), None).is_err());
    }

    #[test]
    fn daemon_events_are_described() {
        let (k, m) = describe_daemon_event(&DaemonEvent::Committed {
            game_id: 1,
            game: "RPG".into(),
            rel_path: "a.json".into(),
            commit: 3,
            summary: "+300 Gold".into(),
        });
        assert_eq!(k, "commit");
        assert!(m.contains("#3") && m.contains("+300 Gold"));
        assert_eq!(describe_daemon_event(&DaemonEvent::Stopped).0, "stopped");
    }
}
