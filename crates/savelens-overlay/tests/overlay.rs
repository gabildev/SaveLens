//! Headless end-to-end tests of the overlay: a real engine and store in a
//! temporary directory, a JSON save tracked with `Engine::track` and a pinned
//! `Gold` variable at `Player.Gold`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use savelens_cas::{CommitKind, Store};
use savelens_codecs::EnvelopePipeline;
use savelens_core::{Location, NodePath, ScalarValue};
use savelens_overlay::bus;
use savelens_overlay::*;
use savelens_watcher::{Engine, HotkeyAction, HotkeyConfig, SaveLensConfig, WORKING};

fn save_json(gold: i64, hp: f64, level: i64) -> Vec<u8> {
    format!(
        "{{\n  \"Player\": {{\n    \"Gold\": {gold},\n    \"HP\": {hp:.1},\n    \"Level\": {level}\n  }}\n}}\n"
    )
    .into_bytes()
}

fn listing(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect()
}

struct Env {
    _tmp: tempfile::TempDir,
    engine: Engine,
    saves: PathBuf,
    data: PathBuf,
    game: i64,
}

impl Env {
    fn save_path(&self) -> PathBuf {
        self.saves.join("slot1.json")
    }

    fn gold_on_disk(&self) -> Option<ScalarValue> {
        let root = self.engine.root_at(self.game, WORKING, "slot1.json").unwrap();
        Location::Ast {
            path: NodePath::parse("Player.Gold").unwrap(),
        }
        .read(&root)
    }

    fn controller(&self) -> OverlayController {
        OverlayController::new(self.engine.clone(), &quiet_opts()).unwrap()
    }
}

fn quiet_opts() -> OverlayOptions {
    OverlayOptions {
        game: None,
        opacity: 0.85,
        embedded_daemon: false,
        hotkeys: false,
    }
}

fn env_with(config: SaveLensConfig) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let saves = tmp.path().join("Saves");
    fs::create_dir_all(&saves).unwrap();
    fs::write(saves.join("slot1.json"), save_json(1200, 87.5, 7)).unwrap();
    let data = tmp.path().join("data");
    let engine = Engine::new(Store::open(&data).unwrap(), EnvelopePipeline::builtin(), config);
    let (game, _) = engine
        .track("Test RPG", Some(42), &saves, &["*.json".to_string()], "native")
        .unwrap();
    engine
        .pin(
            game.id,
            "slot1.json",
            "Gold",
            &Location::Ast {
                path: NodePath::parse("Player.Gold").unwrap(),
            },
        )
        .unwrap();
    Env {
        saves: saves.canonicalize().unwrap(),
        data,
        engine,
        game: game.id,
        _tmp: tmp,
    }
}

fn env() -> Env {
    env_with(SaveLensConfig::default())
}

fn set_mtime(path: &Path, t: SystemTime) {
    fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
}

#[test]
fn controller_loads_the_cheat_table_with_live_values() {
    let e = env();
    e.engine
        .pin(
            e.game,
            "slot1.json",
            "Vida",
            &Location::Ast {
                path: NodePath::parse("Player.HP").unwrap(),
            },
        )
        .unwrap();
    let c = e.controller();
    assert_eq!(c.selected_game().map(|g| g.id), Some(e.game));
    let rows = c.rows();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].alias, "Gold");
    assert_eq!(rows[0].current, Some(ScalarValue::Int(1200)));
    assert_eq!(rows[0].location, "Player.Gold");
    assert_eq!(rows[1].alias, "Vida");
    assert_eq!(rows[1].current.map(|v| v.as_f64()), Some(87.5));
    assert!(rows.iter().all(|r| r.error.is_none() && !r.freeze_enabled));
    assert_eq!(c.disk_reads(), 1);
    assert!(!c.trainer_enabled());
}

