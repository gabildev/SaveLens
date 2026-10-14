//! End-to-end tests of the engine and the daemon on real temporary
//! directories: tracking, semantic history, edits, cherry-pick, checkout,
//! trainer, Safe Edit Gate, game-directory isolation and panic restore.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use savelens_cas::{CommitKind, PanicScope, Store};
use savelens_codecs::document::{Capabilities, Confidence, DetectContext, Document, StructuralCodec};
use savelens_codecs::layers::crypto::keys::KeyDatabase;
use savelens_codecs::{CodecRegistry, CodecResult, EnvelopePipeline, PipelineOptions};
use savelens_core::{CherryPickOptions, Endianness, Location, Node, NodePath, ScalarValue, ScanType};
use savelens_watcher::*;

fn listing(dir: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for e in fs::read_dir(dir).unwrap().flatten() {
        out.insert(e.file_name().to_string_lossy().to_string());
    }
    out
}

fn save_json(gold: i64, items: &[&str], quest: &str) -> Vec<u8> {
    let items: Vec<String> = items.iter().map(|i| format!("\"{i}\"")).collect();
    format!(
        "{{\n  \"Player\": {{\n    \"Gold\": {gold},\n    \"HP\": 87.5,\n    \"Inventory\": [{}]\n  }},\n  \"Quests\": {{ \"Main_02\": \"{quest}\" }}\n}}\n",
        items.join(", ")
    )
    .into_bytes()
}

struct Env {
    _tmp: tempfile::TempDir,
    engine: Engine,
    saves: PathBuf,
}

fn env_with(config: SaveLensConfig, pipeline: EnvelopePipeline) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let saves = tmp.path().join("Saves");
    fs::create_dir_all(&saves).unwrap();
    fs::write(saves.join("slot1.json"), save_json(1200, &["Sword", "Shield"], "Active")).unwrap();
    fs::write(saves.join("options.ini"), b"[video]\nvsync=1\n").unwrap();
    let store = Store::open(tmp.path().join("data")).unwrap();
    let engine = Engine::new(store, pipeline, config);
    Env { saves: saves.canonicalize().unwrap(), engine, _tmp: tmp }
}

fn env() -> Env {
    env_with(SaveLensConfig::default(), EnvelopePipeline::builtin())
}

fn gold_path() -> NodePath {
    NodePath::parse("Player.Gold").unwrap()
}

