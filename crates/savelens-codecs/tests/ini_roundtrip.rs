//! INI / CFG integration tests through the full envelope pipeline.

use base64::Engine;
use savelens_codecs::{DetectContext, EnvelopePipeline};
use savelens_core::{Node, NodePath, RoundTripStatus};

fn p(s: &str) -> NodePath {
    NodePath::parse(s).unwrap()
}

/// A Ren'Py / GameMaker style save: BOM, CRLF (plus one LF line), global
/// keys, comments, quoted strings, inline comments, duplicate keys, an
/// unrecognised line and no final newline.
fn save() -> Vec<u8> {
    let text = concat!(
        "; Autosave written by the game - do not edit\r\n",
        "version = 3\r\n",
        "build=\"1.0.7\"\r\n",
        "\r\n",
        "[Player]\r\n",
        "name = \"Sir Lancelot\"  ; display name\r\n",
        "gold = 1250\r\n",
        "hp=87.50\r\n",
        "speed : 1.5e2\r\n",
        "hardcore = False\r\n",
        "\r\n",
        "# inventory slots (duplicates are intentional)\n",
        "[Inventory]\r\n",
        "slot = sword\r\n",
        "slot = shield\r\n",
        "slot = potion ; x3\r\n",
        "!!garbage line kept verbatim\r\n",
        "\r\n",
        "[ Flags ]   ; story flags\r\n",
        "\tmet_merlin\t=\ttrue\r\n",
        "chapter=007",
    );
    let mut v = vec![0xef, 0xbb, 0xbf];
    v.extend_from_slice(text.as_bytes());
    v
}

fn ctx() -> DetectContext {
    DetectContext::for_file("autosave.ini")
}

#[test]
fn detects_and_round_trips() {
    let pipeline = EnvelopePipeline::builtin();
    let data = save();
    let s = pipeline.open(&data, &ctx()).unwrap();
    assert_eq!(s.codec_id(), "ini");
    assert!(s.layers().is_empty());
    assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert_eq!(s.encode().unwrap(), data);

    let root = s.root();
    let g = |path: &str| root.get(&p(path)).cloned().unwrap();
    assert_eq!(g("version"), Node::I64(3));
    assert_eq!(g("build"), Node::String("1.0.7".into()));
    assert_eq!(g("Player.name"), Node::String("Sir Lancelot".into()));
    assert_eq!(g("Player.gold"), Node::I64(1250));
    assert_eq!(g("Player.hp"), Node::F64(87.5));
    assert_eq!(g("Player.speed"), Node::F64(150.0));
    assert_eq!(g("Player.hardcore"), Node::Bool(false));
    assert_eq!(g("Inventory.slot"), Node::String("sword".into()));
    assert_eq!(g("Inventory[\"slot#2\"]"), Node::String("shield".into()));
    assert_eq!(g("Inventory[\"slot#3\"]"), Node::String("potion".into()));
    assert_eq!(g("Flags.met_merlin"), Node::Bool(true));
    assert_eq!(g("Flags.chapter"), Node::String("007".into()));

    // Also detected without an extension hint.
    let s = pipeline.open(&data, &DetectContext::default()).unwrap();
    assert_eq!(s.codec_id(), "ini");
}

#[test]
fn value_edits_keep_layout() {
    let pipeline = EnvelopePipeline::builtin();
    let data = save();
    let mut s = pipeline.open(&data, &ctx()).unwrap();
    s.set(&p("Player.gold"), Node::I64(999_999)).unwrap();
    s.set(&p("Player.hp"), Node::F64(100.0)).unwrap();
    s.set(&p("Player.hardcore"), Node::Bool(true)).unwrap();
    s.set(&p("Player.name"), Node::String("Sir Galahad; the Pure # 2".into())).unwrap();
    s.set(&p("Inventory[\"slot#3\"]"), Node::String("elixir".into())).unwrap();
    s.set(&p("version"), Node::I32(4)).unwrap();
    let out = s.encode_verified(&pipeline, &ctx()).unwrap();

    let expected = String::from_utf8(save()).unwrap()
        .replace("gold = 1250", "gold = 999999")
        .replace("hp=87.50", "hp=100.00")
        .replace("hardcore = False", "hardcore = True")
        .replace("\"Sir Lancelot\"", "\"Sir Galahad; the Pure # 2\"")
        .replace("slot = potion ; x3", "slot = elixir ; x3")
        .replace("version = 3", "version = 4");
    assert_eq!(String::from_utf8(out.clone()).unwrap(), expected);

    let re = pipeline.open(&out, &ctx()).unwrap();
    assert_eq!(re.codec_id(), "ini");
    assert_eq!(re.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert!(savelens_core::bit_equal(&re.root(), &s.root()));
    assert_eq!(re.root().get(&p("Player.name")), Some(&Node::String("Sir Galahad; the Pure # 2".into())));
    // Untouched parts are byte-identical: comments, CRLF, the opaque line, the missing final newline.
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("\u{feff}; Autosave written by the game - do not edit\r\n"));
    assert!(text.contains("# inventory slots (duplicates are intentional)\n[Inventory]\r\n"));
    assert!(text.contains("!!garbage line kept verbatim\r\n"));
    assert!(text.contains("[ Flags ]   ; story flags\r\n\tmet_merlin\t=\ttrue\r\n"));
    assert!(text.ends_with("chapter=007"));
}