#[test]
fn refresh_only_rereads_when_the_mtime_changes() {
    let e = env();
    let mut c = e.controller();
    let reads = c.disk_reads();
    assert!(!c.refresh(false).unwrap(), "nothing changed on disk");
    assert!(!c.refresh(false).unwrap());
    assert_eq!(c.disk_reads(), reads);

    // Same size and mtime restored: the stamp is identical, so no re-read
    // (this is what keeps the 1 s polling cheap).
    let old_mtime = fs::metadata(e.save_path()).unwrap().modified().unwrap();
    fs::write(e.save_path(), save_json(1300, 87.5, 7)).unwrap();
    set_mtime(&e.save_path(), old_mtime);
    assert!(!c.refresh(false).unwrap());
    assert_eq!(c.row("Gold").unwrap().current, Some(ScalarValue::Int(1200)));
    assert_eq!(c.disk_reads(), reads);

    // New mtime: re-read, history and trend follow.
    set_mtime(&e.save_path(), old_mtime + Duration::from_secs(5));
    assert!(c.refresh(false).unwrap());
    assert_eq!(c.disk_reads(), reads + 1);
    let row = c.row("Gold").unwrap();
    assert_eq!(row.current, Some(ScalarValue::Int(1300)));
    assert_eq!(row.history.iter().copied().collect::<Vec<_>>(), vec![1200.0, 1300.0]);
    assert_eq!(row.trend(), Trend::Up);

    // The periodic tick also goes through the mtime gate.
    let now = Instant::now() + Duration::from_secs(2);
    c.tick(now);
    assert_eq!(c.disk_reads(), reads + 1);
    fs::write(e.save_path(), save_json(900, 87.5, 7)).unwrap();
    set_mtime(&e.save_path(), old_mtime + Duration::from_secs(10));
    c.tick(now + Duration::from_secs(2));
    assert_eq!(c.disk_reads(), reads + 2);
    assert_eq!(c.row("Gold").unwrap().trend(), Trend::Down);
}

#[test]
fn freeze_toggle_and_value_persist_to_the_store() {
    let e = env();
    let mut c = e.controller();
    // Enabling without a value freezes the current on-disk value.
    c.set_freeze_enabled("Gold", true).unwrap();
    let entry = e.engine.store().cheat_by_alias(e.game, "Gold").unwrap();
    assert!(entry.freeze_enabled);
    assert_eq!(entry.frozen_value, Some(ScalarValue::Int(1200)));

    c.set_frozen_value("Gold", "777_777").unwrap();
    let entry = e.engine.store().cheat_by_alias(e.game, "Gold").unwrap();
    assert_eq!(entry.frozen_value, Some(ScalarValue::Int(777_777)));
    assert!(entry.freeze_enabled, "editing the value keeps the toggle");
    assert_eq!(c.row("Gold").unwrap().frozen_value, Some(ScalarValue::Int(777_777)));
    assert!(c.row("Gold").unwrap().drifted());

    c.set_freeze_enabled("Gold", false).unwrap();
    let entry = e.engine.store().cheat_by_alias(e.game, "Gold").unwrap();
    assert!(!entry.freeze_enabled);
    assert_eq!(entry.frozen_value, Some(ScalarValue::Int(777_777)), "value kept while unfrozen");

    assert!(c.set_frozen_value("Gold", "a lot").is_err());
    assert!(c.set_freeze_enabled("Nope", true).is_err());
    // A second controller (another process) sees the same state.
    let other = e.controller();
    assert_eq!(other.row("Gold").unwrap().frozen_value, Some(ScalarValue::Int(777_777)));
    // Clearing the value disables freezing.
    c.set_frozen_value("Gold", "  ").unwrap();
    let entry = e.engine.store().cheat_by_alias(e.game, "Gold").unwrap();
    assert_eq!((entry.frozen_value, entry.freeze_enabled), (None, false));
}