#[test]
fn track_snapshot_edit_and_panic_restore() {
    let e = env();
    let (game, import) = e.engine.track("Test RPG", Some(42), &e.saves, &["*.json".to_string()], "native").unwrap();
    let import = import.expect("import commit");
    assert_eq!(import.kind, CommitKind::Import);
    assert_eq!(e.engine.tracked_files(&game).unwrap().len(), 1, "only *.json is tracked");
    let before = listing(&e.saves);
    let original = fs::read(e.saves.join("slot1.json")).unwrap();

    // Game saves: gold 1200 -> 1500, quest completed.
    fs::write(e.saves.join("slot1.json"), save_json(1500, &["Sword", "Shield"], "Completed")).unwrap();
    let c = e.engine.snapshot(game.id, "manual", CommitKind::Manual).unwrap().unwrap();
    assert!(c.summary.contains("+300 Gold"), "{}", c.summary);
    let logs = e.engine.store().changelogs(c.id).unwrap();
    assert!(logs[0].lines.contains(&"Player.Gold: 1200 -> 1500 (+300)".to_string()), "{:?}", logs[0].lines);
    assert!(logs[0].lines.iter().any(|l| l.contains("\"Active\" -> \"Completed\"")));

    // Alias shows up in later changelogs.
    e.engine.pin(game.id, "slot1.json", "Oro", &Location::Ast { path: gold_path() }).unwrap();

    // Edit gold on disk.
    let rep = e
        .engine
        .edit_value(game.id, "slot1.json", &EditTarget::Path { path: gold_path(), value: "99999".into() }, false)
        .unwrap();
    assert!(!rep.no_op);
    assert!(rep.safety_tag.as_deref().unwrap().starts_with("safety/pre-mutation-"));
    let now = fs::read_to_string(e.saves.join("slot1.json")).unwrap();
    assert!(now.contains("\"Gold\": 99999,"), "formatting preserved: {now}");
    let edit_commit = e.engine.store().commit(rep.commit.unwrap()).unwrap();
    assert!(edit_commit.summary.contains("+98499 Oro"), "{}", edit_commit.summary);
    assert_eq!(listing(&e.saves), before, "no extra files in the game directory");

    // Same edit again is a no-op.
    let again = e
        .engine
        .edit_value(game.id, "slot1.json", &EditTarget::Path { path: gold_path(), value: "99999".into() }, false)
        .unwrap();
    assert!(again.no_op);

    // Cheat table reads the live value.
    let cv = e.engine.cheat_values(game.id).unwrap();
    assert_eq!(cv[0].current, Some(ScalarValue::Int(99999)));

    // Panic restore returns to the state before the edit (game's 1500 save).
    let pr = e.engine.panic_restore(&PanicScope::Game(game.id)).unwrap();
    assert_eq!(pr.restored_tag, rep.safety_tag.unwrap());
    assert_eq!(fs::read(e.saves.join("slot1.json")).unwrap(), save_json(1500, &["Sword", "Shield"], "Completed"));
    assert_ne!(fs::read(e.saves.join("slot1.json")).unwrap(), original);
    let head = e.engine.log(game.id, None, 1).unwrap();
    assert_eq!(head[0].kind, CommitKind::Restore);

    // Diff between revisions.
    let d = e.engine.diff(game.id, &import.id.to_string(), WORKING, None).unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].status, FileStatus::Modified);
    assert!(d[0].lines.iter().any(|l| l.starts_with("Oro: 1200 -> 1500")), "{:?}", d[0].lines);
}

#[test]
fn surgical_cherry_pick_keeps_other_progress() {
    let e = env();
    let (game, _) = e.engine.track("RPG", None, &e.saves, &["*.json".to_string()], "native").unwrap();
    // Later save: lost the inventory but earned gold and finished the quest.
    fs::write(e.saves.join("slot1.json"), save_json(5000, &["Stick"], "Completed")).unwrap();
    e.engine.snapshot(game.id, "game saved", CommitKind::Auto).unwrap().unwrap();
    let rep = e
        .engine
        .cherry_pick(
            game.id,
            "HEAD~1",
            "slot1.json",
            &[NodePath::parse("Player.Inventory").unwrap()],
            &CherryPickOptions::default(),
        )
        .unwrap();
    assert!(!rep.no_op);
    let root = e.engine.root_at(game.id, WORKING, "slot1.json").unwrap();
    assert_eq!(root.get(&gold_path()), Some(&Node::I64(5000)));
    assert_eq!(
        root.get(&NodePath::parse("Player.Inventory").unwrap()),
        Some(&Node::Array(vec![Node::String("Sword".into()), Node::String("Shield".into())]))
    );
    assert_eq!(root.get(&NodePath::parse("Quests.Main_02").unwrap()), Some(&Node::String("Completed".into())));
    assert_eq!(e.engine.log(game.id, None, 1).unwrap()[0].kind, CommitKind::CherryPick);
}

