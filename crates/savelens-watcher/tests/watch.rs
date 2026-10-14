//! "What changed?" end to end: a survival-RPG save is rewritten on disk the
//! way a game would, the engine asks about player stats only (never about
//! clocks, positions, seeds, world lists or bookkeeping counters), answers
//! pin values into the cheat table, ignores mute them, and nothing but the
//! game's own saves ever raises a question.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use savelens_cas::{CommitKind, GameId, QuestionStatus, Store};
use savelens_codecs::EnvelopePipeline;
use savelens_core::{Location, Node, NodePath, ScalarValue, VariableCategory};
use savelens_watcher::*;

#[derive(Clone, Copy)]
struct State {
    gold: i64,
    hp: i64,
    hunger: f64,
    ammo: i64,
    wood: i64,
    double_jump: bool,
    night: bool,
    t: f64,
    x: f64,
    saves: i64,
    enemy_hp: i64,
    steps: i64,
    mystery: i64,
}

impl State {
    fn new() -> State {
        State {
            gold: 100,
            hp: 90,
            hunger: 0.25,
            ammo: 30,
            wood: 12,
            double_jump: false,
            night: false,
            t: 1000.5,
            x: 10.0,
            saves: 1,
            enemy_hp: 50,
            steps: 1,
            mystery: 7,
        }
    }

    fn json(&self) -> Vec<u8> {
        let enemies: Vec<String> = (0..6).map(|i| format!("{{\"hp\": {}}}", self.enemy_hp + i)).collect();
        format!(
            r#"{{
  "player": {{
    "gold": {}, "hp": {}, "maxHP": 100, "hunger": {}, "ammo": {}, "wood": {},
    "hasDoubleJump": {}, "position": {{"x": {}, "y": 3.5}}
  }},
  "meta": {{"timePlayed": {}, "lastSaveTime": 1700000000, "saveCount": {}, "seed": 42, "isNight": {}}},
  "world": {{"enemies": [{}], "stepCounter": {}, "mystery": {}}}
}}
"#,
            self.gold,
            self.hp,
            self.hunger,
            self.ammo,
            self.wood,
            self.double_jump,
            self.x,
            self.t,
            self.saves,
            self.night,
            enemies.join(", "),
            self.steps,
            self.mystery
        )
        .into_bytes()
    }

    /// What happens between two saves while playing: the clock runs, the
    /// player walks, the save counter goes up, day/night flips, every enemy
    /// takes damage.
    fn tick(&mut self) {
        self.t += 300.0;
        self.x += 4.5;
        self.saves += 1;
        self.night = !self.night;
        self.enemy_hp -= 3;
        self.steps += 17;
    }
}

struct Env {
    _tmp: tempfile::TempDir,
    store: Store,
    engine: Engine,
    saves: PathBuf,
    game: GameId,
}

fn env_with(config: SaveLensConfig) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let saves = tmp.path().join("Saves");
    fs::create_dir_all(&saves).unwrap();
    fs::write(saves.join("slot1.json"), State::new().json()).unwrap();
    let store = Store::open(tmp.path().join("data")).unwrap();
    let engine = Engine::new(store.clone(), EnvelopePipeline::builtin(), config);
    let (game, _) = engine.track("Survival RPG", None, &saves, &["*.json".to_string()], "native").unwrap();
    Env { saves: saves.canonicalize().unwrap(), store, engine, _tmp: tmp, game: game.id }
}

fn env() -> Env {
    env_with(SaveLensConfig::default())
}

/// The game writes the file; the watcher commits it.
fn game_saves(e: &Env, s: &State) -> SaveOutcome {
    fs::write(e.saves.join("slot1.json"), s.json()).unwrap();
    e.engine.commit_game_save(e.game, "slot1.json", s.json()).unwrap()
}

fn labels(qs: &[savelens_cas::WatchQuestion]) -> Vec<String> {
    qs.iter().map(|q| q.candidate().label()).collect()
}

fn path(s: &str) -> NodePath {
    NodePath::parse(s).unwrap()
}

