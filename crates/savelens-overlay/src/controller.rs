//! Headless state & controller of the overlay.
//!
//! Everything the window shows and every action it can trigger lives here,
//! so it can be exercised by tests without a display. The UI layer
//! ([`crate::OverlayApp`]) only renders this state and forwards clicks.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::{unbounded, Receiver, Sender};
use savelens_cas::{CommitKind, EventRecord, GameId, GameRecord, PanicReport, PanicScope, WatchQuestion};
use savelens_core::{ScalarValue, VariableCategory};
use savelens_watcher::{
    CheatValue, Daemon, DaemonEvent, DaemonHandle, DaemonOptions, Engine, HotkeyAction, HotkeyConfig, HotkeyService,
    IgnoreScope,
};

use crate::bus;
use crate::presets::{builtin_presets, Preset, PresetEntry, PresetStore};
use crate::OverlayOptions;

/// Injections slower than this are highlighted in amber.
pub const LATENCY_WARN_MS: f64 = 50.0;
/// Values kept per variable for the sparkline.
pub const HISTORY_LEN: usize = 32;
/// Periodic (mtime-gated) refresh interval of the cheat table.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
/// Poll interval of the store activity log.
const EVENT_POLL: Duration = Duration::from_millis(500);
/// Poll interval of the request bus.
const REQUEST_POLL: Duration = Duration::from_millis(100);
/// Poll interval of the request bus while foreign requests have no consumer.
/// A daemon heartbeat younger than this means "a daemon is running"
/// (the daemon beats every 2 s).
const DAEMON_ALIVE_MS: i64 = 10_000;
/// Notices kept on screen.
const MAX_NOTICES: usize = 4;
/// Activity lines kept.
const MAX_ACTIVITY: usize = 6;

/// Wakes the UI thread (e.g. `egui::Context::request_repaint`).
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// Direction of the last change of a variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trend {
    Up,
    Down,
    Flat,
    Unknown,
}

/// A row of the live cheat table.
#[derive(Debug, Clone, PartialEq)]
pub struct CheatRow {
    pub alias: String,
    pub rel_path: String,
    /// Human-readable location (`Player.Gold`, `@0x40 u32le`, …).
    pub location: String,
    /// Value currently on disk.
    pub current: Option<ScalarValue>,
    /// Why the value could not be read.
    pub error: Option<String>,
    pub freeze_enabled: bool,
    pub frozen_value: Option<ScalarValue>,
    /// Distinct successive values seen on disk (oldest first).
    pub history: VecDeque<f64>,
}

impl CheatRow {
    /// Direction of the last change.
    pub fn trend(&self) -> Trend {
        let n = self.history.len();
        if n < 2 {
            return Trend::Unknown;
        }
        let (a, b) = (self.history[n - 2], self.history[n - 1]);
        if b > a {
            Trend::Up
        } else if b < a {
            Trend::Down
        } else {
            Trend::Flat
        }
    }

    /// The value on disk differs from the frozen value.
    pub fn drifted(&self) -> bool {
        match (self.current, self.frozen_value) {
            (Some(c), Some(f)) if self.freeze_enabled => c.as_f64() != f.as_f64(),
            _ => false,
        }
    }
}

/// Where a latency measurement comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatencySource {
    /// `DaemonEvent::TrainerApplied` of the embedded daemon.
    Daemon,
    /// "Apply now" / quick snapshot in this overlay.
    Overlay,
    /// Parsed from the shared activity log (another process).
    EventLog,
}

/// Last freeze-on-save injection.
#[derive(Debug, Clone, PartialEq)]
pub struct LatencySample {
    pub ms: f64,
    pub source: LatencySource,
    pub rel_path: String,
    pub applied: Vec<String>,
    pub at: Instant,
}

impl LatencySample {
    /// Slower than [`LATENCY_WARN_MS`].
    pub fn is_slow(&self) -> bool {
        self.ms > LATENCY_WARN_MS
    }
}

/// Severity of a notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

/// A message shown inline in the overlay.
#[derive(Debug, Clone, PartialEq)]
pub struct Notice {
    pub level: NoticeLevel,
    pub text: String,
    pub at: Instant,
}

/// State of the global hotkeys.
#[derive(Debug, Clone, PartialEq)]
pub enum HotkeyStatus {
    /// Not requested (`OverlayOptions::hotkeys == false`).
    Off,
    /// Disabled in `config.toml` (`[hotkeys] enabled = false`).
    DisabledInConfig,
    /// A running `savelens watch` daemon registered them.
    OwnedByDaemon { pid: u32 },
    /// Registered by this overlay.
    Active { bindings: Vec<(HotkeyAction, String)> },
    /// Could not be registered (no X11 display, key taken, bad binding…).
    Unavailable(String),
}

impl HotkeyStatus {
    /// Should be shown as an amber warning.
    pub fn is_warning(&self) -> bool {
        matches!(self, HotkeyStatus::Unavailable(_))
    }

    /// One-line description.
    pub fn describe(&self) -> String {
        match self {
            HotkeyStatus::Off => "global hotkeys off".into(),
            HotkeyStatus::DisabledInConfig => "global hotkeys disabled in config.toml".into(),
            HotkeyStatus::OwnedByDaemon { pid } => format!("hotkeys handled by `savelens watch` (pid {pid})"),
            HotkeyStatus::Active { bindings } => {
                bindings.iter().map(|(a, k)| format!("{k} {}", short_label(*a))).collect::<Vec<_>>().join(" · ")
            }
            HotkeyStatus::Unavailable(e) => format!("hotkeys unavailable: {e}"),
        }
    }
}

