//! Generic XML integration tests through the full envelope pipeline.

use savelens_codecs::{CodecError, DetectContext, EnvelopePipeline};
use savelens_core::{Node, NodePath, RoundTripStatus};

fn p(s: &str) -> NodePath {
    NodePath::parse(s).unwrap()
}

const SAVE: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\r\n",
    "<!-- RPG save slot 2 -->\r\n",
    "<!DOCTYPE SaveGame [\r\n  <!ENTITY studio \"Indie & Co\">\r\n]>\r\n",
    "<SaveGame version=\"12\" checksumMode='none'>\r\n",
    "  <Player id=\"p1\" class='Mage &amp; Healer'>\r\n",
    "    <Name>Elrond &lt;the wise&gt;</Name>\r\n",
    "    <Level>42</Level>\r\n",
    "    <Gold>15000</Gold>\r\n",
    "    <Ratio>0.750</Ratio>\r\n",
    "    <Hardcore>False</Hardcore>\r\n",
    "    <Bio><![CDATA[Born in <Rivendell> & raised by elves]]></Bio>\r\n",
    "    <Empty/>\r\n",
    "    <Inventory>\r\n",
    "      <Item slot=\"0\" qty=\"1\">Sword</Item>\r\n",
    "      <Item slot=\"1\" qty=\"5\">Potion</Item>\r\n",
    "      <Item slot=\"2\" qty=\"1\"><Enchant>Fire</Enchant><Enchant>Ice</Enchant></Item>\r\n",
    "    </Inventory>\r\n",
    "  </Player>\r\n",
    "  <!-- quest log -->\r\n",
    "  <Quests>\r\n",
    "    <Quest>Find the ring</Quest>\r\n",
    "    <Quest>Destroy the ring</Quest>\r\n",
    "  </Quests>\r\n",
    "  <Note>Made by &studio; &#169; 2024 &#x2764;</Note>\r\n",
    "  <?game-hint skip?>\r\n",
    "</SaveGame>\r\n",
);

fn ctx() -> DetectContext {
    DetectContext::for_file("slot2.xml")
}

#[test]
fn detects_and_round_trips() {
    let pipeline = EnvelopePipeline::builtin();
    let s = pipeline.open(SAVE.as_bytes(), &ctx()).unwrap();
    assert_eq!(s.codec_id(), "xml");
    assert!(s.layers().is_empty());
    assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert_eq!(s.encode().unwrap(), SAVE.as_bytes());

    let root = s.root();
    let g = |path: &str| root.get(&p(path)).cloned().unwrap_or_else(|| panic!("missing {path}"));
    assert_eq!(g("[\"@version\"]"), Node::String("12".into()));
    assert_eq!(g("[\"@checksumMode\"]"), Node::String("none".into()));
    assert_eq!(g("Player[\"@class\"]"), Node::String("Mage & Healer".into()));
    assert_eq!(g("Player.Name"), Node::String("Elrond <the wise>".into()));
    assert_eq!(g("Player.Level"), Node::I64(42));
    assert_eq!(g("Player.Ratio"), Node::F64(0.75));
    assert_eq!(g("Player.Hardcore"), Node::Bool(false));
    assert_eq!(g("Player.Bio"), Node::String("Born in <Rivendell> & raised by elves".into()));
    assert_eq!(g("Player.Empty"), Node::String(String::new()));
    assert_eq!(g("Player.Inventory.Item[\"#text\"]"), Node::String("Sword".into()));
    assert_eq!(g("Player.Inventory[\"Item#2\"][\"@qty\"]"), Node::String("5".into()));
    assert_eq!(g("Player.Inventory[\"Item#3\"][\"Enchant#2\"]"), Node::String("Ice".into()));
    assert_eq!(g("Quests[\"Quest#2\"]"), Node::String("Destroy the ring".into()));
    assert_eq!(g("Note"), Node::String("Made by &studio; \u{a9} 2024 \u{2764}".into()));
    assert!(s.envelope().notes.iter().any(|n| n.contains("<SaveGame>")), "{:?}", s.envelope().notes);
}

