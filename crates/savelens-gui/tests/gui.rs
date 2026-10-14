//! Headless tests of the SaveLens GUI: real engine on a temporary directory,
//! frames rendered through `egui::Context::run`, and the view-model actions
//! (scanner, cheat table, cherry-pick, panic restore, commit graph).

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use egui::{Pos2, RawInput, Rect, Vec2};
use savelens_cas::{CommitId, CommitKind, GameId, Store};
use savelens_codecs::EnvelopePipeline;
use savelens_core::{Location, Node, NodePath, ScalarValue};
use savelens_gui::{theme, FilterKind, NodeStatus, SaveLensApp, Side, ToastKind, View};
use savelens_watcher::{Engine, SaveLensConfig, WORKING};

const WAIT: Duration = Duration::from_secs(30);

fn save_json(gold: i64, items: &[&str], quest: &str) -> Vec<u8> {
    let items: Vec<String> = items.iter().map(|i| format!("\"{i}\"")).collect();
    format!(
        "{{\n  \"Player\": {{\n    \"Gold\": {gold},\n    \"HP\": 87.5,\n    \"Inventory\": [{}]\n  }},\n  \"Quests\": {{ \"Main_02\": \"{quest}\" }}\n}}\n",
        items.join(", ")
    )
    .into_bytes()
}

struct Fx {
    _tmp: tempfile::TempDir,
    engine: Engine,
    saves: PathBuf,
    game: GameId,
    import: CommitId,
    second: CommitId,
}

impl Fx {
    fn slot(&self) -> PathBuf {
        self.saves.join("slot1.json")
    }
}

fn fixture() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let saves = tmp.path().join("Saves");
    fs::create_dir_all(&saves).unwrap();
    fs::write(saves.join("slot1.json"), save_json(1200, &["Sword", "Shield"], "Active")).unwrap();
    let store = Store::open(tmp.path().join("data")).unwrap();
    let engine = Engine::new(store, EnvelopePipeline::builtin(), SaveLensConfig::default());
    let (game, import) = engine.track("Test RPG", Some(42), &saves, &["*.json".to_string()], "native").unwrap();
    // The game saves again: more gold, lost the inventory, finished the quest.
    fs::write(saves.join("slot1.json"), save_json(1500, &["Stick"], "Completed")).unwrap();
    let second = engine.snapshot(game.id, "game saved", CommitKind::Auto).unwrap().unwrap();
    Fx {
        saves: saves.canonicalize().unwrap(),
        engine,
        game: game.id,
        import: import.unwrap().id,
        second: second.id,
        _tmp: tmp,
    }
}

fn app_for(fx: &Fx) -> SaveLensApp {
    let mut app = SaveLensApp::new(fx.engine.clone()).unwrap();
    assert!(app.wait_idle(WAIT), "initial loading finished");
    assert_eq!(app.selected_game(), Some(fx.game));
    assert_eq!(app.selected_file(), Some("slot1.json"));
    app
}

fn frame(ctx: &egui::Context, app: &mut SaveLensApp, size: Vec2) {
    let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)), ..RawInput::default() };
    let _ = ctx.run(input, |ctx| app.ui(ctx));
}

fn no_error_toasts(app: &SaveLensApp) {
    let errors: Vec<&str> =
        app.toasts().items().iter().filter(|t| t.kind == ToastKind::Error).map(|t| t.text.as_str()).collect();
    assert!(errors.is_empty(), "unexpected errors: {errors:?}");
}

fn gold() -> NodePath {
    NodePath::parse("Player.Gold").unwrap()
}

fn inventory() -> NodePath {
    NodePath::parse("Player.Inventory").unwrap()
}