#[test]
fn master_trainer_switch_and_hotkey_toggle() {
    let e = env();
    let mut c = e.controller();
    c.set_trainer_enabled(true).unwrap();
    assert!(e.engine.store().game(e.game).unwrap().trainer_enabled);
    assert!(c.trainer_enabled());
    // The ToggleTrainer hotkey flips it back.
    c.queue_action(HotkeyAction::ToggleTrainer);
    c.tick(Instant::now());
    assert!(!e.engine.store().game(e.game).unwrap().trainer_enabled);
    assert!(!c.trainer_enabled());
    // Another process flips it: picked up on refresh.
    e.engine.store().set_trainer_enabled(e.game, true).unwrap();
    c.refresh(false).unwrap();
    assert!(c.trainer_enabled());
}

#[test]
fn apply_now_writes_the_file_and_records_latency() {
    let e = env();
    let mut c = e.controller();
    assert!(c.apply_now().is_err(), "nothing frozen yet");
    c.set_frozen_value("Gold", "999999").unwrap();
    c.set_freeze_enabled("Gold", true).unwrap();
    let before = fs::read(e.save_path()).unwrap();
    let sample = c.apply_now().unwrap().expect("injected");
    assert_eq!(sample.applied, vec!["Gold".to_string()]);
    assert_eq!(sample.source, LatencySource::Overlay);
    assert!(sample.ms > 0.0);
    assert_eq!(c.last_latency().map(|l| l.ms), Some(sample.ms));
    let after = fs::read(e.save_path()).unwrap();
    assert_ne!(before, after);
    assert_eq!(after, save_json(999_999, 87.5, 7), "only Gold changed, layout preserved");
    assert_eq!(e.gold_on_disk(), Some(ScalarValue::Int(999_999)));
    assert_eq!(c.row("Gold").unwrap().current, Some(ScalarValue::Int(999_999)));
    // The injection was recorded in the shared activity log with its latency.
    let ev = e.engine.store().events(None, 20).unwrap();
    assert!(ev.iter().any(|x| x.kind == "trainer" && x.message.contains("ms")));
    // Applying again is a no-op.
    assert!(c.apply_now().unwrap().is_none());
}

#[test]
fn presets_round_trip_in_the_data_dir_not_the_game_dir() {
    let e = env();
    let game_dir_before = listing(&e.saves);
    let mut c = e.controller();
    // Built-in "Max gold" is offered because `Gold` is a currency.
    let presets = c.presets();
    assert_eq!(presets.len(), 1);
    assert!(presets[0].builtin && presets[0].name == "Max gold");

    c.set_frozen_value("Gold", "4242").unwrap();
    c.set_freeze_enabled("Gold", true).unwrap();
    let saved = c.save_preset("Speedrun").unwrap();
    assert_eq!(saved.entries.len(), 1);
    assert_eq!(c.presets_path(), e.data.join(PRESETS_FILE));
    assert!(e.data.join(PRESETS_FILE).is_file());

    // Load "Max gold", then the custom preset again.
    let missing = c.apply_preset("max gold").unwrap();
    assert!(missing.is_empty());
    assert_eq!(c.row("Gold").unwrap().frozen_value, Some(ScalarValue::Int(999_999)));

    // A fresh controller (new process) reads the preset back from the data dir.
    let mut c2 = e.controller();
    let names: Vec<String> = c2.presets().into_iter().map(|p| p.name).collect();
    assert_eq!(names, vec!["Max gold".to_string(), "Speedrun".to_string()]);
    c2.apply_preset("Speedrun").unwrap();
    let entry = e.engine.store().cheat_by_alias(e.game, "Gold").unwrap();
    assert_eq!(entry.frozen_value, Some(ScalarValue::Int(4242)));
    assert!(entry.freeze_enabled);
    let raw = fs::read_to_string(e.data.join(PRESETS_FILE)).unwrap();
    assert!(raw.contains("Speedrun") && raw.contains("Test RPG"));

    c2.delete_preset("Speedrun").unwrap();
    assert!(c2.apply_preset("Speedrun").is_err());
    assert_eq!(listing(&e.saves), game_dir_before, "the game directory was never touched");
}

