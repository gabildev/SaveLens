//! Application state, global key handling, background-job plumbing and the
//! frame layout shared by every tab.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use savelens_cas::{
    BranchRecord, Changelog, CommitId, CommitKind, CommitRecord, DaemonStatus, EventRecord, GameId, GameRecord,
    PanicScope, StoreStats, TagRecord,
};
use savelens_core::{Alignment as ScanAlign, Endianness, Location, NodePath, ScanConfig, ScanSession, ScanType};
use savelens_discovery::{DiscoveredGame, DiscoveryConfig};
use savelens_watcher::{CheatValue, Daemon, DaemonEvent, DaemonHandle, DaemonOptions, Engine, TrackedFile, WORKING};

use crate::jobs::{JobKind, JobOutput, Jobs, LoadedTree};
use crate::theme;
use crate::tree::TreeRow;
use crate::widgets::{self, InputEvent, Level, TextInput, Toasts};

/// The four main tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Games,
    Timeline,
    Scanner,
    Ast,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Games, Tab::Timeline, Tab::Scanner, Tab::Ast];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Games => "Games",
            Tab::Timeline => "Timeline",
            Tab::Scanner => "Scanner",
            Tab::Ast => "AST Explorer",
        }
    }

    pub fn index(self) -> usize {
        match self {
            Tab::Games => 0,
            Tab::Timeline => 1,
            Tab::Scanner => 2,
            Tab::Ast => 3,
        }
    }
}

/// A tracked game with cached statistics.
#[derive(Debug, Clone)]
pub(crate) struct GameRow {
    pub rec: GameRecord,
    pub files: usize,
    pub logical: u64,
}

/// Timeline data of the current game.
#[derive(Debug, Default)]
pub(crate) struct TimelineState {
    pub commits: Vec<CommitRecord>,
    pub sel: usize,
    pub branches: Vec<BranchRecord>,
    pub tags: Vec<TagRecord>,
    pub changelogs: HashMap<CommitId, Vec<Changelog>>,
}

/// Which table of the scanner has the focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanPane {
    Candidates,
    Cheats,
}

/// Byte-order choice of the scanner options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EndianChoice {
    Little,
    Big,
    Both,
}

/// Scanner state.
pub(crate) struct ScannerState {
    pub config: ScanConfig,
    pub endian: EndianChoice,
    /// Scan the whole decoded payload as raw bytes instead of the AST.
    pub raw_payload: bool,
    pub session: ScanSession,
    pub filter: String,
    pub source: String,
    pub cand_sel: usize,
    pub pane: ScanPane,
    pub cheat_sel: usize,
    pub cheats: Vec<CheatValue>,
    pub snapshot: Option<savelens_core::Node>,
    pub last_error: Option<String>,
    /// `(game, file)` the session was run on.
    pub scope: Option<(GameId, String)>,
}

impl ScannerState {
    fn new() -> Self {
        let config = ScanConfig::default();
        ScannerState {
            session: ScanSession::new(config.clone()),
            config,
            endian: EndianChoice::Little,
            raw_payload: false,
            filter: String::new(),
            source: WORKING.to_string(),
            cand_sel: 0,
            pane: ScanPane::Candidates,
            cheat_sel: 0,
            cheats: Vec::new(),
            snapshot: None,
            last_error: None,
            scope: None,
        }
    }

    /// Rebuilds the configuration's endianness list from the choice.
    pub fn apply_endian(&mut self) {
        self.config.endians = match self.endian {
            EndianChoice::Little => vec![Endianness::Little],
            EndianChoice::Big => vec![Endianness::Big],
            EndianChoice::Both => vec![Endianness::Little, Endianness::Big],
        };
    }

    /// Starts over with the current configuration.
    pub fn reset(&mut self) {
        self.session = ScanSession::new(self.config.clone());
        self.cand_sel = 0;
        self.snapshot = None;
        self.last_error = None;
        self.scope = None;
    }
}

/// AST explorer state.
#[derive(Default)]
pub(crate) struct AstState {
    pub rev: String,
    pub tree: Option<LoadedTree>,
    pub loaded: Option<(GameId, String, String)>,
    pub rows: Vec<TreeRow>,
    pub expanded: HashSet<String>,
    pub sel: usize,
    pub marked: Vec<NodePath>,
    pub search: String,
    pub matches: Vec<NodePath>,
    pub match_idx: usize,
    pub error: Option<String>,
}

/// What a text prompt is for.
#[derive(Debug, Clone)]
pub(crate) enum InputPurpose {
    Command,
    NewBranch { from: CommitId },
    Tag { commit: CommitId },
    PinAlias { location: Location, freeze: bool },
    EditCandidate { location: Location },
    EditCheat { alias: String },
    ScanFilter,
    AstSearch,
    AstEdit { path: NodePath },
}

/// A text prompt.
#[derive(Debug, Clone)]
pub(crate) struct Prompt {
    pub purpose: InputPurpose,
    pub title: String,
    pub help: String,
    pub input: TextInput,
}

/// What a confirmation dialog does when accepted.
#[derive(Debug, Clone)]
pub(crate) enum Action {
    Quit,
    PanicRestore,
    Checkout { rev: String },
    CherryPick { rev: String, rel: String, paths: Vec<NodePath> },
    DeleteCheat { alias: String },
}

/// A confirmation dialog.
#[derive(Debug, Clone)]
pub(crate) struct Confirm {
    pub title: String,
    pub lines: Vec<Line<'static>>,
    pub action: Action,
    pub danger: bool,
}

/// What a picker selects.
#[derive(Debug, Clone)]
pub(crate) enum PickPurpose {
    SwitchBranch,
    File,
    ScanSource,
    AstRev,
    TrackLocation { game: Box<DiscoveredGame> },
}

/// One entry of a picker.
#[derive(Debug, Clone)]
pub(crate) struct PickItem {
    pub label: String,
    pub detail: String,
    pub value: String,
}

/// A list picker.
#[derive(Debug, Clone)]
pub(crate) struct Picker {
    pub title: String,
    pub items: Vec<PickItem>,
    pub sel: usize,
    pub purpose: PickPurpose,
}

/// Modal state on top of the tabs.
#[derive(Debug, Clone, Default)]
pub(crate) enum Mode {
    #[default]
    Normal,
    Help {
        scroll: u16,
    },
    Input(Prompt),
    Confirm(Confirm),
    Picker(Picker),
    Viewer {
        title: String,
        lines: Vec<Line<'static>>,
        scroll: u16,
    },
    ScanOptions {
        sel: usize,
    },
}