#[test]
fn every_view_renders_headless_at_any_size() {
    let fx = fixture();
    let mut app = app_for(&fx);
    let ctx = egui::Context::default();
    let sizes = [
        Vec2::new(1400.0, 900.0),
        Vec2::new(800.0, 600.0),
        Vec2::new(320.0, 200.0),
        Vec2::new(40.0, 30.0),
        Vec2::new(1.0, 1.0),
    ];
    for view in View::ALL {
        app.set_view(view);
        assert!(app.wait_idle(WAIT));
        for size in sizes {
            for _ in 0..2 {
                frame(&ctx, &mut app, size);
            }
        }
        assert_eq!(app.view(), view);
    }
    assert!(app.diff_model().is_some(), "diff view loaded its trees");
    assert!(app.editor_root().is_some(), "editor loaded the working tree");
    assert!(app.integrity().is_some_and(|i| i.badges.iter().any(|b| b == "Lossless Verified")));

    // Scanner with results, the cheat table, every modal.
    app.set_view(View::Scanner);
    app.scanner_mut().value_a = "1500".into();
    app.start_first_scan().unwrap();
    assert!(app.wait_idle(WAIT));
    app.add_candidate_to_cheats(0, None).unwrap();
    assert!(app.wait_idle(WAIT));
    app.cheats_mut().set_edit("Gold", "1600");
    for size in sizes {
        frame(&ctx, &mut app, size);
    }
    app.set_view(View::TreeDiff);
    app.request_cherry_pick(vec![inventory()]);
    for size in sizes {
        frame(&ctx, &mut app, size);
    }
    app.cancel_cherry_pick();
    app.edit_value(gold(), "1700".into()).unwrap();
    assert!(app.wait_idle(WAIT));
    app.request_panic(false).unwrap();
    for size in sizes {
        frame(&ctx, &mut app, size);
    }
    app.cancel_panic();
    // Timeline with the changelog of every commit expanded.
    app.set_view(View::Timeline);
    assert!(app.wait_idle(WAIT));
    let ids: Vec<CommitId> = app.history().unwrap().commits.iter().map(|c| c.id).collect();
    for id in ids {
        app.toggle_commit_expanded(id);
    }
    for size in sizes {
        frame(&ctx, &mut app, size);
    }
    no_error_toasts(&app);
}

#[test]
fn theme_constants_are_applied_to_the_context() {
    let fx = fixture();
    let mut app = app_for(&fx);
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, Vec2::new(1400.0, 900.0));
    let style = ctx.style();
    assert_eq!(style.visuals.panel_fill, theme::BG_PANEL);
    assert_eq!(style.visuals.window_fill, theme::BG_CARD);
    assert_eq!(style.visuals.extreme_bg_color, theme::BG_INPUT);
    assert_eq!(style.visuals.hyperlink_color, theme::CYAN);
    assert_eq!(style.visuals.error_fg_color, theme::RED);
    assert_eq!(style.visuals.widgets.inactive.corner_radius, egui::CornerRadius::same(theme::RADIUS));
    assert_eq!(style.spacing.item_spacing.x, theme::SPACING);
    assert!(style.visuals.dark_mode);
    assert_eq!(ctx.theme(), egui::Theme::Dark);
}

