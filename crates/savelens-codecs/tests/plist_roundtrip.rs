//! XML property-list integration tests through the full envelope pipeline.

use base64::Engine;
use savelens_codecs::{CodecError, DetectContext, EnvelopePipeline};
use savelens_core::{Node, NodePath, RoundTripStatus};

fn p(s: &str) -> NodePath {
    NodePath::parse(s).unwrap()
}

const PLIST: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
    "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
    "<plist version=\"1.0\">\n",
    "<dict>\n",
    "\t<key>SaveVersion</key>\n",
    "\t<integer>7</integer>\n",
    "\t<key>LastPlayed</key>\n",
    "\t<date>2024-05-01T18:30:00Z</date>\n",
    "\t<!-- player block -->\n",
    "\t<key>Player</key>\n",
    "\t<dict>\n",
    "\t\t<key>Name</key>\n",
    "\t\t<string>Link &amp; Zelda</string>\n",
    "\t\t<key>Rupees</key>\n",
    "\t\t<integer>-25</integer>\n",
    "\t\t<key>Health</key>\n",
    "\t\t<real>3.250</real>\n",
    "\t\t<key>MasterMode</key>\n",
    "\t\t<false/>\n",
    "\t\t<key>HasMap</key>\n",
    "\t\t<true/>\n",
    "\t\t<key>Position</key>\n",
    "\t\t<array>\n",
    "\t\t\t<real>10.5</real>\n",
    "\t\t\t<real>-2</real>\n",
    "\t\t\t<real>1e3</real>\n",
    "\t\t</array>\n",
    "\t</dict>\n",
    "\t<key>Slots</key>\n",
    "\t<array>\n",
    "\t\t<dict>\n",
    "\t\t\t<key>Item</key>\n",
    "\t\t\t<string>Bow</string>\n",
    "\t\t</dict>\n",
    "\t\t<dict>\n",
    "\t\t\t<key>Item</key>\n",
    "\t\t\t<string/>\n",
    "\t\t</dict>\n",
    "\t</array>\n",
    "\t<key>Thumbnail</key>\n",
    "\t<data>\n",
    "\tAAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygp\n",
    "\tKissLS4vMDEyMzQ1Njc4OTo7PD0+Pw==\n",
    "\t</data>\n",
    "\t<key>Key</key>\n",
    "\t<data>3q2+7w==</data>\n",
    "</dict>\n",
    "</plist>\n",
);

fn ctx() -> DetectContext {
    DetectContext::for_file("GameSave.plist")
}

#[test]
fn detects_and_round_trips() {
    let pipeline = EnvelopePipeline::builtin();
    let s = pipeline.open(PLIST.as_bytes(), &ctx()).unwrap();
    assert_eq!(s.codec_id(), "plist");
    assert!(s.layers().is_empty());
    assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert_eq!(s.encode().unwrap(), PLIST.as_bytes());

    let root = s.root();
    let g = |path: &str| root.get(&p(path)).cloned().unwrap_or_else(|| panic!("missing {path}"));
    assert_eq!(g("SaveVersion"), Node::I64(7));
    assert_eq!(g("LastPlayed"), Node::String("2024-05-01T18:30:00Z".into()));
    assert_eq!(g("Player.Name"), Node::String("Link & Zelda".into()));
    assert_eq!(g("Player.Rupees"), Node::I64(-25));
    assert_eq!(g("Player.Health"), Node::F64(3.25));
    assert_eq!(g("Player.MasterMode"), Node::Bool(false));
    assert_eq!(g("Player.HasMap"), Node::Bool(true));
    assert_eq!(g("Player.Position[2]"), Node::F64(1000.0));
    assert_eq!(g("Slots[0].Item"), Node::String("Bow".into()));
    assert_eq!(g("Slots[1].Item"), Node::String(String::new()));
    assert_eq!(g("Thumbnail"), Node::Bytes((0u8..64).collect()));
    assert_eq!(g("Key"), Node::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));

    // Without the extension hint it is still a plist, not generic XML.
    let s = pipeline.open(PLIST.as_bytes(), &DetectContext::default()).unwrap();
    assert_eq!(s.codec_id(), "plist");
}