/// The SaveLens terminal application.
pub struct App {
    pub(crate) engine: Engine,
    pub(crate) tab: Tab,
    pub(crate) mode: Mode,
    pub(crate) toasts: Toasts,
    pub(crate) jobs: Jobs,
    pub(crate) discovery_cfg: DiscoveryConfig,
    pub(crate) games: Vec<GameRow>,
    pub(crate) discovered: Vec<DiscoveredGame>,
    pub(crate) discovery_ran: bool,
    pub(crate) games_sel: usize,
    pub(crate) game: Option<GameId>,
    pub(crate) stats: StoreStats,
    pub(crate) events: Vec<EventRecord>,
    pub(crate) daemon: Option<DaemonHandle>,
    pub(crate) daemon_since: Option<Instant>,
    pub(crate) daemon_hotkeys: Vec<String>,
    pub(crate) daemon_status: Option<DaemonStatus>,
    pub(crate) timeline: TimelineState,
    pub(crate) files: Vec<TrackedFile>,
    pub(crate) file: Option<String>,
    pub(crate) scanner: ScannerState,
    pub(crate) ast: AstState,
    pub(crate) scanner_page: usize,
    pub(crate) ast_offset: usize,
    pub(crate) frame: u64,
    last_refresh: Instant,
    quit: bool,
}

/// How often cached data is refreshed from the store.
const REFRESH_EVERY: Duration = Duration::from_secs(2);

impl App {
    /// Creates the app and loads the initial state from the store.
    pub fn new(engine: Engine) -> Result<App> {
        let mut app = App {
            engine,
            tab: Tab::Games,
            mode: Mode::Normal,
            toasts: Toasts::default(),
            jobs: Jobs::default(),
            discovery_cfg: DiscoveryConfig::from_env(),
            games: Vec::new(),
            discovered: Vec::new(),
            discovery_ran: false,
            games_sel: 0,
            game: None,
            stats: StoreStats::default(),
            events: Vec::new(),
            daemon: None,
            daemon_since: None,
            daemon_hotkeys: Vec::new(),
            daemon_status: None,
            timeline: TimelineState::default(),
            files: Vec::new(),
            file: None,
            scanner: ScannerState::new(),
            ast: AstState { rev: WORKING.to_string(), ..AstState::default() },
            scanner_page: 10,
            ast_offset: 0,
            frame: 0,
            last_refresh: Instant::now(),
            quit: false,
        };
        app.refresh_global().context("loading games from the store")?;
        app.game = app.games.first().map(|g| g.rec.id);
        app.refresh_game().context("loading the selected game")?;
        if app.games.is_empty() {
            app.toast(Level::Info, "No games tracked yet: press d to discover games or :track <name> <dir>");
        }
        Ok(app)
    }

    /// Replaces the discovery configuration (defaults to the environment).
    pub fn set_discovery_config(&mut self, cfg: DiscoveryConfig) {
        self.discovery_cfg = cfg;
    }

    /// The engine.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// The active tab.
    pub fn tab(&self) -> Tab {
        self.tab
    }

    /// The selected game.
    pub fn current_game(&self) -> Option<GameId> {
        self.game
    }

    /// True while a background job (discovery, scan, parsing) runs.
    pub fn is_busy(&self) -> bool {
        self.jobs.busy()
    }

    /// True while the embedded watcher daemon runs.
    pub fn daemon_running(&self) -> bool {
        self.daemon.is_some()
    }

    /// Every notification shown so far (oldest first).
    pub fn messages(&self) -> Vec<String> {
        self.toasts.all().iter().map(|t| t.text.clone()).collect()
    }