#[test]
fn checkout_and_branches() {
    let e = env();
    let (game, import) = e.engine.track("RPG", None, &e.saves, &[], "native").unwrap();
    let import = import.unwrap();
    assert_eq!(e.engine.tracked_files(&game).unwrap().len(), 2, "all files when no pattern");
    fs::write(e.saves.join("slot1.json"), save_json(1, &[], "Failed")).unwrap();
    e.engine.snapshot(game.id, "bad run", CommitKind::Auto).unwrap().unwrap();
    // Branch from the import, switch to it: disk gets the import state.
    e.engine.create_branch(game.id, "experiment", Some(&import.id.to_string())).unwrap();
    let sw = e.engine.switch_branch(game.id, "experiment").unwrap();
    assert_eq!(sw.written, vec!["slot1.json".to_string()]);
    assert_eq!(fs::read(e.saves.join("slot1.json")).unwrap(), save_json(1200, &["Sword", "Shield"], "Active"));
    assert_eq!(e.engine.game(game.id).unwrap().current_branch, "experiment");
    // Back to main.
    e.engine.switch_branch(game.id, "main").unwrap();
    assert_eq!(fs::read(e.saves.join("slot1.json")).unwrap(), save_json(1, &[], "Failed"));
    // Quick rollback (HEAD~1).
    let rb = e.engine.quick_rollback(game.id).unwrap();
    assert_eq!(rb.written.len(), 1);
    assert_eq!(fs::read(e.saves.join("slot1.json")).unwrap(), save_json(1200, &["Sword", "Shield"], "Active"));
    assert!(rb.commit.is_some());
    assert!(e.engine.switch_branch(game.id, "nope").is_err());
}

#[test]
fn trainer_injects_frozen_values_fast() {
    let e = env();
    let (game, _) = e.engine.track("RPG", None, &e.saves, &["*.json".to_string()], "native").unwrap();
    e.engine.pin(game.id, "slot1.json", "Gold", &Location::Ast { path: gold_path() }).unwrap();
    e.engine.freeze(game.id, "Gold", Some(ScalarValue::Int(777_777)), true).unwrap();
    // The game spends gold and saves.
    fs::write(e.saves.join("slot1.json"), save_json(10, &["Sword"], "Active")).unwrap();
    let rep = e.engine.inject_trainer(game.id, "slot1.json").unwrap().expect("applied");
    assert_eq!(rep.applied, vec!["Gold".to_string()]);
    let root = e.engine.root_at(game.id, WORKING, "slot1.json").unwrap();
    assert_eq!(root.get(&gold_path()), Some(&Node::I64(777_777)));
    let limit = if cfg!(debug_assertions) { 1000.0 } else { 50.0 };
    assert!(rep.elapsed_ms < limit, "trainer took {} ms", rep.elapsed_ms);
    // Already frozen: nothing to do.
    assert!(e.engine.inject_trainer(game.id, "slot1.json").unwrap().is_none());
}

#[test]
fn raw_binary_cheat_with_checksum_is_resigned() {
    let e = env();
    // Unknown binary save with a CRC32 trailer.
    let mut body = vec![0u8; 256];
    for (i, b) in body.iter_mut().enumerate() {
        *b = (i * 31 % 251) as u8;
    }
    body[0x40..0x44].copy_from_slice(&1500u32.to_le_bytes());
    let mut file = body.clone();
    file.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
    fs::write(e.saves.join("game.dat"), &file).unwrap();
    let (game, _) = e.engine.track("Bin", None, &e.saves, &["*.dat".to_string()], "native").unwrap();
    let loc = Location::Raw { region: NodePath::root(), offset: 0x40, ty: ScanType::U32, endian: Endianness::Little };
    e.engine.pin(game.id, "game.dat", "Money", &loc).unwrap();
    e.engine.set_cheat(game.id, "Money", ScalarValue::Int(424_242)).unwrap();
    let out = fs::read(e.saves.join("game.dat")).unwrap();
    assert_eq!(&out[0x40..0x44], &424_242u32.to_le_bytes());
    let n = out.len();
    assert_eq!(&out[n - 4..], &crc32fast::hash(&out[..n - 4]).to_le_bytes(), "CRC32 re-signed");
}

/// A deliberately lossy codec: claims `.lossy` files and re-serialises them
/// with normalised whitespace, so the Safe Edit Gate must block it.
struct LossyCodec;

#[derive(Clone)]
struct LossyDoc(Vec<u8>);