#[test]
fn value_edits_preserve_layout() {
    let pipeline = EnvelopePipeline::builtin();
    let mut s = pipeline.open(PLIST.as_bytes(), &ctx()).unwrap();
    s.set(&p("SaveVersion"), Node::I32(8)).unwrap();
    s.set(&p("Player.Name"), Node::String("<Ganon> & \"friends\"".into())).unwrap();
    s.set(&p("Player.Rupees"), Node::I64(9999)).unwrap();
    s.set(&p("Player.Health"), Node::F64(20.0)).unwrap();
    s.set(&p("Player.MasterMode"), Node::Bool(true)).unwrap();
    s.set(&p("Player.HasMap"), Node::Bool(false)).unwrap();
    s.set(&p("Player.Position[2]"), Node::F64(2500.0)).unwrap();
    s.set(&p("Slots[1].Item"), Node::String("Shield".into())).unwrap();
    s.set(&p("LastPlayed"), Node::String("2025-01-02T03:04:05Z".into())).unwrap();
    let thumb: Vec<u8> = (0u8..80).rev().collect();
    s.set(&p("Thumbnail"), Node::Bytes(thumb.clone())).unwrap();
    s.set(&p("Key"), Node::Bytes(vec![1, 2, 3])).unwrap();
    let out = s.encode_verified(&pipeline, &ctx()).unwrap();
    let text = String::from_utf8(out.clone()).unwrap();

    let b64 = base64::engine::general_purpose::STANDARD.encode(&thumb);
    let wrapped: Vec<&str> = b64.as_bytes().chunks(56).map(|c| std::str::from_utf8(c).unwrap()).collect();
    let expected = PLIST
        .replace("<integer>7</integer>", "<integer>8</integer>")
        .replace("Link &amp; Zelda", "&lt;Ganon&gt; &amp; \"friends\"")
        .replace("<integer>-25</integer>", "<integer>9999</integer>")
        .replace("<real>3.250</real>", "<real>20.000</real>")
        .replace("<key>MasterMode</key>\n\t\t<false/>", "<key>MasterMode</key>\n\t\t<true/>")
        .replace("<key>HasMap</key>\n\t\t<true/>", "<key>HasMap</key>\n\t\t<false/>")
        .replace("<real>1e3</real>", "<real>2.5e3</real>")
        .replace("<string/>", "<string>Shield</string>")
        .replace("2024-05-01T18:30:00Z", "2025-01-02T03:04:05Z")
        .replace(
            "\tAAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygp\n\tKissLS4vMDEyMzQ1Njc4OTo7PD0+Pw==\n",
            &format!("\t{}\n", wrapped.join("\n\t")),
        )
        .replace("<data>3q2+7w==</data>", "<data>AQID</data>");
    assert_eq!(text, expected);
    // Comments and the DOCTYPE are untouched.
    assert!(text.contains("\t<!-- player block -->\n"));
    assert!(text.contains("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\""));

    let re = pipeline.open(&out, &ctx()).unwrap();
    assert_eq!(re.codec_id(), "plist");
    assert_eq!(re.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert!(savelens_core::bit_equal(&re.root(), &s.root()));
    assert_eq!(re.root().get(&p("Thumbnail")), Some(&Node::Bytes(thumb)));
    assert_eq!(re.root().get(&p("Player.Name")), Some(&Node::String("<Ganon> & \"friends\"".into())));
}

#[test]
fn structural_changes_rejected() {
    let pipeline = EnvelopePipeline::builtin();
    let mut s = pipeline.open(PLIST.as_bytes(), &ctx()).unwrap();
    let mut root = s.root();
    root.set(&p("Player.Stamina"), Node::I64(3), true).unwrap();
    assert!(matches!(s.apply_ast(&root), Err(CodecError::StructureUnsupported { .. })));
    let mut root = s.root();
    if let Some(Node::Array(a)) = root.get_mut(&p("Player.Position")) {
        a.pop();
    }
    assert!(s.apply_ast(&root).is_err());
    assert!(s.set(&p("Player"), Node::I64(1)).is_err());
    assert!(s.set(&p("Player.Rupees"), Node::String("lots".into())).is_err());
    assert_eq!(s.encode().unwrap(), PLIST.as_bytes());
}

#[test]
fn invalid_plists_fall_back() {
    let pipeline = EnvelopePipeline::builtin();
    // Well-formed XML but not a valid plist (dict key without value): generic XML wins.
    let odd = "<plist version=\"1.0\"><dict><key>a</key></dict></plist>";
    let s = pipeline.open(odd.as_bytes(), &DetectContext::default()).unwrap();
    assert_eq!(s.codec_id(), "xml");
    assert_eq!(s.encode().unwrap(), odd.as_bytes());
    // Truncated plist: nothing structured accepts it.
    let cut = &PLIST[..PLIST.len() / 2];
    let s = pipeline.open(cut.as_bytes(), &DetectContext::default()).unwrap();
    assert_ne!(s.codec_id(), "plist");
    assert_ne!(s.codec_id(), "xml");
    assert_eq!(s.encode().unwrap(), cut.as_bytes());
}