    /// Ticks until no background job runs or `timeout` elapses. Returns
    /// whether the app is idle.
    pub fn wait_idle(&mut self, timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            self.tick();
            if !self.jobs.busy() {
                return true;
            }
            if start.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Stops the embedded daemon (if any).
    pub fn shutdown(&mut self) {
        if let Some(d) = self.daemon.take() {
            if let Err(e) = d.stop() {
                tracing::warn!("daemon stop: {e}");
            }
        }
        self.daemon_since = None;
    }

    pub(crate) fn toast(&mut self, level: Level, text: impl Into<String>) {
        self.toasts.push(level, text);
    }

    pub(crate) fn toast_err(&mut self, e: &anyhow::Error) {
        let text = format!("{e:#}");
        self.toasts.push(Level::Error, text);
    }

    /// The selected game record.
    pub(crate) fn game_rec(&self) -> Option<&GameRecord> {
        let id = self.game?;
        self.games.iter().find(|g| g.rec.id == id).map(|g| &g.rec)
    }

    pub(crate) fn require_game(&self) -> Result<GameRecord> {
        self.game_rec().cloned().ok_or_else(|| anyhow!("no game selected: track or select a game on the Games tab (1)"))
    }

    pub(crate) fn require_file(&self) -> Result<String> {
        self.file.clone().ok_or_else(|| anyhow!("the selected game has no tracked file: press f to pick one"))
    }

    // ------------------------------------------------------------------
    // Data refresh
    // ------------------------------------------------------------------

    /// Games, storage stats, activity log and daemon heartbeat.
    pub(crate) fn refresh_global(&mut self) -> Result<()> {
        let store = self.engine.store();
        let mut rows = Vec::new();
        for rec in store.games()? {
            let files = self.engine.tracked_files(&rec).map(|f| f.len()).unwrap_or(0);
            let logical = store.game_logical_bytes(rec.id)?;
            rows.push(GameRow { rec, files, logical });
        }
        self.games = rows;
        self.stats = store.stats()?;
        self.events = store.events(None, 60)?;
        self.daemon_status = store.daemon_status(15_000)?;
        if let Some(id) = self.game {
            if !self.games.iter().any(|g| g.rec.id == id) {
                self.game = self.games.first().map(|g| g.rec.id);
            }
        }
        let total = self.games.len() + self.untracked().len();
        self.games_sel = self.games_sel.min(total.saturating_sub(1));
        self.last_refresh = Instant::now();
        Ok(())
    }

    /// Commits, branches, tags, files and cheat table of the selected game.
    pub(crate) fn refresh_game(&mut self) -> Result<()> {
        let Some(game) = self.game_rec().cloned() else {
            self.timeline = TimelineState::default();
            self.files.clear();
            self.file = None;
            self.scanner.cheats.clear();
            return Ok(());
        };
        let store = self.engine.store();
        let commits = self.engine.log(game.id, Some(&game.current_branch), 1000)?;
        let head_changed = self.timeline.commits.first().map(|c| c.id) != commits.first().map(|c| c.id);
        if head_changed && self.timeline.sel != 0 {
            // Keep the cursor on the same commit when new commits arrive.
            if let Some(cur) = self.timeline.commits.get(self.timeline.sel).map(|c| c.id) {
                self.timeline.sel = commits.iter().position(|c| c.id == cur).unwrap_or(0);
            }
        }
        self.timeline.commits = commits;
        self.timeline.sel = self.timeline.sel.min(self.timeline.commits.len().saturating_sub(1));
        self.timeline.branches = store.branches(game.id)?;
        self.timeline.tags = store.tags(game.id)?;
        self.files = self.engine.tracked_files(&game)?;
        let still_there = self.file.as_ref().is_some_and(|f| self.files.iter().any(|t| &t.rel_path == f));
        if !still_there {
            self.set_file(self.files.first().map(|f| f.rel_path.clone()));
        }
        self.refresh_cheats();
        Ok(())
    }

    pub(crate) fn refresh_cheats(&mut self) {
        let Some(id) = self.game else {
            self.scanner.cheats.clear();
            return;
        };
        match self.engine.cheat_values(id) {
            Ok(c) => {
                self.scanner.cheats = c;
                self.scanner.cheat_sel = self.scanner.cheat_sel.min(self.scanner.cheats.len().saturating_sub(1));
            }
            Err(e) => self.scanner.last_error = Some(e.to_string()),
        }
    }

    /// Refreshes everything; errors become toasts.
    pub(crate) fn refresh(&mut self) {
        if let Err(e) = self.refresh_global().and_then(|_| self.refresh_game()) {
            self.toast_err(&e);
        }
        self.ast_ensure_loaded();
    }

    /// Selects a game (resets per-game state).
    pub(crate) fn select_game(&mut self, id: GameId) {
        if self.game == Some(id) {
            return;
        }
        self.game = Some(id);
        self.timeline = TimelineState::default();
        self.file = None;
        self.scanner.reset();
        self.scanner.source = WORKING.to_string();
        self.ast.rev = WORKING.to_string();
        self.ast.marked.clear();
        self.refresh();
    }

    /// Selects the file used by the scanner and the AST explorer.
    pub(crate) fn set_file(&mut self, file: Option<String>) {
        if self.file == file {
            return;
        }
        self.file = file;
        self.scanner.reset();
        self.ast.marked.clear();
        self.ast.matches.clear();
        self.ast.loaded = None;
        self.ast.tree = None;
        self.ast.rows.clear();
        self.ast.expanded.clear();
        self.ast.sel = 0;
    }

    /// Games found by discovery that are not tracked yet.
    pub(crate) fn untracked(&self) -> Vec<&DiscoveredGame> {
        self.discovered
            .iter()
            .filter(|d| {
                !self.games.iter().any(|g| {
                    (d.app_id.is_some() && g.rec.app_id == d.app_id)
                        || d.save_locations.iter().any(|l| {
                            l.root == g.rec.save_root || l.root.canonicalize().ok().as_ref() == Some(&g.rec.save_root)
                        })
                })
            })
            .collect()
    }

    // ------------------------------------------------------------------
    // Tick: jobs, daemon events, periodic refresh
    // ------------------------------------------------------------------

    /// Pulls finished jobs and daemon events and refreshes cached data.
    pub fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        for out in self.jobs.poll() {
            self.on_job(out);
        }
        let mut needs_refresh = false;
        let mut disconnected = false;
        let mut events = Vec::new();
        if let Some(d) = &self.daemon {
            loop {
                match d.events().try_recv() {
                    Ok(ev) => events.push(ev),
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        for ev in events {
            needs_refresh |= self.on_daemon_event(ev);
        }
        if disconnected && self.daemon.is_some() {
            self.daemon = None;
            self.daemon_since = None;
            self.toast(Level::Error, "watcher daemon exited unexpectedly");
            needs_refresh = true;
        }
        if needs_refresh || self.last_refresh.elapsed() >= REFRESH_EVERY {
            let periodic = !needs_refresh;
            match self.refresh_global().and_then(|_| self.refresh_game()) {
                Ok(()) => {}
                // Periodic refresh errors are transient (e.g. a busy database): log only.
                Err(e) if periodic => tracing::warn!("refresh: {e:#}"),
                Err(e) => self.toast_err(&e),
            }
            if needs_refresh {
                self.ast_reload_if_working();
            }
        }
    }

    fn on_daemon_event(&mut self, ev: DaemonEvent) -> bool {
        match ev {
            DaemonEvent::Started { games, hotkeys } => {
                self.daemon_hotkeys = hotkeys;
                self.toast(Level::Success, format!("Watcher started: watching {games} game(s)"));
                true
            }
            DaemonEvent::Watching { game, root } => {
                self.toast(Level::Info, format!("Watching {game} at {}", root.display()));
                false
            }
            DaemonEvent::Unwatched { game } => {
                self.toast(Level::Info, format!("Stopped watching {game}"));
                false
            }
            DaemonEvent::Committed { game, rel_path, commit, summary, .. } => {
                self.toast(Level::Success, format!("{game}: auto-commit #{commit} {rel_path}: {summary}"));
                true
            }
            DaemonEvent::Unchanged { .. } => false,
            DaemonEvent::TrainerApplied { game, rel_path, applied, elapsed_ms } => {
                self.toast(
                    Level::Success,
                    format!("{game}: trainer froze {} in {rel_path} ({elapsed_ms:.1} ms)", applied.join(", ")),
                );
                true
            }
            DaemonEvent::Locked { path } => {
                self.toast(
                    Level::Warning,
                    format!("{} stayed locked by the game; snapshotted read-only", path.display()),
                );
                false
            }
            DaemonEvent::PanicRestored { path, tag } => {
                self.toast(Level::Warning, format!("Panic restore (hotkey): {} → {tag}", path.display()));
                true
            }
            DaemonEvent::Snapshot { game, commit } => {
                match commit {
                    Some(c) => self.toast(Level::Success, format!("{game}: quick snapshot #{c}")),
                    None => self.toast(Level::Info, format!("{game}: quick snapshot, nothing changed")),
                }
                true
            }
            DaemonEvent::Rollback { game, files } => {
                self.toast(Level::Warning, format!("{game}: rolled back {} file(s)", files.len()));
                true
            }
            DaemonEvent::TrainerToggled { game, enabled } => {
                self.toast(
                    Level::Info,
                    format!("{game}: disk trainer {}", if enabled { "enabled" } else { "disabled" }),
                );
                true
            }
            DaemonEvent::OverlayToggleRequested => {
                self.toast(Level::Info, "Overlay toggle requested");
                false
            }
            DaemonEvent::Warning { message } => {
                self.toast(Level::Warning, message);
                false
            }
            DaemonEvent::Error { message } => {
                self.toast(Level::Error, message);
                false
            }
            DaemonEvent::Stopped => {
                self.toast(Level::Info, "Watcher stopped");
                true
            }
        }
    }

    fn on_job(&mut self, out: JobOutput) {
        match out {
            JobOutput::Discovery(res) => self.on_discovery(res),
            JobOutput::Scan { session, snapshot, result, label } => {
                self.on_scan_done(*session, snapshot.map(|b| *b), result, label)
            }
            JobOutput::AstLoad { key, result } => self.on_ast_loaded(key, result),
            JobOutput::Failed { kind, message } => {
                if kind == JobKind::Scan {
                    self.scanner.reset();
                }
                self.toast(Level::Error, format!("{kind:?} failed: {message}"));
            }
        }
    }

    // ------------------------------------------------------------------
    // Daemon
    // ------------------------------------------------------------------

    pub(crate) fn toggle_daemon(&mut self) -> Result<()> {
        if self.daemon.is_some() {
            self.shutdown();
            self.toast(Level::Info, "Watcher stopped");
            self.refresh();
            return Ok(());
        }
        if self.daemon_status.is_some() {
            self.toast(
                Level::Warning,
                "Another SaveLens watcher is already running (heartbeat found); starting a second one anyway",
            );
        }
        let hotkeys = match hotkey_backend() {
            Ok(()) => true,
            Err(reason) => {
                self.toast(
                    Level::Warning,
                    format!("Global hotkeys unavailable: {reason}; the watcher runs without them"),
                );
                false
            }
        };
        let opts = DaemonOptions { hotkeys, ..DaemonOptions::default() };
        let handle = Daemon::spawn(self.engine.clone(), opts).context("starting the watcher daemon")?;
        self.daemon = Some(handle);
        self.daemon_since = Some(Instant::now());
        self.toast(Level::Info, "Starting watcher daemon…");
        Ok(())
    }

    // ------------------------------------------------------------------
    // Keys
    // ------------------------------------------------------------------

    /// Handles a key press. Returns `true` when the app should quit.
    pub fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.kind == KeyEventKind::Release {
            return Ok(self.quit);
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.shutdown();
            self.quit = true;
            return Ok(true);
        }
        let mode = std::mem::take(&mut self.mode);
        let res = match mode {
            Mode::Normal => self.key_normal(key),
            Mode::Help { scroll } => {
                self.key_help(key, scroll);
                Ok(())
            }
            Mode::Input(p) => self.key_input(p, key),
            Mode::Confirm(c) => self.key_confirm(c, key),
            Mode::Picker(p) => self.key_picker(p, key),
            Mode::Viewer { title, lines, scroll } => {
                self.key_viewer(title, lines, scroll, key);
                Ok(())
            }
            Mode::ScanOptions { sel } => {
                self.key_scan_options(sel, key);
                Ok(())
            }
        };
        if let Err(e) = res {
            self.toast_err(&e);
        }
        Ok(self.quit)
    }

    fn key_normal(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('?') => self.mode = Mode::Help { scroll: 0 },
            KeyCode::Char(':') => self.prompt(
                InputPurpose::Command,
                "Command",
                "",
                "track <name> <dir> [pattern…] · snapshot [msg] · branch <name> · checkout <rev> · tag <name> · gc",
            ),
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Char('P') => self.panic_prompt()?,
            KeyCode::Char('w') => self.toggle_daemon()?,
            KeyCode::Char(c @ '1'..='4') => {
                let idx = (c as usize) - ('1' as usize);
                self.switch_tab(Tab::ALL[idx]);
            }
            KeyCode::Tab => self.switch_tab(Tab::ALL[(self.tab.index() + 1) % Tab::ALL.len()]),
            KeyCode::BackTab => self.switch_tab(Tab::ALL[(self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len()]),
            _ => match self.tab {
                Tab::Games => self.key_games(key)?,
                Tab::Timeline => self.key_timeline(key)?,
                Tab::Scanner => self.key_scanner(key)?,
                Tab::Ast => self.key_ast(key)?,
            },
        }
        Ok(())
    }

    pub(crate) fn switch_tab(&mut self, tab: Tab) {
        self.tab = tab;
        match tab {
            Tab::Ast => self.ast_ensure_loaded(),
            Tab::Scanner => self.refresh_cheats(),
            Tab::Games | Tab::Timeline => {}
        }
    }

    fn request_quit(&mut self) {
        if self.daemon.is_some() {
            self.mode = Mode::Confirm(Confirm {
                title: "Quit SaveLens?".into(),
                lines: vec![
                    Line::from("The embedded watcher daemon is running."),
                    Line::from("Quitting stops it: saves will no longer be auto-committed"),
                    Line::from("and freeze-on-save rules will not be applied."),
                ],
                action: Action::Quit,
                danger: false,
            });
        } else {
            self.quit = true;
        }
    }

    /// Opens a text prompt.
    pub(crate) fn prompt(&mut self, purpose: InputPurpose, title: &str, initial: &str, help: &str) {
        self.mode = Mode::Input(Prompt {
            purpose,
            title: title.to_string(),
            help: help.to_string(),
            input: TextInput::new(initial),
        });
    }

    /// Opens a confirmation dialog.
    pub(crate) fn confirm(&mut self, title: &str, lines: Vec<Line<'static>>, action: Action, danger: bool) {
        self.mode = Mode::Confirm(Confirm { title: title.to_string(), lines, action, danger });
    }

    fn key_help(&mut self, key: KeyEvent, scroll: u16) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.mode = Mode::Help { scroll: scroll.saturating_add(1) },
            KeyCode::Char('k') | KeyCode::Up => self.mode = Mode::Help { scroll: scroll.saturating_sub(1) },
            KeyCode::PageDown => self.mode = Mode::Help { scroll: scroll.saturating_add(10) },
            KeyCode::PageUp => self.mode = Mode::Help { scroll: scroll.saturating_sub(10) },
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter => {}
            _ => self.mode = Mode::Help { scroll },
        }
    }