#[test]
fn asks_about_stats_only_and_answers_pin_them() {
    let e = env();
    assert!(e.engine.questions(Some(e.game), false).unwrap().is_empty(), "tracking (import) never asks");

    // Save 2: the player spent gold, lost hp, got hungry, fired, chopped wood,
    // unlocked double jump — and all the noise moved too.
    let mut s = State::new();
    s.tick();
    s.gold = 80;
    s.hp = 72;
    s.hunger = 0.5;
    s.ammo = 24;
    s.wood = 20;
    s.double_jump = true;
    let out = game_saves(&e, &s);
    let c = out.commit.as_ref().expect("commit");
    assert_eq!(c.kind, CommitKind::Auto);
    let asked = labels(&out.questions);
    assert_eq!(asked.len(), 3, "at most 3 questions per save: {asked:?}");
    for noisy in [
        "player.position.x",
        "meta.timePlayed",
        "meta.saveCount",
        "meta.isNight",
        "world.stepCounter",
        "world.enemies[0].hp",
    ] {
        assert!(!asked.contains(&noisy.to_string()), "{noisy} must not be asked: {asked:?}");
    }
    for q in &out.questions {
        assert!(q.guess.is_some(), "named stats come first: {asked:?}");
        assert_eq!(q.status, QuestionStatus::Pending);
        assert_eq!(q.commit_id, Some(c.id));
    }
    let gold = out.questions.iter().find(|q| q.candidate().label() == "player.gold").expect("gold asked");
    assert_eq!(gold.guess.as_ref().unwrap().category, VariableCategory::Money);
    assert_eq!(gold.candidate().change_text(), "100 -> 80 (-20)");

    // Answer gold: pinned into the cheat table with a readable alias.
    let (q, entry) = e.engine.answer_question(gold.id, VariableCategory::Money, None).unwrap();
    assert_eq!(q.status, QuestionStatus::Answered);
    assert_eq!(entry.alias, "Money (gold)");
    assert_eq!(entry.location, Location::Ast { path: path("player.gold") });
    let cheats = e.engine.cheat_values(e.game).unwrap();
    assert_eq!(cheats[0].current, Some(ScalarValue::Int(80)));

    // Ignore hp "and everything like it"; snooze the third one.
    let others: Vec<_> = out.questions.iter().filter(|q| q.id != gold.id).cloned().collect();
    let (_, mute) = e.engine.ignore_question(others[0].id, IgnoreScope::Similar).unwrap();
    assert_eq!(mute.rel_path.as_deref(), Some("slot1.json"));
    e.engine.snooze_question(others[1].id, Some(30)).unwrap();
    assert!(e.engine.questions(Some(e.game), false).unwrap().is_empty());

    // Save 3: gold, the ignored and the snoozed value change again — none is
    // asked again; the stats that were rate-limited before come up now.
    s.tick();
    s.gold = 60;
    s.hp = 50;
    s.hunger = 0.75;
    s.ammo = 12;
    s.wood = 25;
    let out = game_saves(&e, &s);
    let asked = labels(&out.questions);
    assert!(!asked.contains(&"player.gold".to_string()), "pinned: {asked:?}");
    assert!(!asked.contains(&others[0].candidate().label()), "ignored: {asked:?}");
    assert!(!asked.contains(&others[1].candidate().label()), "snoozed: {asked:?}");
    assert!(!asked.is_empty(), "the remaining stats are asked");
    let all = e.engine.questions(Some(e.game), true).unwrap();
    assert!(all.iter().any(|q| q.status == QuestionStatus::Snoozed));

    // A flag answered as an unlock can be pinned and frozen like a number.
    let flag = e
        .engine
        .questions(Some(e.game), false)
        .unwrap()
        .into_iter()
        .chain(all.clone())
        .find(|q| q.candidate().label() == "player.hasDoubleJump");
    if let Some(flag) = flag {
        assert!(flag.flag);
        assert_eq!(flag.candidate().change_text(), "OFF -> ON");
        let (_, entry) = e.engine.answer_question(flag.id, VariableCategory::Unlock, Some("Double jump")).unwrap();
        let v = e.engine.cheat_values(e.game).unwrap().into_iter().find(|c| c.entry.alias == entry.alias).unwrap();
        assert_eq!(v.current, Some(ScalarValue::Int(1)));
    }
}

#[test]
fn only_the_games_own_saves_raise_questions() {
    let e = env();
    let mut s = State::new();
    s.tick();
    s.gold = 5;
    let out = game_saves(&e, &s);
    for q in &out.questions {
        e.engine.ignore_question(q.id, IgnoreScope::This).unwrap();
    }
    let before = e.engine.questions(Some(e.game), true).unwrap().len();

    // Checkout of the previous commit: values change on disk, nobody asks.
    e.engine.checkout(e.game, "HEAD~1", None).unwrap();
    // A manual snapshot after the player edited the file by hand.
    let mut m = State::new();
    m.mystery = 99;
    fs::write(e.saves.join("slot1.json"), m.json()).unwrap();
    assert!(e.engine.snapshot(e.game, "manual", CommitKind::Manual).unwrap().is_some());
    // An edit through SaveLens.
    e.engine
        .edit_value(e.game, "slot1.json", &EditTarget::Path { path: path("player.ammo"), value: "999".into() }, false)
        .unwrap();
    assert_eq!(e.engine.questions(Some(e.game), true).unwrap().len(), before);
}

