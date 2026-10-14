//! GVAS integration tests on real Unreal Engine 4/5 save games.
//!
//! The corpus lives in `tests/real-saves/corpus/unreal/` at the workspace
//! root (override with `SAVELENS_CORPUS=<corpus dir>`). When it is missing,
//! the tests print a note and pass (unless `SAVELENS_REQUIRE_REAL_SAVES` is set).

use std::path::PathBuf;

use savelens_codecs::{DetectContext, EnvelopePipeline, OpenedSave};
use savelens_core::{bit_equal, Node, NodePath, RoundTripStatus};

fn corpus() -> Option<PathBuf> {
    let base = match std::env::var_os("SAVELENS_CORPUS") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/real-saves/corpus"),
    };
    let dir = base.join("unreal");
    if dir.is_dir() {
        Some(dir)
    } else {
        assert!(
            std::env::var_os("SAVELENS_REQUIRE_REAL_SAVES").is_none(),
            "real-save corpus required but not found at {}",
            dir.display()
        );
        eprintln!("real-save corpus not found at {}; skipping", dir.display());
        None
    }
}

fn p(s: &str) -> NodePath {
    NodePath::parse(s).unwrap()
}

/// `(file, expected codec, raw Bytes nodes before the FText / delegate /
/// field path / tag container work, maximum raw Bytes nodes now)`.
///
/// The Palworld saves are wrapped in Palworld's own `PlZ` zlib container,
/// which no envelope layer unwraps yet, so they open (losslessly) with the
/// raw codec.
const FILES: &[(&str, &str, usize, usize)] = &[
    ("Delegate.sav", "gvas", 3, 0),
    ("Options.sav", "gvas", 0, 0),
    ("Profile_0.sav", "gvas", 0, 0),
    ("SaveSlot_03.sav", "gvas", 2, 0),
    ("Slot1.sav", "gvas", 0, 0),
    ("Slot2.sav", "gvas", 2, 0),
    ("Slot3.sav", "gvas", 2, 0),
    ("assert_failed.sav", "gvas", 0, 0),
    ("component8.sav", "gvas", 2, 0),
    ("deep-rock-galactic.sav", "gvas", 1, 0),
    ("drg-proppack.sav", "gvas", 0, 0),
    ("enum_array.sav", "gvas", 3, 0),
    ("features_01.bin", "gvas", 1, 0),
    ("package_version_524.sav", "gvas", 0, 0),
    ("package_version_525.sav", "gvas", 0, 0),
    ("palworld_zlib.sav", "gvas", 0, 0),
    ("palworld_zlib_twice.sav", "gvas", 0, 0),
    ("regression_01.bin", "gvas", 0, 0),
    ("ro_64bit_fav.sav", "gvas", 3, 0),
    ("string_table_entry.sav", "gvas", 3, 0),
    ("tagcontainer.sav", "gvas", 1, 0),
    ("text_property_noarray.bin", "gvas", 368, 0),
    ("transform.sav", "gvas", 3, 0),
    ("vector2d.sav", "gvas", 1, 0),
];

fn raw_nodes(root: &Node) -> Vec<(String, usize)> {
    let mut raws = Vec::new();
    root.walk(|path: &NodePath, n: &Node| {
        if let Node::Bytes(b) = n {
            raws.push((path.to_string(), b.len()));
        }
    });
    raws
}

fn open(dir: &std::path::Path, name: &str) -> (Vec<u8>, OpenedSave) {
    let data = std::fs::read(dir.join(name)).unwrap();
    let s = EnvelopePipeline::builtin().open(&data, &DetectContext::for_file(name)).unwrap();
    (data, s)
}

#[test]
fn every_real_save_round_trips_with_few_raw_values() {
    let Some(dir) = corpus() else { return };
    let mut listed: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".sav") || n.ends_with(".bin"))
        .collect();
    listed.sort();
    let known: Vec<&str> = FILES.iter().map(|f| f.0).collect();
    for name in &listed {
        assert!(known.contains(&name.as_str()), "{name} is not in the expectation table");
    }
    let (mut before, mut after) = (0, 0);
    for &(name, codec, raw_before, raw_max) in FILES {
        let (data, s) = open(&dir, name);
        assert_eq!(s.codec_id(), codec, "{name}: {:?}", s.envelope().notes);
        assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless), "{name}");
        assert_eq!(s.encode().unwrap(), data, "{name}");
        let raws = raw_nodes(&s.root());
        println!("{name}: raw Bytes nodes {raw_before} -> {}", raws.len());
        assert!(
            raws.len() <= raw_max,
            "{name}: {} raw nodes (max {raw_max}): {:?}",
            raws.len(),
            &raws[..raws.len().min(10)]
        );
        before += raw_before;
        after += raws.len();
    }
    println!("total raw Bytes nodes {before} -> {after}");
}