    fn key_viewer(&mut self, title: String, lines: Vec<Line<'static>>, scroll: u16, key: KeyEvent) {
        let max = lines.len().saturating_sub(1) as u16;
        let scroll = match key.code {
            KeyCode::Char('j') | KeyCode::Down => scroll.saturating_add(1).min(max),
            KeyCode::Char('k') | KeyCode::Up => scroll.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => scroll.saturating_add(10).min(max),
            KeyCode::PageUp => scroll.saturating_sub(10),
            KeyCode::Char('g') | KeyCode::Home => 0,
            KeyCode::Char('G') | KeyCode::End => max,
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => return,
            _ => scroll,
        };
        self.mode = Mode::Viewer { title, lines, scroll };
    }

    fn key_input(&mut self, mut p: Prompt, key: KeyEvent) -> Result<()> {
        match p.input.handle(key) {
            InputEvent::Editing => {
                self.mode = Mode::Input(p);
                Ok(())
            }
            InputEvent::Cancel => Ok(()),
            InputEvent::Submit => {
                let value = p.input.value().to_string();
                self.submit_input(p.purpose, value)
            }
        }
    }

    fn submit_input(&mut self, purpose: InputPurpose, value: String) -> Result<()> {
        match purpose {
            InputPurpose::Command => self.run_command(&value),
            InputPurpose::NewBranch { from } => self.timeline_create_branch(&value, from),
            InputPurpose::Tag { commit } => self.timeline_tag(&value, commit),
            InputPurpose::PinAlias { location, freeze } => self.scanner_pin(&value, location, freeze),
            InputPurpose::EditCandidate { location } => self.scanner_edit_location(&value, location),
            InputPurpose::EditCheat { alias } => self.scanner_edit_cheat(&alias, &value),
            InputPurpose::ScanFilter => {
                self.scanner.filter = value.trim().to_string();
                self.scanner_run()
            }
            InputPurpose::AstSearch => {
                self.ast_search(&value);
                Ok(())
            }
            InputPurpose::AstEdit { path } => self.ast_edit(path, &value),
        }
    }