impl Document for LossyDoc {
    fn codec_id(&self) -> &'static str {
        "lossy"
    }
    fn to_ast(&self) -> Node {
        Node::map_from([("text", Node::String(String::from_utf8_lossy(&self.0).trim().to_string()))])
    }
    fn apply_ast(&mut self, root: &Node) -> CodecResult<()> {
        if let Some(Node::String(s)) = root.get(&NodePath::parse("text").unwrap()) {
            self.0 = s.clone().into_bytes();
        }
        Ok(())
    }
    fn encode(&self) -> CodecResult<Vec<u8>> {
        Ok(String::from_utf8_lossy(&self.0).trim().as_bytes().to_vec())
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::VALUES_ONLY
    }
    fn clone_box(&self) -> Box<dyn Document> {
        Box::new(self.clone())
    }
}

impl StructuralCodec for LossyCodec {
    fn id(&self) -> &'static str {
        "lossy"
    }
    fn name(&self) -> &'static str {
        "test lossy codec"
    }
    fn detect(&self, _bytes: &[u8], ctx: &DetectContext) -> Confidence {
        if ctx.extension().as_deref() == Some("lossy") {
            99
        } else {
            0
        }
    }
    fn decode(&self, bytes: &[u8], _ctx: &DetectContext) -> CodecResult<Box<dyn Document>> {
        Ok(Box::new(LossyDoc(bytes.to_vec())))
    }
}

#[test]
fn safe_edit_gate_blocks_lossy_codecs() {
    let mut reg = CodecRegistry::with_defaults();
    reg.register(Arc::new(LossyCodec));
    let pipeline = EnvelopePipeline::new(reg, KeyDatabase::builtin(), PipelineOptions::default());
    let e = env_with(SaveLensConfig::default(), pipeline);
    fs::write(e.saves.join("a.lossy"), b"  hello 1234  \n").unwrap();
    let (game, _) = e.engine.track("L", None, &e.saves, &["*.lossy".to_string()], "native").unwrap();
    let opened = e.engine.open_version(game.id, WORKING, "a.lossy").unwrap();
    assert_eq!(opened.edit_mode(), savelens_core::EditMode::RawInPlace);
    let err = e
        .engine
        .edit_value(
            game.id,
            "a.lossy",
            &EditTarget::Path { path: NodePath::parse("text").unwrap(), value: "x".into() },
            false,
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::GateBlocked(_)), "{err}");
    assert_eq!(fs::read(e.saves.join("a.lossy")).unwrap(), b"  hello 1234  \n", "file untouched");
    // Raw in-place mode is allowed and keeps every other byte.
    let loc =
        Location::Raw { region: NodePath::root(), offset: 8, ty: ScanType::AsciiDecimal, endian: Endianness::Little };
    e.engine
        .edit_value(game.id, "a.lossy", &EditTarget::Location { location: loc, value: ScalarValue::Int(9876) }, true)
        .unwrap();
    assert_eq!(fs::read(e.saves.join("a.lossy")).unwrap(), b"  hello 9876  \n");
}

fn wait_for<F: Fn(&DaemonEvent) -> bool>(h: &DaemonHandle, f: F, timeout: Duration) -> Option<DaemonEvent> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match h.events().recv_timeout(Duration::from_millis(100)) {
            Ok(ev) if f(&ev) => return Some(ev),
            Ok(_) => {}
            Err(_) => {}
        }
    }
    None
}

