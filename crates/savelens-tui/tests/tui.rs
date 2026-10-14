//! End-to-end tests of the TUI against a real engine, a real store and a
//! real save directory: rendering with `TestBackend` and key-driven flows.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use savelens_cas::{GameId, Store};
use savelens_codecs::EnvelopePipeline;
use savelens_core::{Location, NodePath};
use savelens_discovery::DiscoveryConfig;
use savelens_tui::{App, Tab};
use savelens_watcher::{Engine, SaveLensConfig};

fn save_json(gold: i64, mana: i64, items: &[&str]) -> String {
    let items: Vec<String> = items.iter().map(|i| format!("\"{i}\"")).collect();
    format!(
        "{{\n  \"Player\": {{\n    \"Name\": \"Aria\",\n    \"Gold\": {gold},\n    \"Mana\": {mana},\n    \"Level\": 5,\n    \"Inventory\": [{}]\n  }},\n  \"Quests\": {{ \"Main_02\": \"Active\" }}\n}}\n",
        items.join(", ")
    )
}

struct Env {
    _tmp: tempfile::TempDir,
    saves: PathBuf,
    engine: Engine,
    game: GameId,
    app: App,
}

impl Env {
    fn slot(&self) -> PathBuf {
        self.saves.join("slot1.json")
    }

    fn write_save(&self, content: &str) {
        fs::write(self.slot(), content).unwrap();
    }

    fn disk(&self) -> String {
        fs::read_to_string(self.slot()).unwrap()
    }

    fn key(&mut self, code: KeyCode) {
        self.app.handle_key(KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
    }

    fn ch(&mut self, c: char) {
        self.key(KeyCode::Char(c));
    }

    fn ctrl(&mut self, c: char) {
        self.app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)).unwrap();
    }

    fn typed(&mut self, s: &str) {
        for c in s.chars() {
            self.ch(c);
        }
    }

    /// Opens a prompt with `opener`, replaces its text and submits it.
    fn prompt(&mut self, opener: char, text: &str) {
        self.ch(opener);
        self.ctrl('u');
        self.typed(text);
        self.key(KeyCode::Enter);
    }

    fn idle(&mut self) {
        assert!(self.app.wait_idle(Duration::from_secs(30)), "background jobs did not finish");
    }

    fn render(&mut self, w: u16, h: u16) -> String {
        render(&mut self.app, w, h)
    }

    fn last_message(&self) -> String {
        self.app.messages().last().cloned().unwrap_or_default()
    }
}

fn render(app: &mut App, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| app.draw(f)).unwrap();
    let buf = term.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let saves = tmp.path().join("Saves");
    fs::create_dir_all(&saves).unwrap();
    fs::write(saves.join("slot1.json"), save_json(1200, 1200, &["Sword", "Shield"])).unwrap();
    let store = Store::open(tmp.path().join("data")).unwrap();
    let engine = Engine::new(store, EnvelopePipeline::builtin(), SaveLensConfig::default());
    let (game, import) = engine.track("Test RPG", Some(4242), &saves, &["*.json".to_string()], "native").unwrap();
    assert!(import.is_some());
    let mut app = App::new(engine.clone()).unwrap();
    // Never touch the developer's real Steam installation.
    app.set_discovery_config(DiscoveryConfig::for_home(tmp.path().join("empty-home")));
    Env { saves: saves.canonicalize().unwrap(), engine, game: game.id, app, _tmp: tmp }
}