    fn key_confirm(&mut self, c: Confirm, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.run_action(c.action),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                self.toast(Level::Info, "Cancelled");
                Ok(())
            }
            _ => {
                self.mode = Mode::Confirm(c);
                Ok(())
            }
        }
    }

    fn run_action(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Quit => {
                self.shutdown();
                self.quit = true;
                Ok(())
            }
            Action::PanicRestore => self.panic_restore(),
            Action::Checkout { rev } => self.timeline_checkout(&rev),
            Action::CherryPick { rev, rel, paths } => self.ast_cherry_pick(&rev, &rel, &paths),
            Action::DeleteCheat { alias } => self.scanner_delete_cheat(&alias),
        }
    }

    fn key_picker(&mut self, mut p: Picker, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => p.sel = widgets::step(p.sel, p.items.len(), 1),
            KeyCode::Char('k') | KeyCode::Up => p.sel = widgets::step(p.sel, p.items.len(), -1),
            KeyCode::Char('g') | KeyCode::Home => p.sel = 0,
            KeyCode::Char('G') | KeyCode::End => p.sel = p.items.len().saturating_sub(1),
            KeyCode::PageDown => p.sel = widgets::step(p.sel, p.items.len(), 10),
            KeyCode::PageUp => p.sel = widgets::step(p.sel, p.items.len(), -10),
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Enter => {
                let Some(item) = p.items.get(p.sel).cloned() else {
                    return Ok(());
                };
                return self.picked(p.purpose, item);
            }
            _ => {}
        }
        self.mode = Mode::Picker(p);
        Ok(())
    }

    fn picked(&mut self, purpose: PickPurpose, item: PickItem) -> Result<()> {
        match purpose {
            PickPurpose::SwitchBranch => self.timeline_switch_branch(&item.value),
            PickPurpose::File => {
                self.set_file(Some(item.value.clone()));
                self.ast_ensure_loaded();
                self.toast(Level::Info, format!("File: {}", item.value));
                Ok(())
            }
            PickPurpose::ScanSource => {
                self.scanner.source = item.value;
                Ok(())
            }
            PickPurpose::AstRev => {
                self.ast.rev = item.value;
                self.ast.marked.clear();
                self.ast_ensure_loaded();
                Ok(())
            }
            PickPurpose::TrackLocation { game } => {
                let idx: usize = item.value.parse().context("invalid location index")?;
                self.track_discovered(&game, idx)
            }
        }
    }

    /// Picker over the tracked files of the selected game.
    pub(crate) fn pick_file(&mut self) -> Result<()> {
        self.require_game()?;
        if self.files.is_empty() {
            bail!("the selected game has no tracked files");
        }
        let items: Vec<PickItem> = self
            .files
            .iter()
            .map(|f| PickItem { label: f.rel_path.clone(), detail: widgets::bytes(f.size), value: f.rel_path.clone() })
            .collect();
        let sel = self.file.as_ref().and_then(|cur| items.iter().position(|i| &i.value == cur)).unwrap_or(0);
        self.mode = Mode::Picker(Picker { title: "Tracked file".into(), items, sel, purpose: PickPurpose::File });
        Ok(())
    }

    /// Picker items: WORKING plus the commits of the current branch.
    pub(crate) fn revision_items(&self) -> Vec<PickItem> {
        let mut items = vec![PickItem {
            label: WORKING.to_string(),
            detail: "files on disk now".into(),
            value: WORKING.to_string(),
        }];
        for (i, c) in self.timeline.commits.iter().enumerate() {
            let rev = if i == 0 { "HEAD".to_string() } else { format!("HEAD~{i}") };
            items.push(PickItem {
                label: format!("{rev:<8} #{:<5} [{}]", c.id, c.kind),
                detail: format!("{} {}", widgets::short_time(c.created_at), widgets::ellipsize(&c.summary, 60)),
                value: rev,
            });
        }
        items
    }

    // ------------------------------------------------------------------
    // Panic restore
    // ------------------------------------------------------------------

    fn panic_prompt(&mut self) -> Result<()> {
        let Some(m) = self.engine.store().last_mutation(&PanicScope::Latest)? else {
            self.toast(Level::Info, "Nothing to restore: SaveLens has not modified any file yet");
            return Ok(());
        };
        let lines = vec![
            Line::from(Span::styled("Revert the most recent SaveLens mutation, bit for bit:", theme::subtle())),
            Line::from(""),
            widgets::kv("File", m.abs_path.display().to_string(), theme::TEXT),
            widgets::kv("Change", m.reason.clone(), theme::PEACH),
            widgets::kv("When", savelens_cas::format_ts(m.created_at), theme::SUBTEXT),
            widgets::kv("Restore", m.safety.tag.clone(), theme::YELLOW),
            widgets::kv(
                "Verified",
                if m.safety.verified { "blake3 verified backup" } else { "backup NOT verified" },
                if m.safety.verified { theme::GREEN } else { theme::RED },
            ),
            Line::from(""),
            Line::from(Span::styled(
                "The current bytes are backed up first, so this can itself be undone.",
                theme::muted(),
            )),
        ];
        self.confirm("⚠ PANIC RESTORE", lines, Action::PanicRestore, true);
        Ok(())
    }

    fn panic_restore(&mut self) -> Result<()> {
        let rep = self.engine.panic_restore(&PanicScope::Latest)?;
        let what = if rep.removed {
            format!("{} removed (it did not exist before)", rep.path.display())
        } else {
            format!("{} restored to {} (hash verified)", rep.path.display(), rep.restored_tag)
        };
        self.toast(Level::Warning, format!("Panic restore: {what}"));
        self.refresh();
        self.ast_reload_if_working();
        Ok(())
    }

    // ------------------------------------------------------------------
    // Command line
    // ------------------------------------------------------------------

    fn run_command(&mut self, line: &str) -> Result<()> {
        let args = split_args(line)?;
        let Some((cmd, rest)) = args.split_first() else {
            return Ok(());
        };
        match cmd.as_str() {
            "track" => {
                let (name, dir, patterns) = match rest {
                    [name, dir, patterns @ ..] => (name, dir, patterns.to_vec()),
                    _ => bail!("usage: track <name> <dir> [pattern…]"),
                };
                let dir = expand_home(dir);
                let (game, commit) = self.engine.track(name, None, &dir, &patterns, "native")?;
                let files = commit.map(|c| c.summary).unwrap_or_else(|| "nothing to import".into());
                self.toast(Level::Success, format!("Tracking {} ({files})", game.name));
                self.refresh();
                self.select_game(game.id);
            }
            "snapshot" | "snap" => {
                let msg = if rest.is_empty() { "manual snapshot (TUI)".to_string() } else { rest.join(" ") };
                self.snapshot(&msg)?;
            }
            "branch" => {
                let [name] = rest else { bail!("usage: branch <name>") };
                let game = self.require_game()?;
                let b = self.engine.create_branch(game.id, name, None)?;
                self.toast(Level::Success, format!("Created branch {} (B on the Timeline to switch)", b.name));
                self.refresh();
            }
            "checkout" => {
                let [rev] = rest else { bail!("usage: checkout <rev>") };
                self.timeline_checkout(rev)?;
            }
            "tag" => {
                let [name] = rest else { bail!("usage: tag <name>") };
                let game = self.require_game()?;
                let head = self.engine.store().resolve_rev(game.id, "HEAD")?;
                self.timeline_tag(name, head)?;
            }
            "gc" => {
                let (blobs, chunks) = self.engine.store().gc()?;
                self.toast(Level::Success, format!("gc: removed {blobs} blob(s) and {chunks} chunk(s)"));
                self.refresh();
            }
            "discover" => self.start_discovery(),
            "watch" => self.toggle_daemon()?,
            "help" => self.mode = Mode::Help { scroll: 0 },
            "q" | "quit" => self.request_quit(),
            other => bail!("unknown command `{other}` (try track, snapshot, branch, checkout, tag, gc)"),
        }
        Ok(())
    }

    /// Snapshot of the selected game.
    pub(crate) fn snapshot(&mut self, msg: &str) -> Result<()> {
        let game = self.require_game()?;
        match self.engine.snapshot(game.id, msg, CommitKind::Manual)? {
            Some(c) => self.toast(Level::Success, format!("Snapshot #{}: {}", c.id, c.summary)),
            None => self.toast(Level::Info, "Snapshot: nothing changed since HEAD"),
        }
        self.refresh();
        Ok(())
    }

    // ------------------------------------------------------------------
    // Drawing
    // ------------------------------------------------------------------

    /// Renders the whole UI.
    pub fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        f.render_widget(Block::default().style(theme::base()), area);
        if area.width < 24 || area.height < 6 {
            let msg = Paragraph::new(vec![
                Line::from(Span::styled("SaveLens", theme::bold_accent())),
                Line::from(Span::styled("enlarge the terminal", theme::muted())),
            ])
            .wrap(Wrap { trim: true })
            .style(theme::base());
            f.render_widget(msg, area);
            return;
        }
        let [header, body, status] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(area);
        self.draw_header(f, header);
        match self.tab {
            Tab::Games => self.draw_games(f, body),
            Tab::Timeline => self.draw_timeline(f, body),
            Tab::Scanner => self.draw_scanner(f, body),
            Tab::Ast => self.draw_ast(f, body),
        }
        self.draw_status(f, status);
        self.draw_toasts(f, body);
        self.draw_modal(f, area);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let mut spans = vec![Span::styled(
            " ◆ SaveLens ",
            Style::default().fg(theme::BG).bg(theme::ACCENT2).add_modifier(Modifier::BOLD),
        )];
        spans.push(Span::raw(" "));
        for t in Tab::ALL {
            let label = format!(" {} {} ", t.index() + 1, t.title());
            if t == self.tab {
                spans.push(Span::styled(
                    label,
                    Style::default().fg(theme::BG).bg(theme::ACCENT).add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::styled(label, theme::subtle()));
            }
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(theme::base()), area);

        let mut right = Vec::new();
        if let Some(g) = self.game_rec() {
            right.push(Span::styled(g.name.clone(), theme::bold_accent()));
            right.push(Span::styled(format!(" ⎇ {}", g.current_branch), Style::default().fg(theme::ACCENT2)));
            right.push(Span::raw("  "));
        }
        if self.daemon.is_some() {
            right.push(Span::styled("● watching", Style::default().fg(theme::GREEN)));
        } else if self.daemon_status.is_some() {
            right.push(Span::styled("● external watcher", Style::default().fg(theme::TEAL)));
        } else {
            right.push(Span::styled("○ watcher off", theme::muted()));
        }
        right.push(Span::raw(" "));
        f.render_widget(
            Paragraph::new(Line::from(right)).alignment(Alignment::Right).style(Style::default().bg(theme::BG)),
            right_part(area, 48),
        );
    }

    fn status_hints(&self) -> Vec<(&'static str, &'static str)> {
        match &self.mode {
            Mode::Input(_) => vec![("Enter", "confirm"), ("Esc", "cancel"), ("C-u", "clear")],
            Mode::Confirm(_) => vec![("y/Enter", "confirm"), ("n/Esc", "cancel")],
            Mode::Picker(_) => vec![("j/k", "move"), ("Enter", "select"), ("Esc", "close")],
            Mode::Viewer { .. } | Mode::Help { .. } => vec![("j/k", "scroll"), ("Esc", "close")],
            Mode::ScanOptions { .. } => vec![("j/k", "move"), ("Space", "toggle"), ("Esc", "close")],
            Mode::Normal => {
                let mut h = match self.tab {
                    Tab::Games => vec![
                        ("j/k", "move"),
                        ("Enter", "select/track"),
                        ("s", "snapshot"),
                        ("d", "discover"),
                        ("t", "trainer"),
                        ("W", "watch flag"),
                    ],
                    Tab::Timeline => vec![
                        ("j/k", "move"),
                        ("c", "checkout"),
                        ("b", "branch"),
                        ("B", "switch"),
                        ("T", "tag"),
                        ("d", "diff"),
                    ],
                    Tab::Scanner => match self.scanner.pane {
                        ScanPane::Candidates => vec![
                            ("i", "filter"),
                            ("n", "scan"),
                            ("R", "reset"),
                            ("f", "file"),
                            ("s", "source"),
                            ("o", "options"),
                            ("a", "pin"),
                            ("e", "edit"),
                            ("F", "freeze"),
                            ("v", "cheats"),
                        ],
                        ScanPane::Cheats => {
                            vec![("Enter", "edit"), ("F", "freeze"), ("x", "delete"), ("v", "candidates")]
                        }
                    },
                    Tab::Ast => vec![
                        ("h/l", "fold"),
                        ("/", "search"),
                        ("n/N", "next"),
                        ("e", "edit"),
                        ("m", "mark"),
                        ("r", "revision"),
                        ("C", "cherry-pick"),
                        ("f", "file"),
                    ],
                };
                h.insert(0, ("?", "help"));
                h.extend([("w", "watch"), ("P", "panic"), (":", "cmd"), ("q", "quit")]);
                h
            }
        }
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let mut spans: Vec<Span> = Vec::new();
        if let Some(job) = self.jobs.running().first() {
            spans.push(Span::styled(
                format!(
                    " {} {} ({:.1}s) ",
                    widgets::spinner(self.frame),
                    job.label,
                    job.started.elapsed().as_secs_f32()
                ),
                Style::default().fg(theme::BG).bg(theme::YELLOW).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" "));
        }
        for (k, l) in self.status_hints() {
            spans.extend(theme::hint(k, l));
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(Style::default().bg(theme::PANEL)), area);
    }

    fn draw_toasts(&self, f: &mut Frame, body: Rect) {
        let toasts = self.toasts.visible(4);
        if toasts.is_empty() || body.height < 4 {
            return;
        }
        let width = body.width.min(72);
        let n = (toasts.len() as u16).min(body.height.saturating_sub(2));
        let skip = toasts.len() - n as usize;
        for (i, t) in toasts.iter().skip(skip).enumerate() {
            let y = body.y + body.height - n + i as u16;
            let rect = Rect { x: body.x + body.width - width, y, width, height: 1 };
            let text = widgets::ellipsize(&t.text, width.saturating_sub(4) as usize);
            let line = Line::from(vec![
                Span::styled(format!(" {} ", t.level.icon()), Style::default().fg(theme::BG).bg(t.level.color())),
                Span::styled(format!(" {text}"), Style::default().fg(theme::TEXT)),
            ]);
            f.render_widget(Clear, rect);
            f.render_widget(Paragraph::new(line).style(Style::default().bg(theme::SURFACE)), rect);
        }
    }

    fn draw_modal(&mut self, f: &mut Frame, area: Rect) {
        match &self.mode {
            Mode::Normal => {}
            Mode::Help { scroll } => draw_help(f, area, *scroll),
            Mode::Input(p) => {
                let rect = widgets::centered(area, 76, 6);
                f.render_widget(Clear, rect);
                let block = theme::modal(&p.title, theme::ACCENT);
                let inner = block.inner(rect);
                f.render_widget(block, rect);
                let mut spans = vec![Span::styled("❯ ", theme::bold_accent())];
                spans.extend(p.input.spans(true));
                let lines =
                    vec![Line::from(spans), Line::from(""), Line::from(Span::styled(p.help.clone(), theme::muted()))];
                f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
            }
            Mode::Confirm(c) => {
                let color = if c.danger { theme::RED } else { theme::YELLOW };
                let h = (c.lines.len() as u16).saturating_add(5);
                let rect = widgets::centered(area, 84, h);
                f.render_widget(Clear, rect);
                let block = theme::modal(&c.title, color);
                let inner = block.inner(rect);
                f.render_widget(block, rect);
                let mut lines = c.lines.clone();
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled(" y ", Style::default().fg(theme::BG).bg(color).add_modifier(Modifier::BOLD)),
                    Span::styled(" confirm    ", theme::subtle()),
                    Span::styled(" n ", Style::default().fg(theme::BG).bg(theme::OVERLAY).add_modifier(Modifier::BOLD)),
                    Span::styled(" cancel", theme::subtle()),
                ]));
                f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
            }
            Mode::Picker(p) => {
                let h = (p.items.len() as u16).saturating_add(2).clamp(3, 20);
                let rect = widgets::centered(area, 90, h);
                f.render_widget(Clear, rect);
                let items: Vec<ListItem> = p
                    .items
                    .iter()
                    .map(|i| {
                        ListItem::new(Line::from(vec![
                            Span::styled(i.label.clone(), Style::default().fg(theme::TEXT)),
                            Span::raw("  "),
                            Span::styled(i.detail.clone(), theme::muted()),
                        ]))
                    })
                    .collect();
                let list = List::new(items)
                    .block(theme::modal(&p.title, theme::ACCENT))
                    .highlight_style(theme::selected())
                    .highlight_symbol("▶ ");
                let mut state = ListState::default().with_selected(Some(p.sel));
                f.render_stateful_widget(list, rect, &mut state);
            }
            Mode::Viewer { title, lines, scroll } => {
                let rect =
                    widgets::centered(area, area.width.saturating_sub(8).max(20), area.height.saturating_sub(4).max(5));
                f.render_widget(Clear, rect);
                let p = Paragraph::new(lines.clone())
                    .block(theme::modal(title, theme::ACCENT2))
                    .scroll((*scroll, 0))
                    .wrap(Wrap { trim: false });
                f.render_widget(p, rect);
            }
            Mode::ScanOptions { sel } => {
                let sel = *sel;
                self.draw_scan_options(f, area, sel);
            }
        }
    }
}