#[test]
fn values_changing_on_every_save_are_learned_as_noise() {
    // Phase 1: no questions at all (queue size 0), only the history is learned.
    let quiet = SaveLensConfig {
        watch: WatchConfig { max_pending_per_game: 0, ..WatchConfig::default() },
        ..SaveLensConfig::default()
    };
    let e = env_with(quiet);
    let mut s = State::new();
    for _ in 0..5 {
        s.tick();
        let out = game_saves(&e, &s);
        assert!(out.questions.is_empty());
    }
    // Phase 2: same store, normal config. `stepCounter` went up on every
    // save: volatile. `mystery` changes for the first time: asked.
    let engine = Engine::new(e.store.clone(), EnvelopePipeline::builtin(), SaveLensConfig::default());
    s.tick();
    s.mystery = 8;
    fs::write(e.saves.join("slot1.json"), s.json()).unwrap();
    let out = engine.commit_game_save(e.game, "slot1.json", s.json()).unwrap();
    let asked = labels(&out.questions);
    assert_eq!(asked, ["world.mystery"]);
}

#[test]
fn queue_is_bounded_and_mutes_apply_to_new_saves() {
    let cfg = SaveLensConfig {
        watch: WatchConfig { max_pending_per_game: 2, global_mutes: vec!["*.wood".into()], ..WatchConfig::default() },
        ..SaveLensConfig::default()
    };
    let e = env_with(cfg);
    e.engine.mute_pattern(e.game, None, "player.ammo").unwrap();
    let mut s = State::new();
    s.tick();
    s.gold = 1;
    s.hp = 1;
    s.hunger = 1.0;
    s.ammo = 1;
    s.wood = 1;
    let out = game_saves(&e, &s);
    let asked = labels(&out.questions);
    assert_eq!(asked.len(), 2, "queue limit: {asked:?}");
    assert!(!asked.contains(&"player.ammo".to_string()));
    assert!(!asked.contains(&"player.wood".to_string()));
    s.tick();
    s.gold = 2;
    s.hp = 2;
    let out = game_saves(&e, &s);
    assert!(out.questions.is_empty(), "queue full: nothing new, pending ones refreshed");
    let pending = e.engine.questions(Some(e.game), false).unwrap();
    assert_eq!(pending.len(), 2);
    assert!(pending.iter().all(|q| q.times_seen >= 1));
}

fn wait_for<F: Fn(&DaemonEvent) -> bool>(h: &DaemonHandle, f: F, timeout: Duration) -> Option<DaemonEvent> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(ev) = h.events().recv_timeout(Duration::from_millis(100)) {
            if f(&ev) {
                return Some(ev);
            }
        }
    }
    None
}

#[test]
fn daemon_raises_questions_without_blocking_the_trainer() {
    let cfg = SaveLensConfig {
        debounce_ms: 300,
        close_write_quiet_ms: 100,
        stability_ms: 30,
        lock_timeout_ms: 2000,
        ..SaveLensConfig::default()
    };
    let e = env_with(cfg);
    // A frozen value must keep working while questions are pending.
    e.engine.pin(e.game, "slot1.json", "Ammo", &Location::Ast { path: path("player.ammo") }).unwrap();
    e.engine.freeze(e.game, "Ammo", Some(ScalarValue::Int(999)), true).unwrap();
    e.store.set_trainer_enabled(e.game, true).unwrap();
    let handle =
        Daemon::spawn(e.engine.clone(), DaemonOptions { hotkeys: false, games: None, tick: Duration::from_millis(20) })
            .unwrap();
    assert!(wait_for(&handle, |ev| matches!(ev, DaemonEvent::Watching { .. }), Duration::from_secs(5)).is_some());

    let mut s = State::new();
    s.tick();
    s.gold = 70;
    s.ammo = 3;
    fs::write(e.saves.join("slot1.json"), s.json()).unwrap();
    let ev = wait_for(&handle, |ev| matches!(ev, DaemonEvent::Questions { .. }), Duration::from_secs(10))
        .expect("questions event");
    let DaemonEvent::Questions { questions, rel_path, .. } = ev else { unreachable!() };
    assert_eq!(rel_path, "slot1.json");
    let asked = labels(&questions);
    assert!(asked.contains(&"player.gold".to_string()), "{asked:?}");
    assert!(!asked.contains(&"player.ammo".to_string()), "pinned values are never asked: {asked:?}");
    assert!(wait_for(&handle, |ev| matches!(ev, DaemonEvent::TrainerApplied { .. }), Duration::from_secs(10)).is_some());
    let root = e.engine.root_at(e.game, WORKING, "slot1.json").unwrap();
    assert_eq!(root.get(&path("player.ammo")), Some(&Node::I64(999)));
    // The trainer's own write (and its commit) raise nothing new.
    assert!(wait_for(&handle, |ev| matches!(ev, DaemonEvent::Questions { .. }), Duration::from_millis(1500)).is_none());
    handle.stop().unwrap();
}