/// Applies `edits`, encodes with verification, reopens and checks that the
/// reopened tree equals the original tree with exactly those edits.
fn edit_and_verify(name: &str, edits: &[(&str, Node)]) -> OpenedSave {
    let dir = corpus().unwrap();
    let (data, mut s) = open(&dir, name);
    let mut expected = s.root();
    for (path, v) in edits {
        let path = p(path);
        assert!(expected.get(&path).is_some(), "{name}: {path} missing");
        expected.set(&path, v.clone(), false).unwrap();
    }
    s.apply_ast(&expected).unwrap();
    let ctx = DetectContext::for_file(name);
    let pipeline = EnvelopePipeline::builtin();
    let out = s.encode_verified(&pipeline, &ctx).unwrap();
    assert_ne!(out, data);
    let re = pipeline.open(&out, &ctx).unwrap();
    assert_eq!(re.codec_id(), "gvas");
    assert_eq!(re.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert!(bit_equal(&re.root(), &expected), "{name}: reopened tree differs from the edited tree");
    re
}

#[test]
fn deep_rock_galactic_credits() {
    if corpus().is_none() {
        return;
    }
    let re = edit_and_verify("deep-rock-galactic.sav", &[("Credits", Node::I32(9_999_999))]);
    assert_eq!(re.root().get(&p("Credits")), Some(&Node::I32(9_999_999)));
}

#[test]
fn text_source_string_edit_changes_lengths() {
    let Some(dir) = corpus() else { return };
    let name = "text_property_noarray.bin";
    let path = "SaveGameData.QuestsStatus[0].QuestLogTitle";
    let (_, s) = open(&dir, name);
    let text = s.root().get(&p(path)).cloned().unwrap();
    assert_eq!(text.get(&p("HistoryType")), Some(&Node::String("Base".into())));
    assert_eq!(text.get(&p("Namespace")), Some(&Node::String("QuestInfosDataTable".into())));
    assert_eq!(text.get(&p("Key")), Some(&Node::String("Q101_CheckJavelinDamage".into())));
    assert_eq!(text.get(&p("SourceString")), Some(&Node::String("Landing Protocol".into())));
    // A culture-invariant `None` history.
    let none = s.root().get(&p("SaveGameData.QuestsStatus[0].QuestLogDetails")).cloned().unwrap();
    assert_eq!(none.get(&p("HistoryType")), Some(&Node::String("None".into())));
    assert!(matches!(none.get(&p("CultureInvariantString")), Some(Node::String(t)) if t.starts_with("Unable to get")));

    let re = edit_and_verify(
        name,
        &[(&format!("{path}.SourceString"), Node::String("Landing Protocol \u{2013} Reloaded".into()))],
    );
    assert_eq!(
        re.root().get(&p(&format!("{path}.SourceString"))),
        Some(&Node::String("Landing Protocol \u{2013} Reloaded".into()))
    );

    // Layout-deciding fields are read-only.
    let (_, mut s) = open(&dir, name);
    let mut root = s.root();
    root.set(&p(&format!("{path}.HistoryType")), Node::String("None".into()), false).unwrap();
    assert!(s.apply_ast(&root).is_err());
}

#[test]
fn gameplay_tag_container_edit_and_append() {
    if corpus().is_none() {
        return;
    }
    let name = "tagcontainer.sav";
    let re = edit_and_verify(name, &[("TagTest[1]", Node::String("Character.Appearance.OverallColor.Black".into()))]);
    assert_eq!(re.root().get(&p("TagTest[0]")), Some(&Node::String("Character.Appearance.OverallColor.Gold".into())));

    // Tags can be appended (the struct size is recomputed).
    let Some(Node::Array(mut tags)) = re.root().get(&p("TagTest")).cloned() else { panic!("TagTest is not an array") };
    tags.push(Node::String("Character.Appearance.Hat.Crown".into()));
    let re = edit_and_verify(name, &[("TagTest", Node::Array(tags))]);
    assert_eq!(re.root().get(&p("TagTest[4]")), Some(&Node::String("Character.Appearance.Hat.Crown".into())));
}

#[test]
fn delegates_and_field_paths_are_decoded() {
    let Some(dir) = corpus() else { return };
    let (_, s) = open(&dir, "Delegate.sav");
    let root = s.root();
    assert_eq!(root.get(&p("DynamicDelegate.FunctionName")), Some(&Node::String("FirstBinding".into())));
    assert_eq!(root.get(&p("MulticastDelegate[1].FunctionName")), Some(&Node::String("SecondBinding".into())));
    assert_eq!(root.get(&p("MulticastSparseDelegate[0].FunctionName")), Some(&Node::String("FirstBinding".into())));
    edit_and_verify("Delegate.sav", &[("MulticastDelegate[1].FunctionName", Node::String("ThirdBinding".into()))]);

    let (_, s) = open(&dir, "SaveSlot_03.sav");
    let root = s.root();
    assert_eq!(
        root.get(&p("PlayerAttributes[0].key.Attribute.Path[0]")),
        Some(&Node::String("Currency_Blueprints".into()))
    );
    assert_eq!(
        root.get(&p("PlayerAttributes[0].key.Attribute.ResolvedOwner")),
        Some(&Node::String("/Script/CD.CDPlayerAttributeSet".into()))
    );

    let (_, s) = open(&dir, "deep-rock-galactic.sav");
    assert!(matches!(s.root().get(&p("OnVanityMasteryChanged")), Some(Node::Array(v)) if v.len() == 1));
}