#[test]
fn quick_snapshot_with_template_creates_commits() {
    let e = env();
    let mut c = e.controller();
    c.set_frozen_value("Gold", "500000").unwrap();
    c.set_freeze_enabled("Gold", true).unwrap();
    let before = e.engine.log(e.game, None, 100).unwrap().len();
    // The game saved: gold spent.
    fs::write(e.save_path(), save_json(10, 50.0, 8)).unwrap();
    let out = c.quick_snapshot().unwrap();
    let snap = out.snapshot.expect("snapshot commit");
    assert_eq!(out.trainer_commits.len(), 1);
    assert!(out.latency.is_some());
    let log = e.engine.log(e.game, None, 100).unwrap();
    assert_eq!(log.len(), before + 2);
    let snap_commit = log.iter().find(|c| c.id == snap).unwrap();
    assert_eq!(snap_commit.kind, CommitKind::Manual);
    assert!(log.iter().any(|c| c.kind == CommitKind::Trainer && Some(&c.id) == out.trainer_commits.first()));
    // The snapshot holds the game's bytes; the disk holds the template.
    assert_eq!(e.engine.store().read_commit_file(snap, "slot1.json").unwrap(), save_json(10, 50.0, 8));
    assert_eq!(e.gold_on_disk(), Some(ScalarValue::Int(500_000)));

    // Quick rollback restores the previous commit (the snapshot).
    let written = c.quick_rollback().unwrap();
    assert_eq!(written, vec!["slot1.json".to_string()]);
    assert_eq!(fs::read(e.save_path()).unwrap(), save_json(10, 50.0, 8));
}

#[test]
fn panic_restore_restores_the_original_bytes() {
    let e = env();
    let original = fs::read(e.save_path()).unwrap();
    let mut c = e.controller();
    c.set_frozen_value("Gold", "1").unwrap();
    c.set_freeze_enabled("Gold", true).unwrap();
    c.apply_now().unwrap().expect("injected");
    assert_ne!(fs::read(e.save_path()).unwrap(), original);
    let rep = c.panic_restore().unwrap();
    assert!(rep.restored_tag.starts_with("safety/pre-mutation-"));
    assert_eq!(fs::read(e.save_path()).unwrap(), original, "bit-for-bit restore");
    assert_eq!(c.row("Gold").unwrap().current, Some(ScalarValue::Int(1200)));
    // The PanicRestore hotkey path works too (and reports errors inline).
    c.queue_action(HotkeyAction::PanicRestore);
    c.tick(Instant::now());
}

#[test]
fn toggle_overlay_requests_leave_daemon_requests_pending() {
    let e = env();
    let store = e.engine.store();
    store.push_request("snapshot", &serde_json::json!({ "game": e.game })).unwrap();
    bus::request_toggle_overlay(store).unwrap();
    store.push_request("panic-restore", &serde_json::json!({})).unwrap();
    let mut c = e.controller();
    assert!(c.is_visible());
    let cmds = c.poll_requests().unwrap();
    assert_eq!(cmds, vec![OverlayCommand::SetVisible(false)]);
    assert!(!c.is_visible());
    // The daemon still finds its requests, in order, with their payloads.
    let left = store.take_requests().unwrap();
    let actions: Vec<&str> = left.iter().map(|(_, a, _)| a.as_str()).collect();
    assert_eq!(actions, vec!["snapshot", "panic-restore"]);
    assert_eq!(left[0].2["game"].as_i64(), Some(e.game));
    // Nothing left for the overlay.
    assert!(c.poll_requests().unwrap().is_empty());
    bus::request_toggle_overlay(store).unwrap();
    assert_eq!(c.poll_requests().unwrap(), vec![OverlayCommand::SetVisible(true)]);
}