#[test]
fn renders_every_tab_with_key_texts() {
    let mut e = env();
    for (w, h) in [(120, 40), (80, 24)] {
        e.ch('1');
        assert_eq!(e.app.tab(), Tab::Games);
        let s = e.render(w, h);
        assert!(s.contains("Test RPG"), "{s}");
        assert!(s.contains("Tracked games"), "{s}");
        assert!(s.contains("Watcher"), "{s}");

        e.ch('2');
        assert_eq!(e.app.tab(), Tab::Timeline);
        let s = e.render(w, h);
        assert!(s.contains("[import]"), "{s}");
        assert!(s.contains("main"), "{s}");

        e.ch('3');
        assert_eq!(e.app.tab(), Tab::Scanner);
        let s = e.render(w, h);
        assert!(s.contains("Save-State Scanner"), "{s}");
        assert!(s.contains("slot1.json"), "{s}");

        e.ch('4');
        assert_eq!(e.app.tab(), Tab::Ast);
        e.idle();
        let s = e.render(w, h);
        assert!(s.contains("Lossless Verified"), "{s}");
        assert!(s.contains("Gold"), "{s}");
        assert!(s.contains("1200"), "{s}");
    }
    // Tab / Shift+Tab cycle.
    e.key(KeyCode::Tab);
    assert_eq!(e.app.tab(), Tab::Games);
    e.key(KeyCode::BackTab);
    assert_eq!(e.app.tab(), Tab::Ast);
    // Help popup.
    e.ch('?');
    let s = e.render(120, 40);
    assert!(s.contains("Help"), "{s}");
    assert!(s.contains("PANIC RESTORE"), "{s}");
    e.key(KeyCode::Esc);
    let s = e.render(120, 40);
    assert!(!s.contains("Help — SaveLens keys"));
}

#[test]
fn tiny_terminals_never_panic() {
    let mut e = env();
    let sizes = [(20, 5), (1, 1), (0, 0), (24, 6), (30, 8), (40, 10), (200, 3)];
    for tab in ['1', '2', '3', '4'] {
        e.ch(tab);
        e.idle();
        for (w, h) in sizes {
            e.render(w, h);
        }
    }
    // With modals open.
    e.ch('?');
    for (w, h) in sizes {
        e.render(w, h);
    }
    e.key(KeyCode::Esc);
    e.ch(':');
    for (w, h) in sizes {
        e.render(w, h);
    }
    e.key(KeyCode::Esc);
    e.ch('3');
    e.ch('o');
    for (w, h) in sizes {
        e.render(w, h);
    }
    e.key(KeyCode::Esc);
}

#[test]
fn snapshot_shows_semantic_summary_and_diff() {
    let mut e = env();
    e.write_save(&save_json(1500, 1200, &["Sword", "Shield"]));
    e.ch('1');
    e.ch('s');
    assert!(e.last_message().contains("+300 Gold"), "{:?}", e.app.messages());
    e.ch('2');
    let s = e.render(120, 40);
    assert!(s.contains("+300 Gold"), "{s}");
    assert!(s.contains("[manual]"), "{s}");
    assert!(s.contains("Player.Gold: 1200 -> 1500 (+300)"), "{s}");

    // Diff of the import commit against WORKING.
    e.ch('j');
    e.ch('d');
    let s = e.render(120, 40);
    assert!(s.contains("Diff #"), "{s}");
    assert!(s.contains("Player.Gold: 1200 -> 1500 (+300)"), "{s}");
    e.key(KeyCode::Esc);

    // Snapshot again without changes: nothing to commit, no crash.
    e.ch('1');
    e.ch('s');
    assert!(e.last_message().contains("nothing changed"), "{:?}", e.app.messages());
}