/// Checks that a global-hotkey backend can work in this session. On Linux
/// the backend needs an X11 display (natively or through XWayland); without
/// one, initialising it is not safe, so hotkeys are skipped with a warning.
fn hotkey_backend() -> std::result::Result<(), String> {
    if cfg!(all(unix, not(target_os = "macos"))) {
        let has_display = std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty());
        if !has_display {
            return Err("no X11 display (DISPLAY is not set)".into());
        }
    }
    Ok(())
}

/// The right-most `w` columns of `area`.
fn right_part(area: Rect, w: u16) -> Rect {
    let w = w.min(area.width);
    Rect { x: area.x + area.width - w, width: w, ..area }
}

fn draw_help(f: &mut Frame, area: Rect, scroll: u16) {
    let rect = widgets::centered(area, 92, area.height.saturating_sub(2).max(5));
    f.render_widget(Clear, rect);
    let section = |t: &str| Line::from(Span::styled(t.to_string(), theme::title()));
    let row = |k: &str, d: &str| {
        Line::from(vec![
            Span::styled(format!("  {k:<16}"), theme::bold_accent()),
            Span::styled(d.to_string(), Style::default().fg(theme::TEXT)),
        ])
    };
    let lines = vec![
        section("Global"),
        row("1-4 / Tab / S-Tab", "switch tab"),
        row("w", "start / stop the embedded watcher daemon (global hotkeys enabled)"),
        row("P", "PANIC RESTORE: revert the last SaveLens mutation bit for bit"),
        row(":", "command line: track, snapshot, branch, checkout, tag, gc, discover, watch"),
        row("?", "this help"),
        row("q / Ctrl-c", "quit (asks first while the watcher runs)"),
        Line::from(""),
        section("1 Games & Watcher"),
        row("j / k", "move through tracked and discovered games"),
        row("Enter", "select a tracked game / track a discovered game"),
        row("d", "discover Steam / Proton / rule-based games (background)"),
        row("s", "snapshot the selected game"),
        row("t", "toggle the disk trainer (freeze-on-save)"),
        row("W", "toggle watching of the selected game"),
        Line::from(""),
        section("2 Timeline & Branches"),
        row("j / k  g / G", "move through commits"),
        row("c", "checkout the highlighted commit (asks first)"),
        row("b", "new branch from the highlighted commit"),
        row("B", "switch branch"),
        row("T", "tag the highlighted commit"),
        row("d", "semantic diff highlighted commit vs WORKING"),
        Line::from(""),
        section("3 Save-State Scanner"),
        row("i or /", "edit the filter: 1500 · 1500 -> 1300 · +300 · - 300 · inc · dec · changed · unchanged · ?"),
        row("n / Enter", "first scan, then next scans against the selected source"),
        row("R", "reset the scan"),
        row("f / s / o", "pick file / snapshot source / scan options"),
        row("a", "pin the candidate under an alias"),
        row("e", "edit the candidate's value on disk"),
        row("F", "freeze toggle"),
        row("v", "switch focus between candidates and the cheat table"),
        row("Enter / x", "(cheat table) edit value / delete entry"),
        Line::from(""),
        section("4 AST Explorer & Cherry-Pick"),
        row("j / k  g / G", "move"),
        row("l / → · h / ←", "expand · collapse"),
        row("/  n / N", "fuzzy search paths and values · next / previous match"),
        row("e", "edit a scalar leaf on disk"),
        row("m", "mark / unmark a sub-tree for cherry-pick"),
        row("r", "choose the revision shown (the cherry-pick source)"),
        row("C", "cherry-pick the marked sub-trees into the working file"),
        row("f", "pick file"),
    ];
    let p = Paragraph::new(lines).block(theme::modal("Help — SaveLens keys", theme::ACCENT2)).scroll((scroll, 0));
    f.render_widget(p, rect);
}