#[test]
fn hotkeys_unavailable_degrade_to_an_amber_notice() {
    let e = env();
    let mut c = e.controller();
    let cfg = HotkeyConfig {
        quick_snapshot: "ctrl+shift+NotAKey".into(),
        ..HotkeyConfig::default()
    };
    c.start_hotkeys_with(&cfg);
    assert!(c.hotkey_status().is_warning());
    assert!(c.hotkey_status().describe().contains("unavailable"));

    // Hotkeys disabled in config.toml, no daemon: clear statuses, no crash.
    let mut config = SaveLensConfig::default();
    config.hotkeys.enabled = false;
    let engine = Engine::new(e.engine.store().clone(), EnvelopePipeline::builtin(), config);
    let opts = OverlayOptions {
        hotkeys: true,
        embedded_daemon: false,
        ..quiet_opts()
    };
    let mut c = OverlayController::new(engine, &opts).unwrap();
    c.start_services();
    assert_eq!(c.hotkey_status(), &HotkeyStatus::DisabledInConfig);
    assert_eq!(c.daemon_mode(), &DaemonMode::Absent);
    assert!(c.notices().any(|n| n.level == NoticeLevel::Warn && n.text.contains("no daemon")));
}

#[test]
fn empty_store_and_unknown_game_degrade_gracefully() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = Engine::new(
        Store::open(tmp.path().join("data")).unwrap(),
        EnvelopePipeline::builtin(),
        SaveLensConfig::default(),
    );
    let mut c = OverlayController::new(engine, &quiet_opts()).unwrap();
    assert!(c.games().is_empty() && c.rows().is_empty() && c.selected_game().is_none());
    assert!(c.apply_now().is_err());
    assert!(c.quick_snapshot().is_err());
    assert!(c.presets().is_empty());
    c.queue_action(HotkeyAction::QuickSnapshot);
    c.tick(Instant::now());
    assert!(c.notices().any(|n| n.level == NoticeLevel::Error));

    let e = env();
    let opts = OverlayOptions {
        game: Some(9999),
        ..quiet_opts()
    };
    let c = OverlayController::new(e.engine.clone(), &opts).unwrap();
    assert_eq!(c.selected_game().map(|g| g.id), Some(e.game), "falls back to a tracked game");
    assert!(c.notices().any(|n| n.text.contains("#9999")));
}

fn raw_input(size: egui::Vec2) -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
        ..Default::default()
    }
}

#[test]
fn egui_frames_render_at_normal_and_tiny_sizes() {
    let e = env();
    let mut app = OverlayApp::new(e.engine.clone(), quiet_opts()).unwrap();
    app.controller_mut().set_frozen_value("Gold", "5").unwrap();
    // Some history for the sparkline.
    for (i, gold) in [1300, 1100, 1500].into_iter().enumerate() {
        fs::write(e.save_path(), save_json(gold, 87.5, 7)).unwrap();
        set_mtime(&e.save_path(), SystemTime::now() + Duration::from_secs(10 * (i as u64 + 1)));
        app.controller_mut().refresh(false).unwrap();
    }
    app.controller_mut().notify(NoticeLevel::Warn, "test notice");
    let ctx = egui::Context::default();
    for size in [
        egui::vec2(360.0, 520.0),
        egui::vec2(150.0, 90.0),
        egui::vec2(10.0, 10.0),
        egui::vec2(1.0, 1.0),
    ] {
        for _ in 0..3 {
            let out = ctx.run(raw_input(size), |ctx| app.ui(ctx));
            assert!(!out.shapes.is_empty() || size.x < 2.0);
        }
    }

    // Toggle-overlay hotkey: the frame emits a viewport visibility command.
    app.controller().queue_action(HotkeyAction::ToggleOverlay);
    let out = ctx.run(raw_input(egui::vec2(360.0, 520.0)), |ctx| app.ui(ctx));
    let cmds = &out.viewport_output[&egui::ViewportId::ROOT].commands;
    assert!(cmds.contains(&egui::ViewportCommand::Visible(false)), "{cmds:?}");
    assert!(!app.controller().is_visible());

    // Empty store: empty state renders too.
    let tmp = tempfile::tempdir().unwrap();
    let engine = Engine::new(
        Store::open(tmp.path().join("data")).unwrap(),
        EnvelopePipeline::builtin(),
        SaveLensConfig::default(),
    );
    let mut empty = OverlayApp::new(engine, quiet_opts()).unwrap();
    for size in [egui::vec2(360.0, 520.0), egui::vec2(40.0, 30.0)] {
        let _ = ctx.run(raw_input(size), |ctx| empty.ui(ctx));
    }
}