#[test]
fn scanner_narrows_pins_edits_and_freezes() {
    let mut e = env();
    e.ch('3');
    // A next-scan filter on a fresh session is rejected with a message, not a crash.
    e.prompt('i', "inc");
    assert!(e.last_message().contains("previous scan"), "{:?}", e.app.messages());

    e.prompt('i', "1200");
    e.idle();
    let s = e.render(120, 40);
    assert!(s.contains("Player.Gold"), "{s}");
    assert!(s.contains("Player.Mana"), "{s}");
    assert!(e.last_message().contains("2 candidate(s)"), "{:?}", e.app.messages());

    // The game saves: gold goes 1200 -> 1500, mana stays.
    e.write_save(&save_json(1500, 1200, &["Sword", "Shield"]));
    e.prompt('i', "1200 -> 1500");
    e.idle();
    assert!(e.last_message().contains("1 candidate(s)"), "{:?}", e.app.messages());
    let s = e.render(120, 40);
    assert!(s.contains("Player.Gold"), "{s}");
    assert!(!s.contains("Player.Mana"), "{s}");
    assert!(s.contains("1500"), "{s}");

    // Pin it as Oro.
    e.prompt('a', "Oro");
    let cheats = e.engine.store().cheats(e.game).unwrap();
    assert_eq!(cheats.len(), 1);
    assert_eq!(cheats[0].alias, "Oro");
    assert_eq!(cheats[0].location, Location::Ast { path: NodePath::parse("Player.Gold").unwrap() });
    let s = e.render(120, 40);
    assert!(s.contains("Oro"), "{s}");

    // Edit through the scanner: written on disk with a safety backup.
    e.prompt('e', "2500");
    assert!(e.disk().contains("\"Gold\": 2500,"), "{}", e.disk());
    assert!(e.last_message().contains("safety/pre-mutation-"), "{:?}", e.app.messages());
    let s = e.render(120, 40);
    assert!(s.contains("2500"), "{s}");

    // Cheat table: edit with Enter, freeze with F.
    e.ch('v');
    e.ch('e');
    e.ctrl('u');
    e.typed("3000");
    e.key(KeyCode::Enter);
    assert!(e.disk().contains("\"Gold\": 3000,"), "{}", e.disk());
    e.ch('F');
    let cheats = e.engine.store().cheats(e.game).unwrap();
    assert!(cheats[0].freeze_enabled);
    assert_eq!(cheats[0].frozen_value.map(|v| v.to_string()), Some("3000".to_string()));
    let s = e.render(120, 40);
    assert!(s.contains("❄"), "{s}");

    // Delete asks for confirmation.
    e.ch('x');
    let s = e.render(120, 40);
    assert!(s.contains("Delete Oro?"), "{s}");
    e.ch('y');
    assert!(e.engine.store().cheats(e.game).unwrap().is_empty());

    // Scan options toggle resets the session.
    e.ch('v');
    e.ch('o');
    e.key(KeyCode::Char(' '));
    e.key(KeyCode::Esc);
    assert!(e.last_message().contains("scan reset"), "{:?}", e.app.messages());
}

#[test]
fn ast_cherry_pick_restores_inventory_from_head_1() {
    let mut e = env();
    // HEAD~1 = import (Sword, Shield); HEAD = snapshot with a Potion and more gold.
    e.write_save(&save_json(1500, 1200, &["Sword", "Shield", "Potion"]));
    e.ch('1');
    e.ch('s');
    e.ch('4');
    e.idle();
    let s = e.render(120, 40);
    assert!(s.contains("Inventory  [3 items]"), "{s}");

    // Choose HEAD~1 as revision: WORKING, HEAD, HEAD~1.
    e.ch('r');
    e.ch('j');
    e.ch('j');
    e.key(KeyCode::Enter);
    e.idle();
    let s = e.render(120, 40);
    assert!(s.contains("@ HEAD~1"), "{s}");
    assert!(s.contains("Inventory  [2 items]"), "{s}");

    // Find and mark Player.Inventory.
    e.prompt('/', "Player.Inventory");
    let s = e.render(120, 40);
    assert!(s.contains("Path      Player.Inventory\n") || s.contains("Path      Player.Inventory "), "{s}");
    e.ch('m');
    e.ch('C');
    let s = e.render(120, 40);
    assert!(s.contains("Surgical cherry-pick"), "{s}");
    assert!(s.contains("Semantic preview"), "{s}");
    assert!(s.contains("Potion"), "preview mentions the removed item: {s}");
    e.ch('y');

    let disk = e.disk();
    assert!(disk.contains("\"Inventory\": [\"Sword\", \"Shield\"]"), "{disk}");
    assert!(disk.contains("\"Gold\": 1500,"), "everything else is kept: {disk}");
    let head = e.engine.log(e.game, None, 1).unwrap();
    assert_eq!(head[0].kind, savelens_cas::CommitKind::CherryPick);
}