#[test]
fn scanner_flow_cheat_table_and_apply_and_sign() {
    let fx = fixture();
    let mut app = app_for(&fx);

    // First scan on the imported snapshot: exact 1200.
    {
        let s = app.scanner_mut();
        s.source_rev = format!("#{}", fx.import);
        s.filter = FilterKind::Exact;
        s.value_a = "1200".into();
    }
    app.start_first_scan().unwrap();
    assert!(app.scan_running());
    assert!(app.wait_idle(WAIT));
    assert!(app.scanner().total() >= 1, "1200 found");
    assert!(app.scanner().error.is_none());

    // Next scan on the working file: transition 1200 -> 1500.
    {
        let s = app.scanner_mut();
        s.source_rev = WORKING.into();
        s.filter = FilterKind::Transition;
        s.value_a = "1200".into();
        s.value_b = "1500".into();
    }
    app.start_next_scan().unwrap();
    assert!(app.wait_idle(WAIT));
    assert_eq!(app.scanner().total(), 1, "{:?}", app.scanner().results());
    let cand = app.scanner().results()[0].clone();
    assert_eq!(cand.location, Location::Ast { path: gold() });
    assert_eq!((cand.previous, cand.current), (ScalarValue::Int(1200), ScalarValue::Int(1500)));
    assert_eq!(app.scanner().history().len(), 2);

    // A first-scan-only filter is refused for next scans.
    app.scanner_mut().filter = FilterKind::Unknown;
    assert!(app.start_next_scan().is_err());
    app.scanner_mut().filter = FilterKind::Transition;

    // Add to the cheat table: live value from disk.
    app.add_candidate_to_cheats(0, None).unwrap();
    assert!(app.wait_idle(WAIT));
    let rows = &app.cheats().rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].entry.alias, "Gold");
    assert_eq!(rows[0].current, Some(ScalarValue::Int(1500)));

    // Nothing edited yet.
    assert!(app.apply_and_sign().is_err());
    app.cheats_mut().set_edit("Gold", "abc");
    assert!(app.apply_and_sign().is_err(), "invalid number refused");

    // Edit and Apply & Sign.
    app.cheats_mut().set_edit("Gold", "2500");
    assert_eq!(app.cheats().pending_edits().unwrap(), vec![("Gold".to_string(), ScalarValue::Int(2500))]);
    app.apply_and_sign().unwrap();
    assert!(app.wait_idle(WAIT));
    let text = fs::read_to_string(fx.slot()).unwrap();
    assert!(text.contains("\"Gold\": 2500,"), "{text}");
    assert!(text.contains("\"HP\": 87.5"), "rest of the file untouched: {text}");
    let tag = app.last_safety_tag().unwrap().to_string();
    assert!(tag.starts_with("safety/pre-mutation-"), "{tag}");
    assert_eq!(app.cheats().last_safety_tag.as_deref(), Some(tag.as_str()));
    assert_eq!(app.cheats().rows[0].current, Some(ScalarValue::Int(2500)), "live value refreshed");
    assert!(app.cheats().edits.is_empty());
    let head = &app.history().unwrap().commits[0];
    assert_eq!(head.kind, CommitKind::Edit);
    assert!(app.toasts().items().iter().any(|t| t.kind == ToastKind::Success && t.text.contains(&tag)));

    // Freeze rule and trainer toggle go through the engine.
    app.cheats_mut().freeze_inputs.insert("Gold".into(), "9999".into());
    app.set_freeze("Gold", true).unwrap();
    app.set_trainer(true).unwrap();
    assert!(app.wait_idle(WAIT));
    let entry = &app.cheats().rows[0].entry;
    assert!(entry.freeze_enabled);
    assert_eq!(entry.frozen_value, Some(ScalarValue::Int(9999)));
    assert!(app.games()[0].trainer_enabled);
    app.delete_cheat("Gold").unwrap();
    assert!(app.wait_idle(WAIT));
    assert!(app.cheats().rows.is_empty());

    app.reset_scan();
    assert_eq!(app.scanner().total(), 0);
    no_error_toasts(&app);
}

#[test]
fn cherry_pick_from_the_diff_view_restores_the_inventory() {
    let fx = fixture();
    let mut app = app_for(&fx);
    app.set_view(View::TreeDiff);
    app.set_diff_source(&format!("#{}", fx.import));
    assert!(app.wait_idle(WAIT));
    let m = app.diff_model().unwrap();
    assert_ne!(m.status(Side::Left, &inventory()), NodeStatus::Same);
    assert_eq!(m.status(Side::Left, &gold()), NodeStatus::Modified);
    assert_eq!(m.status(Side::Right, &NodePath::parse("Player.HP").unwrap()), NodeStatus::Same);

    // Search filter.
    app.set_diff_search("inventory");
    assert!(app.wait_idle(WAIT));

    // Multi-select, then preview & confirm.
    app.toggle_pick_selection(&inventory());
    assert_eq!(app.pick_selection(), vec![inventory()]);
    app.request_cherry_pick(app.pick_selection());
    let (paths, preview) = app.pick_preview().unwrap();
    assert_eq!(paths, &[inventory()]);
    assert!(!preview.is_empty());
    assert!(preview.iter().any(|l| l.contains("Inventory")), "{preview:?}");
    app.confirm_cherry_pick().unwrap();
    assert!(app.pick_preview().is_none());
    assert!(app.wait_idle(WAIT));

    let root = fx.engine.root_at(fx.game, WORKING, "slot1.json").unwrap();
    assert_eq!(
        root.get(&inventory()),
        Some(&Node::Array(vec![Node::String("Sword".into()), Node::String("Shield".into())]))
    );
    assert_eq!(root.get(&gold()), Some(&Node::I64(1500)), "later progress kept");
    assert_eq!(root.get(&NodePath::parse("Quests.Main_02").unwrap()), Some(&Node::String("Completed".into())));
    // The diff was reloaded: the inventory is now identical on both sides.
    assert_eq!(app.diff_model().unwrap().status(Side::Left, &inventory()), NodeStatus::Same);
    assert_eq!(app.history().unwrap().commits[0].kind, CommitKind::CherryPick);
    assert!(app.last_safety_tag().is_some_and(|t| t.starts_with("safety/pre-mutation-")));
    no_error_toasts(&app);
}