#[test]
fn viewport_is_transparent_always_on_top_and_borderless() {
    let opts = OverlayOptions::default();
    let vp = viewport(&opts);
    assert_eq!(vp.transparent, Some(true));
    assert_eq!(vp.decorations, Some(false));
    assert_eq!(vp.window_level, Some(egui::WindowLevel::AlwaysOnTop));
    assert_eq!(vp.inner_size, Some(egui::vec2(360.0, 520.0)));
    assert!(vp.title.as_deref().is_some_and(|t| t.contains("SaveLens")));
    assert!((opts.clamped_opacity() - 0.88).abs() < f32::EPSILON);
    let odd = OverlayOptions {
        opacity: f32::NAN,
        game: Some(3),
        ..OverlayOptions::default()
    };
    assert_eq!(odd.clamped_opacity(), 1.0);
    assert!(viewport(&odd).title.unwrap().contains("#3"));
    assert_eq!(OverlayOptions { opacity: 0.0, ..odd }.clamped_opacity(), 0.2);
}

#[test]
fn embedded_daemon_freezes_on_save_and_reports_latency() {
    let mut config = SaveLensConfig {
        debounce_ms: 150,
        close_write_quiet_ms: 60,
        stability_ms: 30,
        ..SaveLensConfig::default()
    };
    config.hotkeys.enabled = false;
    let e = env_with(config);
    let opts = OverlayOptions {
        embedded_daemon: true,
        hotkeys: true,
        ..quiet_opts()
    };
    let mut c = OverlayController::new(e.engine.clone(), &opts).unwrap();
    c.start_services();
    assert_eq!(c.daemon_mode(), &DaemonMode::Embedded);
    assert_eq!(c.hotkey_status(), &HotkeyStatus::DisabledInConfig);
    c.set_frozen_value("Gold", "31337").unwrap();
    c.set_freeze_enabled("Gold", true).unwrap();
    c.set_trainer_enabled(true).unwrap();
    // Give the watcher time to register the save directory.
    let ready = Instant::now() + Duration::from_secs(10);
    while e.engine.store().daemon_status(10_000).unwrap().is_none() && Instant::now() < ready {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(200));

    // The game autosaves after spending gold.
    fs::write(e.save_path(), save_json(3, 87.5, 7)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        c.tick(Instant::now());
        if c.last_latency().is_some_and(|l| l.source == LatencySource::Daemon) {
            break;
        }
        assert!(Instant::now() < deadline, "daemon did not inject; notices: {:?}", c.notices().collect::<Vec<_>>());
        std::thread::sleep(Duration::from_millis(25));
    }
    let l = c.last_latency().unwrap();
    assert_eq!(l.applied, vec!["Gold".to_string()]);
    assert_eq!(e.gold_on_disk(), Some(ScalarValue::Int(31_337)));
    c.tick(Instant::now());
    assert_eq!(c.row("Gold").unwrap().current, Some(ScalarValue::Int(31_337)));
    let log = e.engine.log(e.game, None, 50).unwrap();
    assert!(log.iter().any(|x| x.kind == CommitKind::Auto));
    assert!(log.iter().any(|x| x.kind == CommitKind::Trainer));
}