fn short_label(a: HotkeyAction) -> &'static str {
    match a {
        HotkeyAction::QuickSnapshot => "snap",
        HotkeyAction::QuickRollback => "rollback",
        HotkeyAction::PanicRestore => "PANIC",
        HotkeyAction::ToggleTrainer => "trainer",
        HotkeyAction::ToggleOverlay => "overlay",
    }
}

/// Who performs freeze-on-save injections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonMode {
    /// A daemon thread inside this process.
    Embedded,
    /// Another process (`savelens watch`), observed through the store.
    External { pid: u32 },
    /// Nobody: values are only injected by "Apply now".
    Absent,
}

/// What the UI must do after a controller step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayCommand {
    /// Show / hide the overlay window.
    SetVisible(bool),
}

/// Result of "Quick snapshot (with template)".
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotOutcome {
    /// Commit of the snapshot (`None`: nothing changed since the last commit).
    pub snapshot: Option<i64>,
    /// Commits created by the template injection.
    pub trainer_commits: Vec<i64>,
    /// Injection measurement (`None`: values were already frozen on disk).
    pub latency: Option<LatencySample>,
}

/// Size + mtime of a pinned file (`None` when it does not exist).
type FileStamp = Option<(u64, Option<SystemTime>)>;

/// Extracts a latency like `12.5 ms` / `(3ms)` from an activity message.
pub fn parse_latency_ms(message: &str) -> Option<f64> {
    let tokens: Vec<&str> = message
        .split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | ',' | ';' | '[' | ']'))
        .filter(|t| !t.is_empty())
        .collect();
    for (i, tok) in tokens.iter().enumerate() {
        if let Some(num) = tok.strip_suffix("ms") {
            if let Ok(v) = num.parse::<f64>() {
                if v.is_finite() && v >= 0.0 {
                    return Some(v);
                }
            }
        }
        if tokens.get(i + 1).is_some_and(|n| *n == "ms") {
            if let Ok(v) = tok.parse::<f64>() {
                if v.is_finite() && v >= 0.0 {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Headless controller of the overlay.
pub struct OverlayController {
    engine: Engine,
    opts: OverlayOptions,
    games: Vec<GameRecord>,
    selected: Option<GameId>,
    rows: Vec<CheatRow>,
    stamps: HashMap<String, FileStamp>,
    disk_reads: u64,
    last_refresh: Option<Instant>,
    refresh_pending: bool,
    latency: Option<LatencySample>,
    notices: VecDeque<Notice>,
    activity: VecDeque<String>,
    presets: PresetStore,
    visible: bool,
    daemon: Option<DaemonHandle>,
    daemon_mode: DaemonMode,
    hotkeys: Option<HotkeyService>,
    hotkey_status: HotkeyStatus,
    actions_tx: Sender<HotkeyAction>,
    actions_rx: Receiver<HotkeyAction>,
    daemon_tx: Sender<DaemonEvent>,
    daemon_rx: Receiver<DaemonEvent>,
    waker: Arc<Mutex<Option<Waker>>>,
    event_cursor: i64,
    last_event_poll: Option<Instant>,
    last_request_poll: Option<Instant>,
    own_messages: VecDeque<String>,
    /// Pending "what is this variable?" questions of the selected game.
    questions: Vec<WatchQuestion>,
    /// Quiet mode: questions keep being collected but the card is hidden.
    quiet: bool,
}

impl std::fmt::Debug for OverlayController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayController")
            .field("selected", &self.selected)
            .field("rows", &self.rows.len())
            .field("daemon_mode", &self.daemon_mode)
            .field("hotkeys", &self.hotkey_status)
            .finish()
    }
}

impl OverlayController {
    /// Loads games, presets and the cheat table. Does not start the daemon
    /// or the hotkeys (see [`OverlayController::start_services`]).
    pub fn new(engine: Engine, opts: &OverlayOptions) -> Result<Self> {
        let games = engine.store().games().context("cannot list tracked games")?;
        let (presets, preset_error) = PresetStore::load_lenient(engine.store().root());
        let event_cursor =
            engine.store().events(None, 1).context("cannot read the activity log")?.first().map(|e| e.id).unwrap_or(0);
        let (actions_tx, actions_rx) = unbounded();
        let (daemon_tx, daemon_rx) = unbounded();
        let mut c = OverlayController {
            engine,
            opts: opts.clone(),
            games,
            selected: None,
            rows: Vec::new(),
            stamps: HashMap::new(),
            disk_reads: 0,
            last_refresh: None,
            refresh_pending: false,
            latency: None,
            notices: VecDeque::new(),
            activity: VecDeque::new(),
            presets,
            visible: true,
            daemon: None,
            daemon_mode: DaemonMode::Absent,
            hotkeys: None,
            hotkey_status: HotkeyStatus::Off,
            actions_tx,
            actions_rx,
            daemon_tx,
            daemon_rx,
            waker: Arc::new(Mutex::new(None)),
            event_cursor,
            last_event_poll: None,
            last_request_poll: None,
            own_messages: VecDeque::new(),
            questions: Vec::new(),
            quiet: false,
        };
        if let Some(e) = preset_error {
            c.notify(NoticeLevel::Warn, format!("presets file unreadable, starting empty: {e}"));
        }
        let initial = match opts.game {
            Some(id) if c.games.iter().any(|g| g.id == id) => Some(id),
            Some(id) => {
                c.notify(NoticeLevel::Warn, format!("game #{id} is not tracked"));
                c.default_game()
            }
            None => c.default_game(),
        };
        if let Some(id) = initial {
            c.selected = Some(id);
            if let Err(e) = c.refresh(true) {
                c.notify(NoticeLevel::Error, format!("{e:#}"));
            }
        }
        Ok(c)
    }

    /// First game with pinned variables, else the first game.
    fn default_game(&self) -> Option<GameId> {
        self.games
            .iter()
            .find(|g| self.engine.store().cheats(g.id).map(|c| !c.is_empty()).unwrap_or(false))
            .or_else(|| self.games.first())
            .map(|g| g.id)
    }

    // ------------------------------------------------------------------
    // Services: daemon & hotkeys
    // ------------------------------------------------------------------

    /// Starts the embedded daemon and the global hotkeys according to the
    /// options. Failures degrade gracefully into notices.
    pub fn start_services(&mut self) {
        let external = match self.engine.store().daemon_status(DAEMON_ALIVE_MS) {
            Ok(s) => s.filter(|s| s.pid != std::process::id()),
            Err(e) => {
                self.notify(NoticeLevel::Warn, format!("cannot read the daemon status: {e}"));
                None
            }
        };
        match (&external, self.opts.embedded_daemon) {
            (Some(st), _) => {
                self.daemon_mode = DaemonMode::External { pid: st.pid };
                if self.opts.embedded_daemon {
                    self.notify(
                        NoticeLevel::Info,
                        format!("`savelens watch` already running (pid {}): using it", st.pid),
                    );
                }
            }
            (None, true) => self.start_embedded_daemon(),
            (None, false) => {
                self.daemon_mode = DaemonMode::Absent;
                self.notify(
                    NoticeLevel::Warn,
                    "no daemon running: auto-freeze on save is inactive (start `savelens watch` or use Apply now)",
                );
            }
        }
        let cfg = self.engine.config().hotkeys.clone();
        self.hotkey_status = if !self.opts.hotkeys {
            HotkeyStatus::Off
        } else if !cfg.enabled {
            HotkeyStatus::DisabledInConfig
        } else if let Some(st) = external {
            HotkeyStatus::OwnedByDaemon { pid: st.pid }
        } else {
            self.start_hotkeys_with(&cfg);
            self.hotkey_status.clone()
        };
    }

    fn start_embedded_daemon(&mut self) {
        let opts = DaemonOptions {
            // The overlay owns the hotkeys itself so that "quick snapshot"
            // applies the template and "toggle overlay" acts directly.
            hotkeys: false,
            games: None,
            tick: Duration::from_millis(50),
        };
        match Daemon::spawn(self.engine.clone(), opts) {
            Ok(handle) => {
                let rx = handle.events().clone();
                match spawn_forwarder("savelens-overlay-daemon", rx, self.daemon_tx.clone(), self.waker.clone()) {
                    Ok(()) => {
                        self.daemon = Some(handle);
                        self.daemon_mode = DaemonMode::Embedded;
                    }
                    Err(e) => {
                        self.daemon_mode = DaemonMode::Absent;
                        self.notify(NoticeLevel::Error, format!("embedded daemon not started: {e:#}"));
                    }
                }
            }
            Err(e) => {
                self.daemon_mode = DaemonMode::Absent;
                self.notify(NoticeLevel::Error, format!("embedded daemon not started: {e}"));
            }
        }
    }

    /// Registers the global hotkeys of `cfg` (status in [`Self::hotkey_status`]).
    pub fn start_hotkeys_with(&mut self, cfg: &HotkeyConfig) {
        self.hotkeys = None;
        match HotkeyService::start(cfg) {
            Ok(service) => {
                let rx = service.actions().clone();
                match spawn_forwarder("savelens-overlay-hotkeys", rx, self.actions_tx.clone(), self.waker.clone()) {
                    Ok(()) => {
                        self.hotkey_status = HotkeyStatus::Active { bindings: service.bindings().to_vec() };
                        self.hotkeys = Some(service);
                    }
                    Err(e) => self.hotkey_status = HotkeyStatus::Unavailable(format!("{e:#}")),
                }
            }
            Err(e) => self.hotkey_status = HotkeyStatus::Unavailable(e.to_string()),
        }
    }

    /// Installs the function used to wake the UI when a hotkey fires or the
    /// daemon reports something.
    pub fn set_waker(&self, waker: Waker) {
        if let Ok(mut w) = self.waker.lock() {
            *w = Some(waker);
        }
    }

    /// Injects a hotkey action as if it had been pressed.
    pub fn queue_action(&self, action: HotkeyAction) {
        let _ = self.actions_tx.send(action);
    }

    // ------------------------------------------------------------------
    // Accessors
    // ------------------------------------------------------------------

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn games(&self) -> &[GameRecord] {
        &self.games
    }

    pub fn selected_game(&self) -> Option<&GameRecord> {
        let id = self.selected?;
        self.games.iter().find(|g| g.id == id)
    }

    pub fn rows(&self) -> &[CheatRow] {
        &self.rows
    }

    pub fn row(&self, alias: &str) -> Option<&CheatRow> {
        self.rows.iter().find(|r| r.alias == alias)
    }

    /// Number of times the pinned values were re-read from disk.
    pub fn disk_reads(&self) -> u64 {
        self.disk_reads
    }

    pub fn last_latency(&self) -> Option<&LatencySample> {
        self.latency.as_ref()
    }

    pub fn notices(&self) -> impl Iterator<Item = &Notice> {
        self.notices.iter()
    }

    pub fn activity(&self) -> impl Iterator<Item = &String> {
        self.activity.iter()
    }

    pub fn hotkey_status(&self) -> &HotkeyStatus {
        &self.hotkey_status
    }

    pub fn daemon_mode(&self) -> &DaemonMode {
        &self.daemon_mode
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn trainer_enabled(&self) -> bool {
        self.selected_game().is_some_and(|g| g.trainer_enabled)
    }

    /// Adds an inline notice.
    pub fn notify(&mut self, level: NoticeLevel, text: impl Into<String>) {
        let text = text.into();
        match level {
            NoticeLevel::Error => tracing::warn!("overlay: {text}"),
            _ => tracing::info!("overlay: {text}"),
        }
        self.notices.push_back(Notice { level, text, at: Instant::now() });
        while self.notices.len() > MAX_NOTICES {
            self.notices.pop_front();
        }
    }

    /// Removes every notice.
    pub fn clear_notices(&mut self) {
        self.notices.clear();
    }

    fn require_game(&self) -> Result<GameRecord> {
        self.selected_game().cloned().ok_or_else(|| anyhow!("no game selected — track a game first"))
    }

    // ------------------------------------------------------------------
    // Game selection & cheat table
    // ------------------------------------------------------------------

    /// Reloads the list of tracked games.
    pub fn reload_games(&mut self) -> Result<()> {
        self.games = self.engine.store().games()?;
        if self.selected.is_some_and(|id| !self.games.iter().any(|g| g.id == id)) {
            self.selected = None;
            self.rows.clear();
            self.stamps.clear();
        }
        if self.selected.is_none() {
            if let Some(id) = self.default_game() {
                self.select_game(id)?;
            }
        }
        Ok(())
    }

    /// Switches to another game.
    pub fn select_game(&mut self, id: GameId) -> Result<()> {
        if !self.games.iter().any(|g| g.id == id) {
            self.games = self.engine.store().games()?;
        }
        if !self.games.iter().any(|g| g.id == id) {
            bail!("game #{id} is not tracked");
        }
        self.selected = Some(id);
        self.rows.clear();
        self.stamps.clear();
        self.questions.clear();
        self.latency = None;
        self.refresh(true).map(|_| ())
    }

    fn stamps_for(&self, game: &GameRecord, rels: &BTreeSet<String>) -> HashMap<String, FileStamp> {
        rels.iter()
            .map(|rel| {
                let stamp = self
                    .engine
                    .abs_path(game, rel)
                    .ok()
                    .and_then(|p| fs::metadata(p).ok())
                    .map(|m| (m.len(), m.modified().ok()));
                (rel.clone(), stamp)
            })
            .collect()
    }

    /// Refreshes the cheat table. The game record and cheat entries (cheap
    /// SQLite queries) are always reloaded; the save files are only parsed
    /// again when `force` is set, the pinned set changed, or the size/mtime
    /// of a pinned file changed. Returns whether the files were re-read.
    pub fn refresh(&mut self, force: bool) -> Result<bool> {
        self.last_refresh = Some(Instant::now());
        self.refresh_pending = false;
        let Some(id) = self.selected else {
            self.rows.clear();
            self.questions.clear();
            return Ok(false);
        };
        let game = self.engine.game(id)?;
        self.questions = self.engine.questions(Some(id), false)?;
        match self.games.iter_mut().find(|g| g.id == id) {
            Some(slot) => *slot = game.clone(),
            None => self.games.push(game.clone()),
        }
        let entries = self.engine.store().cheats(id)?;
        let same_set = entries.len() == self.rows.len()
            && entries
                .iter()
                .zip(&self.rows)
                .all(|(e, r)| e.alias == r.alias && e.rel_path == r.rel_path && e.location.to_string() == r.location);
        let rels: BTreeSet<String> = entries.iter().map(|e| e.rel_path.clone()).collect();
        let stamps = self.stamps_for(&game, &rels);
        if !force && same_set && stamps == self.stamps {
            for (row, e) in self.rows.iter_mut().zip(&entries) {
                row.freeze_enabled = e.freeze_enabled;
                row.frozen_value = e.frozen_value;
            }
            return Ok(false);
        }
        let values = self.engine.cheat_values(id)?;
        self.disk_reads += 1;
        self.stamps = stamps;
        let mut old: HashMap<String, VecDeque<f64>> = self.rows.drain(..).map(|r| (r.alias, r.history)).collect();
        self.rows = values
            .into_iter()
            .map(|v: CheatValue| {
                let mut history = old.remove(&v.entry.alias).unwrap_or_default();
                if let Some(cur) = v.current {
                    let x = cur.as_f64();
                    if history.back().is_none_or(|last| *last != x) {
                        history.push_back(x);
                    }
                    while history.len() > HISTORY_LEN {
                        history.pop_front();
                    }
                }
                CheatRow {
                    alias: v.entry.alias.clone(),
                    rel_path: v.entry.rel_path.clone(),
                    location: v.entry.location.to_string(),
                    current: v.current,
                    error: v.error,
                    freeze_enabled: v.entry.freeze_enabled,
                    frozen_value: v.entry.frozen_value,
                    history,
                }
            })
            .collect();
        Ok(true)
    }

    // ------------------------------------------------------------------
    // "What is this variable?" questions
    // ------------------------------------------------------------------

    /// Pending questions of the selected game, best first.
    pub fn questions(&self) -> &[WatchQuestion] {
        &self.questions
    }

    /// Quiet mode hides the question card (questions are still collected).
    pub fn quiet(&self) -> bool {
        self.quiet
    }

    pub fn set_quiet(&mut self, quiet: bool) {
        self.quiet = quiet;
    }

    fn reload_questions(&mut self) -> Result<()> {
        self.questions = match self.selected {
            Some(id) => self.engine.questions(Some(id), false)?,
            None => Vec::new(),
        };
        Ok(())
    }

    /// Answers a question: the value is pinned into the cheat table.
    /// Returns the alias it was pinned under.
    pub fn answer_question(&mut self, id: i64, category: VariableCategory, alias: Option<&str>) -> Result<String> {
        let (q, entry) = self.engine.answer_question(id, category, alias)?;
        // The engine already logged it; do not echo it back as foreign activity.
        self.own_messages.push_back(format!("{} is {category}: pinned as `{}`", q.candidate().label(), entry.alias));
        self.notify(NoticeLevel::Info, format!("📌 {} = {category} ➡ `{}`", q.candidate().label(), entry.alias));
        self.reload_questions()?;
        self.refresh(true)?;
        Ok(entry.alias)
    }

    /// "Not interesting": mutes the value (or every value like it).
    pub fn ignore_question(&mut self, id: i64, scope: IgnoreScope) -> Result<String> {
        let (_, mute) = self.engine.ignore_question(id, scope)?;
        self.own_messages.push_back(format!("muted `{}`", mute.pattern));
        self.reload_questions()?;
        Ok(mute.pattern.0)
    }

    /// "Ask me later".
    pub fn snooze_question(&mut self, id: i64) -> Result<()> {
        self.engine.snooze_question(id, None)?;
        self.reload_questions()
    }

    // ------------------------------------------------------------------
    // Disk trainer
    // ------------------------------------------------------------------

    /// Turns freezing of one variable on/off. When turning it on without a
    /// stored frozen value, the current on-disk value is frozen.
    pub fn set_freeze_enabled(&mut self, alias: &str, enabled: bool) -> Result<()> {
        let game = self.require_game()?;
        let entry = self.engine.store().cheat_by_alias(game.id, alias)?;
        if enabled && entry.frozen_value.is_none() {
            self.engine.freeze(game.id, alias, None, true)?;
            if self.engine.store().cheat_by_alias(game.id, alias)?.frozen_value.is_none() {
                self.engine.store().toggle_freeze(game.id, alias, false)?;
                bail!("{alias}: no value on disk to freeze — type a frozen value first");
            }
        } else {
            self.engine.store().toggle_freeze(game.id, alias, enabled)?;
        }
        self.refresh(false)?;
        Ok(())
    }

    /// Sets the frozen value of a variable from user input (keeps the
    /// freeze toggle as it is). An empty input clears the value and
    /// disables freezing.
    pub fn set_frozen_value(&mut self, alias: &str, input: &str) -> Result<()> {
        let game = self.require_game()?;
        let entry = self.engine.store().cheat_by_alias(game.id, alias)?;
        if input.trim().is_empty() {
            self.engine.store().set_freeze(game.id, alias, None, false)?;
        } else {
            let v = ScalarValue::parse(input).map_err(|e| anyhow!("{alias}: {e}"))?;
            self.engine.store().set_freeze(game.id, alias, Some(v), entry.freeze_enabled)?;
        }
        self.refresh(false)?;
        Ok(())
    }

    /// Master trainer switch of the selected game.
    pub fn set_trainer_enabled(&mut self, enabled: bool) -> Result<()> {
        let game = self.require_game()?;
        self.engine.store().set_trainer_enabled(game.id, enabled)?;
        let msg = format!("disk trainer {} (overlay)", if enabled { "enabled" } else { "disabled" });
        self.log_own(Some(game.id), "trainer", &msg);
        self.refresh(false)?;
        Ok(())
    }

    fn log_own(&mut self, game: Option<GameId>, kind: &str, msg: &str) {
        self.own_messages.push_back(msg.to_string());
        while self.own_messages.len() > 16 {
            self.own_messages.pop_front();
        }
        if let Err(e) = self.engine.store().log_event(game, kind, msg) {
            self.notify(NoticeLevel::Warn, format!("activity log: {e}"));
        }
    }

    /// Injects the frozen values of every file that has active rules.
    /// Returns the measurement and the created commits.
    fn inject_all(&mut self, game: &GameRecord) -> Result<(Option<LatencySample>, Vec<i64>)> {
        let rels: BTreeSet<String> = self
            .engine
            .store()
            .cheats(game.id)?
            .into_iter()
            .filter(|c| c.freeze_enabled && c.frozen_value.is_some())
            .map(|c| c.rel_path)
            .collect();
        if rels.is_empty() {
            bail!("no frozen variables: enable ❄ on a variable first");
        }
        let mut total = 0.0;
        let mut applied = Vec::new();
        let mut files = Vec::new();
        let mut commits = Vec::new();
        for rel in &rels {
            if let Some(rep) = self.engine.inject_trainer(game.id, rel)? {
                total += rep.elapsed_ms;
                applied.extend(rep.applied);
                files.push(rep.rel_path);
                commits.extend(rep.commit);
            }
        }
        if applied.is_empty() {
            return Ok((None, commits));
        }
        let sample = LatencySample {
            ms: total,
            source: LatencySource::Overlay,
            rel_path: files.join(", "),
            applied: applied.clone(),
            at: Instant::now(),
        };
        let msg = format!("overlay injected {} into {} in {:.1} ms", applied.join(", "), files.join(", "), total);
        self.log_own(Some(game.id), "trainer", &msg);
        self.latency = Some(sample.clone());
        Ok((Some(sample), commits))
    }

    /// "Apply now": writes the template (frozen values) into the save files.
    pub fn apply_now(&mut self) -> Result<Option<LatencySample>> {
        let game = self.require_game()?;
        let (sample, _) = self.inject_all(&game)?;
        match &sample {
            Some(s) => self.notify(NoticeLevel::Info, format!("applied {} in {:.1} ms", s.applied.join(", "), s.ms)),
            None => self.notify(NoticeLevel::Info, "values already frozen on disk"),
        }
        self.refresh(true)?;
        Ok(sample)
    }

    /// Snapshot of every tracked file, then the template is injected.
    pub fn quick_snapshot(&mut self) -> Result<SnapshotOutcome> {
        let game = self.require_game()?;
        let snap = self.engine.snapshot(game.id, "quick snapshot (overlay)", CommitKind::Manual)?.map(|c| c.id);
        let has_rules =
            self.engine.store().cheats(game.id)?.iter().any(|c| c.freeze_enabled && c.frozen_value.is_some());
        let (latency, trainer_commits) = if has_rules { self.inject_all(&game)? } else { (None, Vec::new()) };
        let text = match (snap, &latency) {
            (Some(c), Some(l)) => format!("snapshot #{c} + template ({:.1} ms)", l.ms),
            (Some(c), None) => format!("snapshot #{c}"),
            (None, Some(l)) => format!("no changes to snapshot; template applied ({:.1} ms)", l.ms),
            (None, None) => "no changes since the last snapshot".to_string(),
        };
        self.notify(NoticeLevel::Info, text);
        self.refresh(true)?;
        Ok(SnapshotOutcome { snapshot: snap, trainer_commits, latency })
    }

    /// Restores the previous commit of the current branch.
    pub fn quick_rollback(&mut self) -> Result<Vec<String>> {
        let game = self.require_game()?;
        let rep = self.engine.quick_rollback(game.id)?;
        let text = if rep.written.is_empty() {
            "rollback: files already match the previous commit".to_string()
        } else {
            format!("rolled back {} to #{}", rep.written.join(", "), rep.target)
        };
        self.notify(NoticeLevel::Info, text);
        self.refresh(true)?;
        Ok(rep.written)
    }

    /// Reverts the most recent mutation (of the selected game, or anywhere
    /// when no game is selected) to its pre-mutation bytes.
    pub fn panic_restore(&mut self) -> Result<PanicReport> {
        let scope = match self.selected {
            Some(id) => PanicScope::Game(id),
            None => PanicScope::Latest,
        };
        let rep = self.engine.panic_restore(&scope)?;
        self.notify(NoticeLevel::Warn, format!("PANIC RESTORE: {} ⬅ {}", rep.path.display(), rep.restored_tag));
        if self.selected.is_some() {
            self.refresh(true)?;
        }
        Ok(rep)
    }

    // ------------------------------------------------------------------
    // Presets
    // ------------------------------------------------------------------

    /// Built-in presets followed by the custom presets of the selected game
    /// (a custom preset with the same name hides the built-in one).
    pub fn presets(&self) -> Vec<Preset> {
        let Some(id) = self.selected else { return Vec::new() };
        let custom = self.presets.presets(id);
        let aliases: Vec<(String, Option<ScalarValue>)> =
            self.rows.iter().map(|r| (r.alias.clone(), r.current)).collect();
        let mut out: Vec<Preset> = builtin_presets(&aliases)
            .into_iter()
            .filter(|b| !custom.iter().any(|c| c.name.eq_ignore_ascii_case(&b.name)))
            .collect();
        out.extend(custom.iter().cloned());
        out
    }

    /// Path of the presets file.
    pub fn presets_path(&self) -> &std::path::Path {
        self.presets.path()
    }

    /// Saves the current freeze configuration as a custom preset: every
    /// variable with a frozen value (or, failing that, its current value).
    pub fn save_preset(&mut self, name: &str) -> Result<Preset> {
        let game = self.require_game()?;
        self.refresh(false)?;
        let entries: Vec<PresetEntry> = self
            .rows
            .iter()
            .filter_map(|r| {
                let value = r.frozen_value.or(if r.freeze_enabled { r.current } else { None })?;
                Some(PresetEntry { alias: r.alias.clone(), value: value.to_string(), freeze: r.freeze_enabled })
            })
            .collect();
        if entries.is_empty() {
            bail!("nothing to save: set a frozen value on at least one variable");
        }
        let preset = Preset { name: name.trim().to_string(), entries, builtin: false };
        self.presets.upsert(game.id, &game.name, preset.clone())?;
        self.notify(NoticeLevel::Info, format!("preset `{}` saved", preset.name));
        Ok(preset)
    }

    /// Applies a preset to the freeze rules (the save file is written by the
    /// daemon on the next save, or immediately with "Apply now"). Returns
    /// the aliases of the preset that are not pinned in this game.
    pub fn apply_preset(&mut self, name: &str) -> Result<Vec<String>> {
        let game = self.require_game()?;
        let preset = self
            .presets()
            .into_iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| anyhow!("no preset named `{name}`"))?;
        let mut parsed = Vec::new();
        for e in &preset.entries {
            parsed.push((e.alias.clone(), e.scalar()?, e.freeze));
        }
        let mut missing = Vec::new();
        for (alias, value, freeze) in parsed {
            match self.engine.store().set_freeze(game.id, &alias, Some(value), freeze) {
                Ok(()) => {}
                Err(savelens_cas::CasError::NotFound { .. }) => missing.push(alias),
                Err(e) => return Err(e.into()),
            }
        }
        let mut text = format!("preset `{}` loaded", preset.name);
        if !missing.is_empty() {
            text.push_str(&format!(" (not pinned here: {})", missing.join(", ")));
        }
        self.notify(if missing.is_empty() { NoticeLevel::Info } else { NoticeLevel::Warn }, text);
        self.refresh(false)?;
        Ok(missing)
    }

    /// Deletes a custom preset.
    pub fn delete_preset(&mut self, name: &str) -> Result<()> {
        let game = self.require_game()?;
        self.presets.remove(game.id, name)?;
        self.notify(NoticeLevel::Info, format!("preset `{name}` deleted"));
        Ok(())
    }

    // ------------------------------------------------------------------
    // Event loop
    // ------------------------------------------------------------------

    /// Runs a hotkey action. Errors become notices.
    pub fn handle_action(&mut self, action: HotkeyAction) -> Vec<OverlayCommand> {
        let result: Result<Option<OverlayCommand>> = match action {
            HotkeyAction::ToggleTrainer => {
                let on = !self.trainer_enabled();
                self.set_trainer_enabled(on).map(|()| {
                    self.notify(NoticeLevel::Info, format!("trainer {}", if on { "ON" } else { "OFF" }));
                    None
                })
            }
            HotkeyAction::QuickSnapshot => self.quick_snapshot().map(|_| None),
            HotkeyAction::QuickRollback => self.quick_rollback().map(|_| None),
            HotkeyAction::PanicRestore => self.panic_restore().map(|_| None),
            HotkeyAction::ToggleOverlay => Ok(Some(self.toggle_visible())),
        };
        match result {
            Ok(cmd) => cmd.into_iter().collect(),
            Err(e) => {
                self.notify(NoticeLevel::Error, format!("{}: {e:#}", action.label()));
                Vec::new()
            }
        }
    }

    /// Flips the visibility of the overlay window.
    pub fn toggle_visible(&mut self) -> OverlayCommand {
        self.visible = !self.visible;
        OverlayCommand::SetVisible(self.visible)
    }

    /// Records that the window was shown/hidden by the UI itself.
    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    fn handle_daemon_event(&mut self, ev: DaemonEvent) {
        let selected_name = self.selected_game().map(|g| g.name.clone());
        let ours = |game: &str| selected_name.as_deref() == Some(game);
        match ev {
            DaemonEvent::TrainerApplied { game, rel_path, applied, elapsed_ms } => {
                if ours(&game) {
                    self.latency = Some(LatencySample {
                        ms: elapsed_ms,
                        source: LatencySource::Daemon,
                        rel_path: rel_path.clone(),
                        applied: applied.clone(),
                        at: Instant::now(),
                    });
                    self.refresh_pending = true;
                }
                self.push_activity(format!("❄ {game}: {} ➡ {rel_path} ({elapsed_ms:.1} ms)", applied.join(", ")));
            }
            DaemonEvent::Committed { game_id, game, summary, commit, .. } => {
                if self.selected == Some(game_id) {
                    self.refresh_pending = true;
                }
                self.push_activity(format!("#{commit} {game}: {summary}"));
            }
            DaemonEvent::PanicRestored { path, tag } => {
                self.refresh_pending = true;
                self.push_activity(format!("panic restore {} ⬅ {tag}", path.display()));
            }
            DaemonEvent::Rollback { game, files } => {
                self.refresh_pending = true;
                self.push_activity(format!("rollback {game}: {}", files.join(", ")));
            }
            DaemonEvent::TrainerToggled { game, enabled } => {
                self.refresh_pending = true;
                self.push_activity(format!("trainer {} for {game}", if enabled { "ON" } else { "OFF" }));
            }
            DaemonEvent::Snapshot { game, commit } => {
                self.refresh_pending = true;
                self.push_activity(match commit {
                    Some(c) => format!("snapshot #{c} of {game}"),
                    None => format!("snapshot of {game}: no changes"),
                });
            }
            DaemonEvent::Questions { game_id, game, rel_path, questions } => {
                let what: Vec<String> = questions.iter().map(|q| q.describe()).collect();
                self.push_activity(format!("❓ {game} {rel_path}: {}", what.join(", ")));
                if self.selected == Some(game_id) {
                    if let Err(e) = self.reload_questions() {
                        self.notify(NoticeLevel::Error, format!("questions: {e:#}"));
                    }
                }
            }
            DaemonEvent::Locked { path } => {
                self.push_activity(format!("{} locked by the game (read-only snapshot)", path.display()));
            }
            DaemonEvent::Warning { message } => self.notify(NoticeLevel::Warn, message),
            DaemonEvent::Error { message } => self.notify(NoticeLevel::Error, message),
            DaemonEvent::Stopped => {
                self.daemon_mode = DaemonMode::Absent;
                self.notify(NoticeLevel::Warn, "embedded daemon stopped");
            }
            // The embedded daemon runs without hotkeys, and the overlay
            // serves `toggle-overlay` requests itself (see `bus`).
            DaemonEvent::OverlayToggleRequested
            | DaemonEvent::Started { .. }
            | DaemonEvent::Watching { .. }
            | DaemonEvent::Unwatched { .. }
            | DaemonEvent::Unchanged { .. } => {}
        }
    }

    fn push_activity(&mut self, line: String) {
        self.activity.push_back(line);
        while self.activity.len() > MAX_ACTIVITY {
            self.activity.pop_front();
        }
    }

    fn handle_store_event(&mut self, ev: EventRecord) {
        let ours = ev.game_id.is_some() && ev.game_id == self.selected;
        let own = self.own_messages.iter().position(|m| *m == ev.message);
        if let Some(i) = own {
            self.own_messages.remove(i);
        }
        let trainer_commit = ev.kind == "commit" && ev.message.contains("[trainer]");
        if ours {
            if ev.kind == "commit" || ev.kind == "trainer" || ev.kind == "branch" || ev.kind == "watch" {
                self.refresh_pending = true;
            }
            if own.is_none() && (ev.kind == "trainer" || trainer_commit) {
                if let Some(ms) = parse_latency_ms(&ev.message) {
                    self.latency = Some(LatencySample {
                        ms,
                        source: LatencySource::EventLog,
                        rel_path: String::new(),
                        applied: Vec::new(),
                        at: Instant::now(),
                    });
                }
            }
        }
        if own.is_none() && (ours || ev.game_id.is_none()) && ev.kind != "track" {
            self.push_activity(format!("[{}] {}", ev.kind, ev.message));
        }
    }

    /// Reads new entries of the shared activity log.
    pub fn poll_store_events(&mut self) -> Result<()> {
        let mut events = self.engine.store().events(Some(self.event_cursor), 200)?;
        events.reverse();
        for ev in events {
            self.event_cursor = self.event_cursor.max(ev.id);
            self.handle_store_event(ev);
        }
        Ok(())
    }

    /// Serves `toggle-overlay` requests of the bus (others are left pending).
    pub fn poll_requests(&mut self) -> Result<Vec<OverlayCommand>> {
        let taken = bus::take_requests_matching(self.engine.store(), bus::TOGGLE_OVERLAY)?;
        Ok(taken.iter().map(|_| self.toggle_visible()).collect())
    }

    /// One step of the event loop: hotkeys, daemon events, activity log,
    /// request bus and the periodic (mtime-gated) refresh.
    pub fn tick(&mut self, now: Instant) -> Vec<OverlayCommand> {
        let mut commands = Vec::new();
        let actions: Vec<HotkeyAction> = self.actions_rx.try_iter().collect();
        for a in actions {
            commands.extend(self.handle_action(a));
        }
        let events: Vec<DaemonEvent> = self.daemon_rx.try_iter().collect();
        for ev in events {
            self.handle_daemon_event(ev);
        }
        if self.last_event_poll.is_none_or(|t| now.duration_since(t) >= EVENT_POLL) {
            self.last_event_poll = Some(now);
            if let Err(e) = self.poll_store_events() {
                self.notify(NoticeLevel::Error, format!("activity log: {e:#}"));
            }
        }
        if self.last_request_poll.is_none_or(|t| now.duration_since(t) >= REQUEST_POLL) {
            self.last_request_poll = Some(now);
            match self.poll_requests() {
                Ok(c) => commands.extend(c),
                Err(e) => self.notify(NoticeLevel::Error, format!("request bus: {e:#}")),
            }
        }
        let due = self.last_refresh.is_none_or(|t| now.duration_since(t) >= REFRESH_INTERVAL);
        if self.refresh_pending || due {
            if self.selected.is_none() {
                if let Err(e) = self.reload_games() {
                    self.notify(NoticeLevel::Error, format!("{e:#}"));
                }
            }
            let force = self.refresh_pending;
            if let Err(e) = self.refresh(force) {
                self.notify(NoticeLevel::Error, format!("{e:#}"));
            }
            self.last_refresh = Some(now);
        }
        commands
    }
}

/// Forwards every message of `rx` to `tx` and wakes the UI. Ends when
/// either side disconnects.
fn spawn_forwarder<T: Send + 'static>(
    name: &str,
    rx: Receiver<T>,
    tx: Sender<T>,
    waker: Arc<Mutex<Option<Waker>>>,
) -> Result<()> {
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            for msg in rx.iter() {
                if tx.send(msg).is_err() {
                    break;
                }
                let wake = waker.lock().ok().and_then(|w| w.clone());
                if let Some(wake) = wake {
                    wake();
                }
            }
        })
        .map(|_| ())
        .with_context(|| format!("cannot spawn the {name} thread"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_parsing() {
        assert_eq!(parse_latency_ms("overlay injected Gold into a.json in 12.5 ms"), Some(12.5));
        assert_eq!(parse_latency_ms("trainer applied (3ms)"), Some(3.0));
        assert_eq!(parse_latency_ms("#4 [trainer] freeze-on-save: Gold"), None);
        assert_eq!(parse_latency_ms("took NaN ms"), None);
    }

    #[test]
    fn trend_and_drift() {
        let mut r = CheatRow {
            alias: "Gold".into(),
            rel_path: "a.json".into(),
            location: "Player.Gold".into(),
            current: Some(ScalarValue::Int(5)),
            error: None,
            freeze_enabled: true,
            frozen_value: Some(ScalarValue::Int(9)),
            history: VecDeque::from(vec![1.0]),
        };
        assert_eq!(r.trend(), Trend::Unknown);
        assert!(r.drifted());
        r.history.push_back(4.0);
        assert_eq!(r.trend(), Trend::Up);
        r.history.push_back(2.0);
        assert_eq!(r.trend(), Trend::Down);
        r.freeze_enabled = false;
        assert!(!r.drifted());
    }
}