#[test]
fn panic_restore_action_restores_the_original_bytes() {
    let fx = fixture();
    let mut app = app_for(&fx);
    assert!(app.request_panic(false).is_err(), "nothing mutated yet");
    let before = fs::read(fx.slot()).unwrap();

    app.set_view(View::Editor);
    assert!(app.wait_idle(WAIT));
    app.edit_value(gold(), "777".into()).unwrap();
    assert!(app.wait_idle(WAIT));
    let edited = fs::read(fx.slot()).unwrap();
    assert_ne!(edited, before);
    assert!(String::from_utf8_lossy(&edited).contains("\"Gold\": 777,"));
    assert_eq!(app.editor_root().unwrap().get(&gold()), Some(&Node::I64(777)), "editor reloaded");

    app.request_panic(true).unwrap();
    let p = app.panic_prompt().unwrap();
    assert!(p.tag.starts_with("safety/pre-mutation-"), "{}", p.tag);
    assert_eq!(p.path, savelens_discovery::normalize_path(&fx.slot()));
    assert_eq!(p.scope, savelens_cas::PanicScope::Game(fx.game));
    app.confirm_panic().unwrap();
    assert!(app.panic_prompt().is_none());
    assert!(app.wait_idle(WAIT));
    assert_eq!(fs::read(fx.slot()).unwrap(), before, "bit-for-bit restore");
    assert_eq!(app.history().unwrap().commits[0].kind, CommitKind::Restore);
    assert!(app.toasts().items().iter().any(|t| t.kind == ToastKind::Success && t.text.contains("PANIC RESTORE")));
    no_error_toasts(&app);
}

#[test]
fn commit_graph_layout_of_a_branched_history_is_consistent() {
    let fx = fixture();
    let mut app = app_for(&fx);
    app.create_branch_at(fx.import, "exp").unwrap();
    assert!(app.wait_idle(WAIT));
    app.switch_branch("exp").unwrap();
    assert!(app.wait_idle(WAIT));
    assert_eq!(fs::read(fx.slot()).unwrap(), save_json(1200, &["Sword", "Shield"], "Active"), "branch head written");
    fs::write(fx.slot(), save_json(3000, &["Sword", "Shield", "Bow"], "Active")).unwrap();
    app.quick_snapshot().unwrap();
    assert!(app.wait_idle(WAIT));
    app.tag_commit(fx.second, "before-exp").unwrap();
    assert!(app.wait_idle(WAIT));
    assert!(app.create_branch_at(fx.import, "bad name").is_err());

    let h = app.history().unwrap();
    assert_eq!(h.current_branch, "exp");
    assert!(h.tags.iter().any(|t| t.name == "before-exp" && t.commit_id == fx.second));
    let l = &h.layout;
    assert_eq!(l.nodes.len(), h.commits.len());
    assert_eq!(l.row_segments.len(), l.nodes.len());
    assert!(l.lane_of("main").is_some() && l.lane_of("exp").is_some());
    assert_ne!(l.lane_of("main"), l.lane_of("exp"));
    // Rows are newest first and every node sits on its branch's lane.
    for (row, n) in l.nodes.iter().enumerate() {
        assert_eq!(n.row, row);
        let c = h.commits.iter().find(|c| c.id == n.commit).unwrap();
        assert_eq!(Some(n.lane), l.lane_of(&c.branch));
        if row > 0 {
            assert!(l.nodes[row - 1].commit > n.commit);
        }
    }
    // Every parent edge connects existing nodes, downwards.
    let with_parent = l.nodes.iter().filter(|n| n.parent.is_some()).count();
    assert_eq!(l.edges.len(), with_parent);
    for e in &l.edges {
        let child = &l.nodes[e.child_row];
        let parent = &l.nodes[e.parent_row];
        assert_eq!(child.parent, Some(parent.commit));
        assert_eq!((child.lane, parent.lane), (e.child_lane, e.parent_lane));
        assert!(e.parent_row > e.child_row);
    }
    // The exp commit branches off the import commit on main.
    let exp_commit = l.nodes.iter().find(|n| l.lanes[n.lane] == "exp").unwrap();
    assert_eq!(exp_commit.parent, Some(fx.import));
    // Branch heads sit on their lane, the current one is flagged.
    for b in &h.branches {
        let head = l.heads.iter().find(|x| x.name == b.name).unwrap();
        assert_eq!(Some(head.row), b.head.and_then(|id| l.row_of(id)));
        assert_eq!(head.lane, l.nodes[head.row].lane);
        assert_eq!(Some(head.lane), l.lane_of(&b.name), "{} has its own commits", b.name);
        assert_eq!(head.is_current, b.name == "exp");
    }
    no_error_toasts(&app);
}