#[test]
fn unrepresentable_value_is_rejected_without_damage() {
    let pipeline = EnvelopePipeline::builtin();
    let data = save();
    let mut s = pipeline.open(&data, &ctx()).unwrap();
    // An unquoted string that would read back as a comment / a number.
    assert!(s.set(&p("Inventory.slot"), Node::String("axe ; sharp".into())).is_err());
    assert!(s.set(&p("Inventory.slot"), Node::String("12".into())).is_err());
    assert!(s.set(&p("Player.gold"), Node::String("line\nbreak".into())).is_err());
    assert_eq!(s.encode().unwrap(), data);
}

#[test]
fn structural_add_and_remove() {
    let pipeline = EnvelopePipeline::builtin();
    let data = save();
    let mut s = pipeline.open(&data, &ctx()).unwrap();
    assert!(s.capabilities().edit_structure);
    let mut root = s.root();
    root.set(&p("Player.mana"), Node::I64(40), true).unwrap();
    root.set(&p("difficulty"), Node::String("Nightmare".into()), true).unwrap();
    root.remove(&p("Player.speed")).unwrap();
    root.set(
        &p("Achievements"),
        Node::map_from([("first_blood", Node::Bool(true)), ("title", Node::String("Dragon Slayer".into()))]),
        true,
    )
    .unwrap();
    s.apply_ast(&root).unwrap();
    let out = s.encode_verified(&pipeline, &ctx()).unwrap();
    let text = String::from_utf8(out.clone()).unwrap();

    assert!(text.contains("build=\"1.0.7\"\r\ndifficulty=\"Nightmare\"\r\n\r\n[Player]"), "{text}");
    assert!(text.contains("hardcore = False\r\nmana = 40\r\n"), "{text}");
    assert!(!text.contains("speed"));
    assert!(text.ends_with("chapter=007\r\n\r\n[Achievements]\r\nfirst_blood=true\r\ntitle=Dragon Slayer"), "{text}");

    let re = pipeline.open(&out, &ctx()).unwrap();
    assert_eq!(re.codec_id(), "ini");
    assert_eq!(re.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert_eq!(re.root().get(&p("Achievements.title")), Some(&Node::String("Dragon Slayer".into())));
    assert_eq!(re.root().get(&p("Player.mana")), Some(&Node::I64(40)));

    // Remove a whole section.
    let mut s = re;
    let mut root = s.root();
    root.remove(&p("Inventory")).unwrap();
    s.apply_ast(&root).unwrap();
    let out = s.encode_verified(&pipeline, &ctx()).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(!text.contains("[Inventory]"));
    assert!(!text.contains("garbage line"));
    assert!(text.contains("\r\n# inventory slots (duplicates are intentional)\n[ Flags ]"), "{text:?}");
}

#[test]
fn base64_wrapped_ini_is_detected_through_the_layer() {
    let pipeline = EnvelopePipeline::builtin();
    let inner = b"[Settings]\nmusic_volume=0.75\nfullscreen=true\nplayer=\"Ann\"\n[Progress]\nlevel=12\n".to_vec();
    let wrapped = base64::engine::general_purpose::STANDARD.encode(&inner).into_bytes();
    let ctx = DetectContext::for_file("settings.dat");
    let mut s = pipeline.open(&wrapped, &ctx).unwrap();
    assert_eq!(s.codec_id(), "ini");
    assert_eq!(s.layers().len(), 1);
    assert!(s.layers()[0].to_string().to_lowercase().contains("base64"), "{:?}", s.layers());
    assert_eq!(s.payload(), inner.as_slice());
    assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert_eq!(s.encode().unwrap(), wrapped);

    s.set(&p("Progress.level"), Node::I64(13)).unwrap();
    let out = s.encode_verified(&pipeline, &ctx).unwrap();
    let decoded = base64::engine::general_purpose::STANDARD.decode(&out).unwrap();
    assert_eq!(decoded, String::from_utf8(inner).unwrap().replace("level=12", "level=13").into_bytes());
}

#[test]
fn json_and_xml_are_not_ini() {
    let pipeline = EnvelopePipeline::builtin();
    let s = pipeline.open(br#"{"a": "b=c", "d": [1, 2]}"#, &DetectContext::for_file("x.cfg")).unwrap();
    assert_eq!(s.codec_id(), "json");
    let s = pipeline.open(b"<config><a>b=c</a></config>", &DetectContext::for_file("x.cfg")).unwrap();
    assert_eq!(s.codec_id(), "xml");
}
