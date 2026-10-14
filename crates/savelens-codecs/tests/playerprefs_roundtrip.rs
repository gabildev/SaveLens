//! End-to-end tests of the Unity PlayerPrefs codec through the pipeline.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use savelens_codecs::{DetectContext, EnvelopePipeline};
use savelens_core::{Node, NodePath, RoundTripStatus};

fn prefs(nl: &str) -> String {
    let lines = [
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>".to_string(),
        "<unity_prefs version_major=\"1\" version_minor=\"1\">".to_string(),
        "\t<pref name=\"Screenmanager Resolution Width\" type=\"int\">1920</pref>".to_string(),
        "\t<pref name=\"Coins\" type=\"int\">250</pref>".to_string(),
        "\t<pref name=\"MasterVolume\" type=\"float\">0.800000011920929</pref>".to_string(),
        "\t<pref type=\"float\" name='Sensitivity'>1.5</pref>".to_string(),
        format!("\t<pref name=\"PlayerName\" type=\"string\">{}</pref>", STANDARD.encode("J\u{f6}rmungandr")),
        "\t<!-- written by Unity -->".to_string(),
        "\t<pref name=\"Coins\" type=\"int\">7</pref>".to_string(),
        "\t<pref name=\"unity.player_session_count\" type=\"string\">MTI=</pref>".to_string(),
        "</unity_prefs>".to_string(),
        String::new(),
    ];
    lines.join(nl)
}

fn key(k: &str) -> NodePath {
    NodePath::root().key(k)
}

fn check(nl: &str) {
    let src = prefs(nl);
    let pipeline = EnvelopePipeline::builtin();
    let ctx = DetectContext::for_file("prefs");
    let mut opened = pipeline.open(src.as_bytes(), &ctx).unwrap();
    assert_eq!(opened.codec_id(), "playerprefs");
    assert!(opened.layers().is_empty());
    assert_eq!(opened.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert_eq!(opened.encode().unwrap(), src.as_bytes());

    let root = opened.root();
    assert_eq!(root.get(&key("Screenmanager Resolution Width")), Some(&Node::I32(1920)));
    assert_eq!(root.get(&key("Coins")), Some(&Node::I32(250)));
    assert_eq!(root.get(&key("Coins#2")), Some(&Node::I32(7)));
    assert_eq!(root.get(&key("MasterVolume")), Some(&Node::F32(0.8)));
    assert_eq!(root.get(&key("Sensitivity")), Some(&Node::F32(1.5)));
    assert_eq!(root.get(&key("PlayerName")), Some(&Node::String("J\u{f6}rmungandr".into())));
    assert_eq!(root.get(&key("unity.player_session_count")), Some(&Node::String("12".into())));

    opened.set(&key("Coins#2"), Node::I64(99_999)).unwrap();
    opened.set(&key("Sensitivity"), Node::F64(2.25)).unwrap();
    opened.set(&key("PlayerName"), Node::String("Ni\u{f1}o & <Co>".into())).unwrap();
    let out = opened.encode_verified(&pipeline, &ctx).unwrap();
    let text = String::from_utf8(out.clone()).unwrap();

    // Only the edited values changed: same lines, same line endings.
    let expected = src
        .replace(">7</pref>", ">99999</pref>")
        .replace("name='Sensitivity'>1.5</pref>", "name='Sensitivity'>2.25</pref>")
        .replace(&STANDARD.encode("J\u{f6}rmungandr"), &STANDARD.encode("Ni\u{f1}o & <Co>"));
    assert_eq!(text, expected);
    // The untouched float keeps its original long spelling.
    assert!(text.contains(">0.800000011920929</pref>"));
    assert_eq!(text.matches(nl).count(), src.matches(nl).count());

    let reopened = pipeline.open(&out, &ctx).unwrap();
    assert_eq!(reopened.codec_id(), "playerprefs");
    let r = reopened.root();
    assert_eq!(r.get(&key("Coins")), Some(&Node::I32(250)));
    assert_eq!(r.get(&key("Coins#2")), Some(&Node::I32(99_999)));
    assert_eq!(r.get(&key("Sensitivity")), Some(&Node::F32(2.25)));
    assert_eq!(r.get(&key("PlayerName")), Some(&Node::String("Ni\u{f1}o & <Co>".into())));
    assert_eq!(r.get(&key("MasterVolume")), Some(&Node::F32(0.8)));
}

#[test]
fn lf_roundtrip_and_edits() {
    check("\n");
}

#[test]
fn crlf_roundtrip_and_edits() {
    check("\r\n");
}

#[test]
fn edited_float_uses_shortest_text() {
    let src = prefs("\n");
    let pipeline = EnvelopePipeline::builtin();
    let ctx = DetectContext::default();
    let mut opened = pipeline.open(src.as_bytes(), &ctx).unwrap();
    opened.set(&key("MasterVolume"), Node::F32(0.1)).unwrap();
    let text = String::from_utf8(opened.encode_verified(&pipeline, &ctx).unwrap()).unwrap();
    assert!(text.contains("type=\"float\">0.1</pref>"));
}

#[test]
fn rejects_wrong_types_and_new_keys() {
    let src = prefs("\n");
    let pipeline = EnvelopePipeline::builtin();
    let ctx = DetectContext::default();
    let mut opened = pipeline.open(src.as_bytes(), &ctx).unwrap();
    assert!(opened.set(&key("Coins"), Node::String("many".into())).is_err());
    assert!(opened.set(&key("Coins"), Node::I64(i64::MAX)).is_err());
    let mut root = opened.root();
    root.set(&key("NewKey"), Node::I32(1), true).unwrap();
    assert!(opened.apply_ast(&root).is_err());
    assert_eq!(opened.encode().unwrap(), src.as_bytes());
}

#[test]
fn not_prefs_is_not_detected() {
    let pipeline = EnvelopePipeline::builtin();
    let ctx = DetectContext::default();
    let codec = pipeline.registry().get("playerprefs").unwrap();
    assert_eq!(codec.detect(b"<plist><dict/></plist>", &ctx), 0);
    let src = prefs("\n");
    for n in 0..src.len() {
        let cut = &src.as_bytes()[..n];
        let opened = pipeline.open(cut, &ctx).unwrap();
        assert_eq!(opened.encode().unwrap(), cut);
    }
}