#[test]
fn editor_edits_leaves_and_tracking_from_the_sidebar() {
    let fx = fixture();
    let mut app = app_for(&fx);
    app.set_view(View::Editor);
    assert!(app.wait_idle(WAIT));
    assert!(app.editor_gate_error().is_none());
    app.set_editor_search("hp");
    app.edit_value(NodePath::parse("Player.HP").unwrap(), "99.5".into()).unwrap();
    assert!(app.wait_idle(WAIT));
    let root = fx.engine.root_at(fx.game, WORKING, "slot1.json").unwrap();
    assert_eq!(root.get(&NodePath::parse("Player.HP").unwrap()), Some(&Node::F64(99.5)));
    // Invalid values are reported, the file is untouched.
    let before = fs::read(fx.slot()).unwrap();
    app.edit_value(gold(), "lots".into()).unwrap();
    assert!(app.wait_idle(WAIT));
    assert_eq!(fs::read(fx.slot()).unwrap(), before);
    assert!(app.toasts().items().iter().any(|t| t.kind == ToastKind::Error && t.text.starts_with("edit:")));

    // Track another folder through the sidebar form.
    let other = fx.saves.parent().unwrap().join("Other");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("profile.sav"), b"\x01\x02\x03\x04hello").unwrap();
    assert!(app.track_folder("", &other, "*.sav").is_err());
    assert!(app.track_folder("Other", &other.join("missing"), "*.sav").is_err());
    app.track_folder("Other Game", &other, "*.sav").unwrap();
    assert!(app.wait_idle(WAIT));
    assert_eq!(app.games().len(), 2);
    let id = app.game_by_name("Other Game").unwrap();
    app.select_game(id);
    assert!(app.wait_idle(WAIT));
    assert_eq!(app.selected_file(), Some("profile.sav"));
    assert_eq!(app.history().unwrap().commits.len(), 1);
    assert!(app.integrity().is_some());
    let ctx = egui::Context::default();
    for v in View::ALL {
        app.set_view(v);
        assert!(app.wait_idle(WAIT));
        frame(&ctx, &mut app, Vec2::new(1200.0, 800.0));
    }
}

#[test]
fn embedded_daemon_starts_and_stops() {
    let fx = fixture();
    let mut app = app_for(&fx);
    app.set_daemon_hotkeys(false);
    let ctx = egui::Context::default();
    app.toggle_daemon();
    assert!(app.daemon_running());
    for _ in 0..20 {
        frame(&ctx, &mut app, Vec2::new(1400.0, 900.0));
        std::thread::sleep(Duration::from_millis(20));
        if app.activity().iter().any(|a| a.source == "daemon") {
            break;
        }
    }
    assert!(app.activity().iter().any(|a| a.source == "daemon"), "{:?}", app.activity());
    app.toggle_daemon();
    assert!(!app.daemon_running());
    frame(&ctx, &mut app, Vec2::new(1400.0, 900.0));
}

