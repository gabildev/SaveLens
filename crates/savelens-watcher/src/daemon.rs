//! Background watcher daemon.
//!
//! * Watches every tracked save root with `notify` (inotify / ReadDirectoryChangesW).
//! * Debounces events per file; an `IN_CLOSE_WRITE` shortens the quiet period.
//! * Before reading: size/mtime stability check and write-lock check (no
//!   process may hold the file open for writing).
//! * Auto-commits with a semantic changelog, then (if enabled) runs the disk
//!   trainer: frozen values are injected atomically and re-signed.
//! * Serves requests queued in the store by other processes (overlay, CLI)
//!   and global hotkeys (snapshot, rollback, panic restore, trainer toggle).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, select, tick, unbounded, Receiver, Sender};
use notify::event::{AccessKind, AccessMode, ModifyKind, RenameMode};
use notify::{EventKind, RecursiveMode, Watcher};
use savelens_cas::{CommitKind, GameId, GameRecord, PanicScope};
use serde::{Deserialize, Serialize};

use crate::engine::Engine;
use crate::error::{EngineError, EngineResult};
use crate::hotkeys::{HotkeyAction, HotkeyService};

/// Events emitted by the daemon (for UIs and the CLI).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DaemonEvent {
    Started { games: usize, hotkeys: Vec<String> },
    Watching { game: String, root: PathBuf },
    Unwatched { game: String },
    Committed { game_id: GameId, game: String, rel_path: String, commit: i64, summary: String },
    Unchanged { game: String, rel_path: String },
    TrainerApplied { game: String, rel_path: String, applied: Vec<String>, elapsed_ms: f64 },
    Locked { path: PathBuf },
    PanicRestored { path: PathBuf, tag: String },
    Snapshot { game: String, commit: Option<i64> },
    Rollback { game: String, files: Vec<String> },
    TrainerToggled { game: String, enabled: bool },
    OverlayToggleRequested,
    Warning { message: String },
    Error { message: String },
    Stopped,
}

/// Daemon options.
#[derive(Debug, Clone)]
pub struct DaemonOptions {
    /// Register global hotkeys.
    pub hotkeys: bool,
    /// Only watch these games (all watch-enabled games when `None`).
    pub games: Option<Vec<GameId>>,
    /// Main loop tick.
    pub tick: Duration,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        DaemonOptions {
            hotkeys: true,
            games: None,
            tick: Duration::from_millis(50),
        }
    }
}

/// Handle to a running daemon.
pub struct DaemonHandle {
    stop: Sender<()>,
    events: Receiver<DaemonEvent>,
    thread: Option<JoinHandle<EngineResult<()>>>,
}

impl DaemonHandle {
    /// Event stream.
    pub fn events(&self) -> &Receiver<DaemonEvent> {
        &self.events
    }

    /// Stops the daemon and waits for it.
    pub fn stop(mut self) -> EngineResult<()> {
        let _ = self.stop.send(());
        match self.thread.take() {
            Some(t) => t
                .join()
                .map_err(|_| EngineError::Watch("daemon thread panicked".into()))?,
            None => Ok(()),
        }
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[derive(Debug, Clone)]
struct Pending {
    game_id: GameId,
    rel: String,
    last_event: Instant,
    close_write: bool,
    locked_since: Option<Instant>,
}

/// The daemon.
pub struct Daemon {
    engine: Engine,
    opts: DaemonOptions,
}

fn is_relevant(kind: &EventKind) -> (bool, bool) {
    match kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => (true, true),
        EventKind::Create(_) => (true, false),
        EventKind::Modify(ModifyKind::Data(_)) | EventKind::Modify(ModifyKind::Any) | EventKind::Modify(ModifyKind::Other) => (true, false),
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) | EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => (true, true),
        EventKind::Modify(ModifyKind::Name(RenameMode::Any)) => (true, false),
        _ => (false, false),
    }
}