#[test]
fn fuzzy_search_finds_gold_and_edits_leaf() {
    let mut e = env();
    e.ch('4');
    e.idle();
    e.prompt('/', "gold");
    let s = e.render(120, 40);
    assert!(s.contains("Path      Player.Gold"), "{s}");
    assert!(s.contains("`gold`"), "{s}");
    // n cycles without crashing.
    e.ch('n');
    e.ch('N');
    let s = e.render(120, 40);
    assert!(s.contains("Path      Player.Gold"), "{s}");

    // Edit the leaf on disk.
    e.prompt('e', "4242");
    assert!(e.disk().contains("\"Gold\": 4242,"), "{}", e.disk());
    e.idle();
    let s = e.render(120, 40);
    assert!(s.contains("4242"), "{s}");

    // Collapse / expand with h / l.
    e.key(KeyCode::Char('g'));
    e.ch('h');
    let s = e.render(120, 40);
    assert!(!s.contains("Inventory"), "{s}");
    e.ch('l');
    let s = e.render(120, 40);
    assert!(s.contains("Player"), "{s}");

    // Cherry-pick without marks or from WORKING is refused with a message.
    e.ch('C');
    assert!(e.last_message().contains("mark sub-trees"), "{:?}", e.app.messages());
}

#[test]
fn panic_restore_reverts_last_mutation_bit_for_bit() {
    let mut e = env();
    let original = fs::read(e.slot()).unwrap();
    // Nothing mutated yet.
    e.ch('P');
    assert!(e.last_message().contains("Nothing to restore"), "{:?}", e.app.messages());

    e.ch('4');
    e.idle();
    e.prompt('/', "gold");
    e.prompt('e', "999999");
    assert!(e.disk().contains("999999"));

    e.ch('P');
    let s = e.render(120, 40);
    assert!(s.contains("PANIC RESTORE"), "{s}");
    assert!(s.contains("safety/pre-mutation-"), "{s}");
    // Cancel first: nothing happens.
    e.ch('n');
    assert!(e.disk().contains("999999"));
    e.ch('P');
    e.ch('y');
    assert_eq!(fs::read(e.slot()).unwrap(), original, "bytes restored bit for bit");
    assert!(e.last_message().contains("Panic restore"), "{:?}", e.app.messages());
}

#[test]
fn command_line_and_timeline_actions() {
    let mut e = env();
    e.prompt(':', "branch exp");
    let branches = e.engine.store().branches(e.game).unwrap();
    assert!(branches.iter().any(|b| b.name == "exp"), "{branches:?}");

    e.prompt(':', "tag v1");
    assert!(e.engine.store().tags(e.game).unwrap().iter().any(|t| t.name == "v1"));

    e.write_save(&save_json(1300, 1200, &["Sword", "Shield"]));
    e.prompt(':', "snapshot \"before boss\"");
    let head = e.engine.log(e.game, None, 1).unwrap();
    assert_eq!(head[0].message, "before boss");
    assert!(head[0].summary.contains("+100 Gold"), "{}", head[0].summary);

    e.prompt(':', "gc");
    assert!(e.last_message().starts_with("gc:"), "{:?}", e.app.messages());
    e.prompt(':', "frobnicate");
    assert!(e.last_message().contains("unknown command"), "{:?}", e.app.messages());
    e.prompt(':', "branch");
    assert!(e.last_message().contains("usage"), "{:?}", e.app.messages());

    // Timeline: tag + new branch from the highlighted commit, checkout of the import.
    e.ch('2');
    e.prompt('T', "boss");
    assert!(e.engine.store().tags(e.game).unwrap().iter().any(|t| t.name == "boss"));
    let s = e.render(120, 40);
    assert!(s.contains("⚑ boss"), "{s}");
    e.ch('j');
    e.prompt('b', "alt");
    assert!(e.engine.store().branches(e.game).unwrap().iter().any(|b| b.name == "alt"));
    e.ch('c');
    let s = e.render(120, 40);
    assert!(s.contains("Checkout #"), "{s}");
    e.ch('y');
    assert!(e.disk().contains("\"Gold\": 1200,"), "{}", e.disk());

    // Switch branch with the picker (exp is listed before main? pick by name).
    e.ch('B');
    let s = e.render(120, 40);
    assert!(s.contains("Switch branch"), "{s}");
    let branches = e.engine.store().branches(e.game).unwrap();
    let idx = branches.iter().position(|b| b.name == "alt").unwrap();
    let cur = branches.iter().position(|b| b.name == "main").unwrap();
    let (dir, n) = if idx > cur { ('j', idx - cur) } else { ('k', cur - idx) };
    for _ in 0..n {
        e.ch(dir);
    }
    e.key(KeyCode::Enter);
    let game = e.engine.game(e.game).unwrap();
    assert_eq!(game.current_branch, "alt");
    let s = e.render(120, 40);
    assert!(s.contains("⎇ alt"), "{s}");

    // :checkout by revision.
    e.prompt(':', "checkout v1");
    assert!(e.disk().contains("\"Gold\": 1200,"));
}