/// Screen rectangle of the first text shape equal to `needle`.
fn find_text(shapes: &[egui::epaint::ClippedShape], needle: &str) -> Option<Rect> {
    fn visit(shape: &egui::Shape, needle: &str) -> Option<Rect> {
        match shape {
            egui::Shape::Text(t) if t.galley.text() == needle => Some(t.galley.rect.translate(t.pos.to_vec2())),
            egui::Shape::Vec(v) => v.iter().find_map(|s| visit(s, needle)),
            _ => None,
        }
    }
    shapes.iter().find_map(|c| visit(&c.shape, needle))
}

/// Renders a frame and returns its shapes.
fn frame_shapes(
    ctx: &egui::Context,
    app: &mut SaveLensApp,
    events: Vec<egui::Event>,
) -> Vec<egui::epaint::ClippedShape> {
    let input = RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1400.0, 900.0))),
        events,
        ..RawInput::default()
    };
    ctx.run(input, |ctx| app.ui(ctx)).shapes
}

/// Clicks the center of the text `label` (press in one frame, release in the next).
fn click_text(ctx: &egui::Context, app: &mut SaveLensApp, label: &str) {
    let shapes = frame_shapes(ctx, app, Vec::new());
    let rect = find_text(&shapes, label).unwrap_or_else(|| panic!("`{label}` is not on screen"));
    let pos = rect.center();
    let button = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    frame_shapes(ctx, app, vec![egui::Event::PointerMoved(pos), button(true)]);
    frame_shapes(ctx, app, vec![button(false)]);
}

#[test]
fn simulated_clicks_switch_views_and_drive_the_panic_button() {
    let fx = fixture();
    let mut app = app_for(&fx);
    let ctx = egui::Context::default();
    frame_shapes(&ctx, &mut app, Vec::new());

    click_text(&ctx, &mut app, "Scanner Studio");
    assert_eq!(app.view(), View::Scanner);
    click_text(&ctx, &mut app, "Tree Diff & Cherry-Pick");
    assert_eq!(app.view(), View::TreeDiff);
    assert!(app.wait_idle(WAIT));

    // Mutate the save, then press the big red button and confirm in the modal.
    let before = fs::read(fx.slot()).unwrap();
    app.edit_value(gold(), "4242".into()).unwrap();
    assert!(app.wait_idle(WAIT));
    assert_ne!(fs::read(fx.slot()).unwrap(), before);
    click_text(&ctx, &mut app, "⚠  PANIC RESTORE ORIGINAL");
    let tag = app.panic_prompt().map(|p| p.tag.clone()).unwrap();
    // Grids and modals need a sizing pass before they paint.
    let shown = (0..3).any(|_| find_text(&frame_shapes(&ctx, &mut app, Vec::new()), &tag).is_some());
    assert!(shown, "the confirmation shows the safety tag");
    click_text(&ctx, &mut app, "RESTORE NOW");
    assert!(app.panic_prompt().is_none());
    assert!(app.wait_idle(WAIT));
    assert_eq!(fs::read(fx.slot()).unwrap(), before);
    no_error_toasts(&app);
}

#[test]
fn what_changed_window_pins_answers() {
    let fx = fixture();
    let mut app = app_for(&fx);
    let ctx = egui::Context::default();
    frame(&ctx, &mut app, Vec2::new(1280.0, 800.0));
    let q = app
        .questions()
        .iter()
        .find(|q| q.candidate().label() == "Player.Gold")
        .cloned()
        .expect("the game save raised a question about Player.Gold");
    assert_eq!(q.guess.as_ref().map(|g| g.category), Some(savelens_core::VariableCategory::Money));
    let msg = app
        .answer(q.id, savelens_gui::questions::QuestionChoice::Answer(savelens_core::VariableCategory::Money), None)
        .unwrap();
    assert!(msg.contains("Money (gold)"), "{msg}");
    assert!(app.wait_idle(WAIT));
    frame(&ctx, &mut app, Vec2::new(1280.0, 800.0));
    assert!(app.questions().iter().all(|p| p.id != q.id));
    let cheats = fx.engine.cheat_values(fx.game).unwrap();
    assert!(cheats.iter().any(|c| c.entry.alias == "Money (gold)" && c.current == Some(ScalarValue::Int(1500))));
    no_error_toasts(&app);
}