#[test]
fn value_edits_escape_and_preserve_everything_else() {
    let pipeline = EnvelopePipeline::builtin();
    let mut s = pipeline.open(SAVE.as_bytes(), &ctx()).unwrap();
    s.set(&p("Player.Gold"), Node::I64(999_999)).unwrap();
    s.set(&p("Player.Level"), Node::I32(43)).unwrap();
    s.set(&p("Player.Ratio"), Node::F64(1.5)).unwrap();
    s.set(&p("Player.Hardcore"), Node::Bool(true)).unwrap();
    s.set(&p("Player.Name"), Node::String("Tom & \"Jerry\" <3".into())).unwrap();
    s.set(&p("Player[\"@class\"]"), Node::String("Rogue's \"Edge\" & <Co>".into())).unwrap();
    s.set(&p("[\"@checksumMode\"]"), Node::String("it's".into())).unwrap();
    s.set(&p("Player.Bio"), Node::String("Contains ]]> terminator & <tags>".into())).unwrap();
    s.set(&p("Player.Empty"), Node::String("now filled".into())).unwrap();
    s.set(&p("Player.Inventory[\"Item#2\"][\"#text\"]"), Node::String("Elixir".into())).unwrap();
    s.set(&p("Player.Inventory[\"Item#2\"][\"@qty\"]"), Node::String("99".into())).unwrap();
    s.set(&p("Quests[\"Quest#2\"]"), Node::String("Keep the ring".into())).unwrap();
    let out = s.encode_verified(&pipeline, &ctx()).unwrap();
    let text = String::from_utf8(out.clone()).unwrap();

    let expected = SAVE
        .replace("<Gold>15000</Gold>", "<Gold>999999</Gold>")
        .replace("<Level>42</Level>", "<Level>43</Level>")
        .replace("<Ratio>0.750</Ratio>", "<Ratio>1.500</Ratio>")
        .replace("<Hardcore>False</Hardcore>", "<Hardcore>True</Hardcore>")
        .replace("Elrond &lt;the wise&gt;", "Tom &amp; \"Jerry\" &lt;3")
        .replace("class='Mage &amp; Healer'", "class='Rogue&apos;s \"Edge\" &amp; &lt;Co&gt;'")
        .replace("checksumMode='none'", "checksumMode='it&apos;s'")
        .replace(
            "<![CDATA[Born in <Rivendell> & raised by elves]]>",
            "<![CDATA[Contains ]]]]><![CDATA[> terminator & <tags>]]>",
        )
        .replace("<Empty/>", "<Empty>now filled</Empty>")
        .replace(">Potion<", ">Elixir<")
        .replace("qty=\"5\"", "qty=\"99\"")
        .replace("Destroy the ring", "Keep the ring");
    assert_eq!(text, expected);

    let re = pipeline.open(&out, &ctx()).unwrap();
    assert_eq!(re.codec_id(), "xml");
    assert_eq!(re.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert!(savelens_core::bit_equal(&re.root(), &s.root()));
    assert_eq!(re.root().get(&p("Player.Bio")), Some(&Node::String("Contains ]]> terminator & <tags>".into())));
    assert_eq!(re.root().get(&p("Player[\"@class\"]")), Some(&Node::String("Rogue's \"Edge\" & <Co>".into())));
    // Untouched parts are byte-identical.
    assert!(text.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\r\n<!-- RPG save slot 2 -->\r\n<!DOCTYPE SaveGame [\r\n"));
    assert!(text.contains("  <!-- quest log -->\r\n"));
    assert!(text.contains("<Note>Made by &studio; &#169; 2024 &#x2764;</Note>\r\n  <?game-hint skip?>\r\n</SaveGame>\r\n"));
}

#[test]
fn structural_edits_are_rejected() {
    let pipeline = EnvelopePipeline::builtin();
    let mut s = pipeline.open(SAVE.as_bytes(), &ctx()).unwrap();
    let err = s.set(&p("Player.Inventory"), Node::String("flattened".into())).unwrap_err();
    assert!(matches!(err, CodecError::StructureUnsupported { .. }), "{err}");
    let mut root = s.root();
    root.set(&p("Player.Mana"), Node::I64(10), true).unwrap();
    assert!(matches!(s.apply_ast(&root), Err(CodecError::StructureUnsupported { .. })));
    let mut root = s.root();
    root.remove(&p("Quests")).unwrap();
    assert!(s.apply_ast(&root).is_err());
    // A string that would be re-read as a number is refused.
    assert!(s.set(&p("Player.Name"), Node::String("123".into())).is_err());
    assert_eq!(s.encode().unwrap(), SAVE.as_bytes());
}

#[test]
fn bom_and_minimal_documents() {
    let pipeline = EnvelopePipeline::builtin();
    let mut data = vec![0xef, 0xbb, 0xbf];
    data.extend_from_slice(b"<settings vsync=\"1\"><fov>90</fov></settings>\n\n");
    let mut s = pipeline.open(&data, &DetectContext::default()).unwrap();
    assert_eq!(s.codec_id(), "xml");
    assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
    s.set(&p("fov"), Node::I64(110)).unwrap();
    let out = s.encode_verified(&pipeline, &DetectContext::default()).unwrap();
    let mut expected = vec![0xef, 0xbb, 0xbf];
    expected.extend_from_slice(b"<settings vsync=\"1\"><fov>110</fov></settings>\n\n");
    assert_eq!(out, expected);
}

#[test]
fn malformed_xml_is_not_detected_and_does_not_panic() {
    let pipeline = EnvelopePipeline::builtin();
    for bad in [
        "<?xml version=\"1.0\"?>\n<Save><Player><Gold>10</Gold></Save>\n",
        "<Save><Player>",
        "<Save></Save><Other/>",
        "<Save a=\"1\" a=\"2\"/>",
        "<Save>&unterminated</Save>",
        "<Save><![CDATA[open</Save>",
        "<",
        "<Save",
    ] {
        let s = pipeline.open(bad.as_bytes(), &DetectContext::for_file("x.xml")).unwrap();
        assert_ne!(s.codec_id(), "xml", "{bad}");
        assert_ne!(s.codec_id(), "plist", "{bad}");
        assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
        assert_eq!(s.encode().unwrap(), bad.as_bytes());
        assert!(pipeline.open_with_codec(bad.as_bytes(), &DetectContext::default(), "xml").is_err());
    }
}