fn blake(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

impl Daemon {
    pub fn new(engine: Engine, opts: DaemonOptions) -> Self {
        Daemon { engine, opts }
    }

    /// Spawns the daemon on its own thread.
    pub fn spawn(engine: Engine, opts: DaemonOptions) -> EngineResult<DaemonHandle> {
        let (stop_tx, stop_rx) = bounded::<()>(1);
        let (ev_tx, ev_rx) = unbounded::<DaemonEvent>();
        let daemon = Daemon::new(engine, opts);
        let thread = std::thread::Builder::new()
            .name("savelens-daemon".into())
            .spawn(move || daemon.run(stop_rx, ev_tx))
            .map_err(|e| EngineError::Watch(format!("cannot spawn daemon: {e}")))?;
        Ok(DaemonHandle {
            stop: stop_tx,
            events: ev_rx,
            thread: Some(thread),
        })
    }

    fn games(&self) -> EngineResult<Vec<GameRecord>> {
        let all = self.engine.store().games()?;
        Ok(all
            .into_iter()
            .filter(|g| g.watch_enabled)
            .filter(|g| self.opts.games.as_ref().is_none_or(|ids| ids.contains(&g.id)))
            .collect())
    }

    /// Runs until `stop` receives a message.
    pub fn run(&self, stop: Receiver<()>, events: Sender<DaemonEvent>) -> EngineResult<()> {
        let cfg = self.engine.config().clone();
        let (fs_tx, fs_rx) = unbounded::<notify::Result<notify::Event>>();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = fs_tx.send(res);
        })?;
        let mut watched: HashMap<GameId, (PathBuf, String)> = HashMap::new();
        let mut games: Vec<GameRecord> = Vec::new();
        let mut pending: HashMap<PathBuf, Pending> = HashMap::new();
        let mut own_writes: HashMap<PathBuf, (String, Instant)> = HashMap::new();
        let mut active_game: Option<GameId> = None;

        let hotkeys = if self.opts.hotkeys && cfg.hotkeys.enabled {
            match HotkeyService::start(&cfg.hotkeys) {
                Ok(h) => Some(h),
                Err(e) => {
                    let _ = events.send(DaemonEvent::Warning {
                        message: format!("global hotkeys disabled: {e}"),
                    });
                    None
                }
            }
        } else {
            None
        };
        let hotkey_rx = hotkeys.as_ref().map(|h| h.actions().clone()).unwrap_or_else(crossbeam_channel::never);

        let refresh = |watcher: &mut notify::RecommendedWatcher,
                           watched: &mut HashMap<GameId, (PathBuf, String)>,
                           games: &mut Vec<GameRecord>,
                           events: &Sender<DaemonEvent>|
         -> EngineResult<()> {
            *games = self.games()?;
            for g in games.iter() {
                if !watched.contains_key(&g.id) {
                    match watcher.watch(&g.save_root, RecursiveMode::Recursive) {
                        Ok(()) => {
                            watched.insert(g.id, (g.save_root.clone(), g.name.clone()));
                            let _ = events.send(DaemonEvent::Watching {
                                game: g.name.clone(),
                                root: g.save_root.clone(),
                            });
                        }
                        Err(e) => {
                            let _ = events.send(DaemonEvent::Warning {
                                message: format!("cannot watch {}: {e}", g.save_root.display()),
                            });
                        }
                    }
                }
            }
            let gone: Vec<GameId> = watched.keys().filter(|id| !games.iter().any(|g| g.id == **id)).copied().collect();
            for id in gone {
                if let Some((root, name)) = watched.remove(&id) {
                    let _ = watcher.unwatch(&root);
                    let _ = events.send(DaemonEvent::Unwatched { game: name });
                }
            }
            Ok(())
        };

        refresh(&mut watcher, &mut watched, &mut games, &events)?;
        let _ = events.send(DaemonEvent::Started {
            games: games.len(),
            hotkeys: hotkeys
                .as_ref()
                .map(|h| h.bindings().iter().map(|(a, k)| format!("{k}: {}", a.label())).collect())
                .unwrap_or_default(),
        });
        self.engine.store().log_event(None, "daemon", &format!("daemon started, watching {} game(s)", games.len()))?;
        self.engine.store().heartbeat(&format!("watching {} game(s)", watched.len()))?;

        let ticker = tick(self.opts.tick);
        let mut last_refresh = Instant::now();
        let mut last_requests = Instant::now();
        let debounce = Duration::from_millis(cfg.debounce_ms);
        let close_quiet = Duration::from_millis(cfg.close_write_quiet_ms.min(cfg.debounce_ms));
        let lock_timeout = Duration::from_millis(cfg.lock_timeout_ms);

        loop {
            select! {
                recv(stop) -> _ => break,
                recv(fs_rx) -> msg => {
                    match msg {
                        Ok(Ok(ev)) => {
                            let (relevant, closed) = is_relevant(&ev.kind);
                            if !relevant { continue; }
                            for p in ev.paths {
                                let Some(g) = games.iter().find(|g| p.starts_with(&g.save_root)) else { continue };
                                let Some(rel) = self.engine.rel_path_for(g, &p) else { continue };
                                let entry = pending.entry(p.clone()).or_insert(Pending {
                                    game_id: g.id,
                                    rel,
                                    last_event: Instant::now(),
                                    close_write: false,
                                    locked_since: None,
                                });
                                entry.last_event = Instant::now();
                                entry.close_write |= closed;
                            }
                        }
                        Ok(Err(e)) => { let _ = events.send(DaemonEvent::Warning { message: format!("watch error: {e}") }); }
                        Err(_) => return Err(EngineError::Watch("filesystem watcher channel closed".into())),
                    }
                }
                recv(hotkey_rx) -> action => {
                    if let Ok(action) = action {
                        self.handle_action(action, active_game, &games, &events, &mut own_writes);
                    }
                }
                recv(ticker) -> _ => {
                    let now = Instant::now();
                    let ready: Vec<PathBuf> = pending
                        .iter()
                        .filter(|(_, p)| {
                            let quiet = if p.close_write { close_quiet } else { debounce };
                            now.duration_since(p.last_event) >= quiet
                        })
                        .map(|(k, _)| k.clone())
                        .collect();
                    for path in ready {
                        let Some(mut p) = pending.remove(&path) else { continue };
                        match self.process(&path, &mut p, lock_timeout, &mut own_writes, &events) {
                            Ok(Some(game)) => active_game = Some(game),
                            Ok(None) => {}
                            Err(Retry) => {
                                p.last_event = Instant::now();
                                pending.insert(path, p);
                            }
                        }
                    }
                    own_writes.retain(|_, (_, t)| now.duration_since(*t) < Duration::from_secs(60));
                    if now.duration_since(last_refresh) >= Duration::from_secs(2) {
                        last_refresh = now;
                        if let Err(e) = refresh(&mut watcher, &mut watched, &mut games, &events) {
                            let _ = events.send(DaemonEvent::Error { message: e.to_string() });
                        }
                        let info = format!("watching {} game(s), {} pending", watched.len(), pending.len());
                        let _ = self.engine.store().heartbeat(&info);
                    }
                    if now.duration_since(last_requests) >= Duration::from_millis(250) {
                        last_requests = now;
                        self.handle_requests(active_game, &games, &events, &mut own_writes);
                    }
                }
            }
        }
        drop(hotkeys);
        let _ = self.engine.store().clear_heartbeat();
        let _ = self.engine.store().log_event(None, "daemon", "daemon stopped");
        let _ = events.send(DaemonEvent::Stopped);
        Ok(())
    }

    fn process(
        &self,
        path: &Path,
        p: &mut Pending,
        lock_timeout: Duration,
        own_writes: &mut HashMap<PathBuf, (String, Instant)>,
        events: &Sender<DaemonEvent>,
    ) -> Result<Option<GameId>, Retry> {
        let cfg = self.engine.config();
        let Ok(meta1) = std::fs::metadata(path) else { return Ok(None) };
        if !meta1.is_file() {
            return Ok(None);
        }
        // Stability: size and mtime must not move during the sample window.
        std::thread::sleep(Duration::from_millis(cfg.stability_ms));
        let Ok(meta2) = std::fs::metadata(path) else { return Ok(None) };
        if meta1.len() != meta2.len() || meta1.modified().ok() != meta2.modified().ok() {
            return Err(Retry);
        }
        // Lock check: never read while the game holds the file open for writing.
        let locked = savelens_discovery::process::is_locked_for_write(path);
        if locked {
            let since = *p.locked_since.get_or_insert_with(Instant::now);
            if since.elapsed() < lock_timeout {
                return Err(Retry);
            }
            let _ = events.send(DaemonEvent::Locked { path: path.to_path_buf() });
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                let _ = events.send(DaemonEvent::Warning {
                    message: format!("cannot read {}: {e}", path.display()),
                });
                return Ok(None);
            }
        };
        let hash = blake(&bytes);
        if own_writes.get(path).is_some_and(|(h, _)| *h == hash) {
            return Ok(Some(p.game_id));
        }
        let game = match self.engine.game(p.game_id) {
            Ok(g) => g,
            Err(e) => {
                let _ = events.send(DaemonEvent::Error { message: e.to_string() });
                return Ok(None);
            }
        };
        if cfg.auto_commit {
            match self.engine.commit_file(game.id, &p.rel, bytes, CommitKind::Auto, "game saved") {
                Ok(Some(c)) => {
                    let _ = events.send(DaemonEvent::Committed {
                        game_id: game.id,
                        game: game.name.clone(),
                        rel_path: p.rel.clone(),
                        commit: c.id,
                        summary: c.summary.clone(),
                    });
                }
                Ok(None) => {
                    let _ = events.send(DaemonEvent::Unchanged {
                        game: game.name.clone(),
                        rel_path: p.rel.clone(),
                    });
                }
                Err(e) => {
                    let _ = events.send(DaemonEvent::Error {
                        message: format!("commit of {} failed: {e}", p.rel),
                    });
                }
            }
        }
        if game.trainer_enabled && !locked {
            match self.engine.inject_trainer(game.id, &p.rel) {
                Ok(Some(rep)) => {
                    if let Ok(now) = std::fs::read(path) {
                        own_writes.insert(path.to_path_buf(), (blake(&now), Instant::now()));
                    }
                    let _ = events.send(DaemonEvent::TrainerApplied {
                        game: game.name.clone(),
                        rel_path: rep.rel_path,
                        applied: rep.applied,
                        elapsed_ms: rep.elapsed_ms,
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = events.send(DaemonEvent::Error {
                        message: format!("trainer on {} failed: {e}", p.rel),
                    });
                }
            }
        }
        Ok(Some(game.id))
    }

    fn remember_game_files(&self, game_id: GameId, own_writes: &mut HashMap<PathBuf, (String, Instant)>) {
        if let Ok(g) = self.engine.game(game_id) {
            if let Ok(files) = self.engine.tracked_files(&g) {
                for f in files {
                    if let Ok(b) = std::fs::read(&f.abs_path) {
                        own_writes.insert(f.abs_path, (blake(&b), Instant::now()));
                    }
                }
            }
        }
    }

    fn target_game(&self, explicit: Option<GameId>, active: Option<GameId>, games: &[GameRecord]) -> Option<GameRecord> {
        let id = explicit.or(active).or_else(|| games.first().map(|g| g.id))?;
        self.engine.game(id).ok()
    }

    fn handle_action(
        &self,
        action: HotkeyAction,
        active: Option<GameId>,
        games: &[GameRecord],
        events: &Sender<DaemonEvent>,
        own_writes: &mut HashMap<PathBuf, (String, Instant)>,
    ) {
        self.run_action(action, None, active, games, events, own_writes);
    }

    fn run_action(
        &self,
        action: HotkeyAction,
        game: Option<GameId>,
        active: Option<GameId>,
        games: &[GameRecord],
        events: &Sender<DaemonEvent>,
        own_writes: &mut HashMap<PathBuf, (String, Instant)>,
    ) {
        let err = |e: EngineError| {
            let _ = events.send(DaemonEvent::Error {
                message: format!("{}: {e}", action.label()),
            });
        };
        match action {
            HotkeyAction::QuickSnapshot => {
                let targets: Vec<GameRecord> = match self.target_game(game, active, games) {
                    Some(g) if game.is_some() || active.is_some() => vec![g],
                    _ => games.to_vec(),
                };
                for g in targets {
                    match self.engine.snapshot(g.id, "quick snapshot (hotkey)", CommitKind::Manual) {
                        Ok(c) => {
                            let _ = events.send(DaemonEvent::Snapshot {
                                game: g.name.clone(),
                                commit: c.map(|c| c.id),
                            });
                        }
                        Err(e) => err(e),
                    }
                }
            }
            HotkeyAction::QuickRollback => {
                let Some(g) = self.target_game(game, active, games) else { return };
                match self.engine.quick_rollback(g.id) {
                    Ok(rep) => {
                        self.remember_game_files(g.id, own_writes);
                        let _ = events.send(DaemonEvent::Rollback {
                            game: g.name.clone(),
                            files: rep.written,
                        });
                    }
                    Err(e) => err(e),
                }
            }
            HotkeyAction::PanicRestore => {
                let scope = match game {
                    Some(id) => PanicScope::Game(id),
                    None => PanicScope::Latest,
                };
                match self.engine.panic_restore(&scope) {
                    Ok(rep) => {
                        if let Ok(b) = std::fs::read(&rep.path) {
                            own_writes.insert(rep.path.clone(), (blake(&b), Instant::now()));
                        }
                        let _ = events.send(DaemonEvent::PanicRestored {
                            path: rep.path,
                            tag: rep.restored_tag,
                        });
                    }
                    Err(e) => err(e),
                }
            }
            HotkeyAction::ToggleTrainer => {
                let Some(g) = self.target_game(game, active, games) else { return };
                let enabled = !g.trainer_enabled;
                match self.engine.store().set_trainer_enabled(g.id, enabled) {
                    Ok(()) => {
                        let _ = self.engine.store().log_event(
                            Some(g.id),
                            "trainer",
                            &format!("disk trainer {}", if enabled { "enabled" } else { "disabled" }),
                        );
                        let _ = events.send(DaemonEvent::TrainerToggled { game: g.name.clone(), enabled });
                    }
                    Err(e) => err(e.into()),
                }
            }
            HotkeyAction::ToggleOverlay => {
                let _ = self.engine.store().push_request("toggle-overlay", &serde_json::json!({}));
                let _ = events.send(DaemonEvent::OverlayToggleRequested);
            }
        }
    }

    fn handle_requests(
        &self,
        active: Option<GameId>,
        games: &[GameRecord],
        events: &Sender<DaemonEvent>,
        own_writes: &mut HashMap<PathBuf, (String, Instant)>,
    ) {
        let reqs = match self.engine.store().take_requests() {
            Ok(r) => r,
            Err(e) => {
                let _ = events.send(DaemonEvent::Error { message: e.to_string() });
                return;
            }
        };
        for (_, action, payload) in reqs {
            let game = payload.get("game").and_then(|v| v.as_i64());
            let act = match action.as_str() {
                "snapshot" => Some(HotkeyAction::QuickSnapshot),
                "rollback" => Some(HotkeyAction::QuickRollback),
                "panic-restore" => Some(HotkeyAction::PanicRestore),
                "toggle-trainer" => Some(HotkeyAction::ToggleTrainer),
                // Overlay toggles are consumed by the overlay process itself.
                "toggle-overlay" => None,
                other => {
                    let _ = events.send(DaemonEvent::Warning {
                        message: format!("unknown request `{other}`"),
                    });
                    None
                }
            };
            if let Some(a) = act {
                self.run_action(a, game, active, games, events, own_writes);
            }
        }
    }
}

/// Marker: the file is not ready yet, try again later.
#[derive(Debug)]
struct Retry;