/// Splits a command line into arguments, honouring double quotes.
pub(crate) fn split_args(line: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has_token = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true;
            }
            '\\' if in_quotes => match chars.next() {
                Some(n) => cur.push(n),
                None => bail!("dangling escape at end of command"),
            },
            c if c.is_whitespace() && !in_quotes => {
                if has_token {
                    out.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            c => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if in_quotes {
        bail!("unterminated quote in command");
    }
    if has_token {
        out.push(cur);
    }
    Ok(out)
}

fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

/// Types offered by the scanner options.
pub(crate) const SCAN_TYPES: [ScanType; 6] =
    [ScanType::I32, ScanType::U32, ScanType::I64, ScanType::F32, ScanType::F64, ScanType::AsciiDecimal];

/// Alignment label.
pub(crate) fn align_label(a: ScanAlign) -> String {
    match a {
        ScanAlign::Unaligned => "unaligned".into(),
        ScanAlign::Natural => "natural".into(),
        ScanAlign::Fixed(n) => format!("{n}-byte"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_args_handles_quotes() {
        assert_eq!(
            split_args("track \"My Game\" /tmp/x *.json").unwrap(),
            vec!["track", "My Game", "/tmp/x", "*.json"]
        );
        assert_eq!(split_args("  snapshot  ").unwrap(), vec!["snapshot"]);
        assert_eq!(split_args("tag \"\"").unwrap(), vec!["tag", ""]);
        assert!(split_args("track \"open").is_err());
        assert!(split_args("").unwrap().is_empty());
    }

    #[test]
    fn home_expansion() {
        assert_eq!(expand_home("/abs"), PathBuf::from("/abs"));
    }
}