#[test]
fn daemon_auto_commits_trains_and_serves_requests() {
    let cfg = SaveLensConfig {
        debounce_ms: 300,
        close_write_quiet_ms: 100,
        stability_ms: 30,
        lock_timeout_ms: 2000,
        ..SaveLensConfig::default()
    };
    let e = env_with(cfg, EnvelopePipeline::builtin());
    let (game, _) = e.engine.track("RPG", None, &e.saves, &["*.json".to_string()], "native").unwrap();
    let before = listing(&e.saves);
    let handle =
        Daemon::spawn(e.engine.clone(), DaemonOptions { hotkeys: false, games: None, tick: Duration::from_millis(20) })
            .unwrap();
    assert!(wait_for(&handle, |ev| matches!(ev, DaemonEvent::Watching { .. }), Duration::from_secs(5)).is_some());

    // The "game" saves.
    fs::write(e.saves.join("slot1.json"), save_json(2000, &["Sword", "Shield"], "Active")).unwrap();
    let ev = wait_for(&handle, |ev| matches!(ev, DaemonEvent::Committed { .. }), Duration::from_secs(10))
        .expect("auto commit");
    if let DaemonEvent::Committed { summary, .. } = ev {
        assert!(summary.contains("+800 Gold"), "{summary}");
    }

    // Enable the disk trainer and freeze gold.
    e.engine.pin(game.id, "slot1.json", "Gold", &Location::Ast { path: gold_path() }).unwrap();
    e.engine.freeze(game.id, "Gold", Some(ScalarValue::Int(123_456)), true).unwrap();
    e.engine.store().set_trainer_enabled(game.id, true).unwrap();
    fs::write(e.saves.join("slot1.json"), save_json(5, &["Sword", "Shield"], "Active")).unwrap();
    let ev = wait_for(&handle, |ev| matches!(ev, DaemonEvent::TrainerApplied { .. }), Duration::from_secs(10))
        .expect("trainer");
    if let DaemonEvent::TrainerApplied { applied, .. } = ev {
        assert_eq!(applied, vec!["Gold".to_string()]);
    }
    let root = e.engine.root_at(game.id, WORKING, "slot1.json").unwrap();
    assert_eq!(root.get(&gold_path()), Some(&Node::I64(123_456)));

    // Panic restore requested by another process (overlay / CLI) through the store.
    e.engine.store().push_request("panic-restore", &serde_json::json!({ "game": game.id })).unwrap();
    assert!(wait_for(&handle, |ev| matches!(ev, DaemonEvent::PanicRestored { .. }), Duration::from_secs(10)).is_some());
    let root = e.engine.root_at(game.id, WORKING, "slot1.json").unwrap();
    assert_eq!(root.get(&gold_path()), Some(&Node::I64(5)), "back to the game's own save");

    assert!(e.engine.store().daemon_status(10_000).unwrap().is_some());
    handle.stop().unwrap();
    assert!(e.engine.store().daemon_status(10_000).unwrap().is_none());
    assert_eq!(listing(&e.saves), before, "the daemon never dirties the game directory");
}

#[cfg(target_os = "linux")]
#[test]
fn daemon_waits_while_the_game_holds_the_file_open_for_writing() {
    use std::io::Write;
    let cfg = SaveLensConfig {
        debounce_ms: 200,
        close_write_quiet_ms: 50,
        stability_ms: 20,
        lock_timeout_ms: 60_000,
        ..SaveLensConfig::default()
    };
    let e = env_with(cfg, EnvelopePipeline::builtin());
    let (_game, _) = e.engine.track("RPG", None, &e.saves, &["*.json".to_string()], "native").unwrap();
    let handle =
        Daemon::spawn(e.engine.clone(), DaemonOptions { hotkeys: false, games: None, tick: Duration::from_millis(20) })
            .unwrap();
    assert!(wait_for(&handle, |ev| matches!(ev, DaemonEvent::Watching { .. }), Duration::from_secs(5)).is_some());
    // Simulate a game that keeps the save open while writing it in two steps.
    let path = e.saves.join("slot1.json");
    let mut f = fs::OpenOptions::new().write(true).truncate(true).open(&path).unwrap();
    f.write_all(b"{\"Player\": {\"Gold\": ").unwrap();
    f.flush().unwrap();
    // While open for writing nothing may be committed.
    assert!(wait_for(&handle, |ev| matches!(ev, DaemonEvent::Committed { .. }), Duration::from_millis(1500)).is_none());
    f.write_all(b"3}}").unwrap();
    drop(f);
    let ev = wait_for(&handle, |ev| matches!(ev, DaemonEvent::Committed { .. }), Duration::from_secs(10))
        .expect("commit after close");
    if let DaemonEvent::Committed { rel_path, .. } = ev {
        assert_eq!(rel_path, "slot1.json");
    }
    handle.stop().unwrap();
}