#[test]
fn quitting_and_watcher_daemon() {
    let mut e = env();
    // Without the daemon q quits immediately.
    assert!(e.app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)).unwrap());

    let mut e = env();
    e.ch('w');
    assert!(e.app.daemon_running());
    let start = Instant::now();
    while !e.app.messages().iter().any(|m| m.starts_with("Watcher started")) {
        assert!(start.elapsed() < Duration::from_secs(20), "{:?}", e.app.messages());
        e.app.tick();
        std::thread::sleep(Duration::from_millis(20));
    }
    let s = e.render(120, 40);
    assert!(s.contains("running (embedded"), "{s}");
    if std::env::var_os("DISPLAY").is_none_or(|d| d.is_empty()) {
        assert!(e.app.messages().iter().any(|m| m.starts_with("Global hotkeys unavailable")), "{:?}", e.app.messages());
    }
    // q asks for confirmation while the daemon runs; n keeps it running.
    assert!(!e.app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)).unwrap());
    let s = e.render(120, 40);
    assert!(s.contains("Quit SaveLens?"), "{s}");
    e.ch('n');
    assert!(e.app.daemon_running());
    // w stops it.
    e.ch('w');
    assert!(!e.app.daemon_running());
    e.ch('w');
    assert!(e.app.daemon_running());
    e.ch('q');
    assert!(e.app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)).unwrap());
    assert!(!e.app.daemon_running());
}

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn discovery_runs_in_background_and_tracks_with_enter() {
    let mut e = env();
    let home = e._tmp.path().canonicalize().unwrap().join("home");
    let root = home.join(".local/share/Steam");
    write(
        &root.join("steamapps/libraryfolders.vdf"),
        &format!(
            "\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t\t\"apps\"\n\t\t{{\n\t\t\t\"1000001\"\t\t\"100\"\n\t\t}}\n\t}}\n}}\n",
            root.display()
        ),
    );
    write(
        &root.join("steamapps/appmanifest_1000001.acf"),
        "\"AppState\"\n{\n\t\"appid\"\t\t\"1000001\"\n\t\"name\"\t\t\"Native Unity Game\"\n\t\"StateFlags\"\t\t\"4\"\n\t\"installdir\"\t\t\"NativeUnity\"\n}\n",
    );
    write(&root.join("steamapps/common/NativeUnity/Game_Data/app.info"), "Company\nProduct");
    write(&root.join("steamapps/common/NativeUnity/Game.x86_64"), "elf");
    write(&home.join(".config/unity3d/Company/Product/save.dat"), "save");
    e.app.set_discovery_config(DiscoveryConfig::for_home(home));

    e.ch('1');
    e.ch('d');
    assert!(e.app.is_busy() || e.app.messages().iter().any(|m| m.starts_with("Discovery")));
    e.idle();
    let s = e.render(120, 40);
    assert!(s.contains("Native Unity Game"), "{s}\n{:?}", e.app.messages());

    // Cursor: tracked game (0) -> discovered game (1).
    e.ch('j');
    e.key(KeyCode::Enter);
    let games = e.engine.store().games().unwrap();
    assert_eq!(games.len(), 2, "{:?}", e.app.messages());
    let g = games.iter().find(|g| g.name == "Native Unity Game").unwrap();
    assert_eq!(e.app.current_game(), Some(g.id));
    let s = e.render(120, 40);
    assert!(s.contains("Every discovered game is already tracked"), "{s}");

    // Trainer and watch toggles on the highlighted game.
    e.ch('t');
    assert!(e.engine.game(g.id).unwrap().trainer_enabled);
    e.ch('W');
    assert!(!e.engine.game(g.id).unwrap().watch_enabled);
}
