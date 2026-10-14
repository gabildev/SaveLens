//! GVAS (Unreal Engine `.sav`) integration tests.
//!
//! Fixtures are produced by an independent test-side byte builder (it does
//! not use the codec's serializer), modelled on what `UGameplayStatics::
//! SaveGameToMemory` emits for UE4 (SaveGameFileVersion 2) and UE5
//! (SaveGameFileVersion 3) save games.

use savelens_codecs::layers::compression::deflate;
use savelens_codecs::{DetectContext, EnvelopePipeline};
use savelens_core::{bit_equal, Node, NodePath, RoundTripStatus};

// ---------------------------------------------------------------------------
// Independent GVAS builder
// ---------------------------------------------------------------------------

#[derive(Default)]
struct B(Vec<u8>);

impl B {
    fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.0.extend_from_slice(v);
        self
    }
    /// ASCII FString (length includes the NUL; empty string = length 0).
    fn s(&mut self, text: &str) -> &mut Self {
        assert!(text.is_ascii());
        if text.is_empty() {
            return self.i32(0);
        }
        self.i32(text.len() as i32 + 1);
        self.bytes(text.as_bytes()).u8(0)
    }
    /// UTF-16 FString (negative length).
    fn w(&mut self, text: &str) -> &mut Self {
        let units: Vec<u16> = text.encode_utf16().collect();
        self.i32(-(units.len() as i32 + 1));
        for u in units {
            self.u16(u);
        }
        self.u16(0)
    }
}

fn fstr(text: &str) -> Vec<u8> {
    let mut b = B::default();
    b.s(text);
    b.0
}

fn guid(seed: u8) -> [u8; 16] {
    std::array::from_fn(|i| seed.wrapping_mul(31).wrapping_add(i as u8))
}

/// Classic property tag + value. `extra` is the type-specific header data
/// that precedes the HasPropertyGuid byte.
fn tag(out: &mut B, name: &str, ty: &str, extra: &[u8], value: &[u8]) {
    out.s(name).s(ty).i32(value.len() as i32).i32(0).bytes(extra).u8(0).bytes(value);
}

fn int_prop(out: &mut B, name: &str, v: i32) {
    tag(out, name, "IntProperty", &[], &v.to_le_bytes());
}

fn str_prop(out: &mut B, name: &str, v: &str) {
    tag(out, name, "StrProperty", &[], &fstr(v));
}

fn bool_prop(out: &mut B, name: &str, v: bool) {
    out.s(name).s("BoolProperty").i32(0).i32(0).u8(v as u8).u8(0);
}

fn struct_prop(out: &mut B, name: &str, struct_name: &str, value: &[u8]) {
    let mut extra = fstr(struct_name);
    extra.extend_from_slice(&[0; 16]);
    tag(out, name, "StructProperty", &extra, value);
}

fn array_prop(out: &mut B, name: &str, inner: &str, value: &[u8]) {
    tag(out, name, "ArrayProperty", &fstr(inner), value);
}

fn none(out: &mut B) {
    out.s("None");
}

fn header(out: &mut B, ue5: Option<i32>) {
    out.bytes(b"GVAS");
    match ue5 {
        Some(v) => {
            out.i32(3).i32(522).i32(v);
            out.u16(5).u16(3).u16(2).u32(29_314_046).s("++UE5+Release-5.3");
        }
        None => {
            out.i32(2).i32(522);
            out.u16(4).u16(27).u16(2).u32(18_319_896).s("++UE4+Release-4.27");
        }
    }
    out.i32(3).i32(3);
    for (seed, ver) in [(1u8, 10i32), (2, 37), (3, 4)] {
        out.bytes(&guid(seed)).i32(ver);
    }
    out.s("/Script/MyGame.MySaveGame");
}

/// An inventory item struct (nested property list).
fn item(name: &str, count: i32) -> Vec<u8> {
    let mut b = B::default();
    tag(&mut b, "ItemId", "NameProperty", &[], &fstr(name));
    int_prop(&mut b, "Count", count);
    none(&mut b);
    b.0
}

fn build_save(ue5: Option<i32>) -> Vec<u8> {
    let double = ue5.is_some();
    let mut b = B::default();
    header(&mut b, ue5);

    int_prop(&mut b, "Level", 7);
    tag(&mut b, "Seed", "UInt32Property", &[], &4_000_000_000u32.to_le_bytes());
    tag(&mut b, "PlayTimeTicks", "Int64Property", &[], &(-1_234_567_890_123i64).to_le_bytes());
    tag(&mut b, "Health", "FloatProperty", &[], &87.5f32.to_le_bytes());
    tag(&mut b, "Precision", "DoubleProperty", &[], &0.1f64.to_le_bytes());
    str_prop(&mut b, "PlayerName", "Hero");
    let mut wide = B::default();
    wide.w("Jos\u{e9} \u{4e16}\u{754c}");
    tag(&mut b, "Motto", "StrProperty", &[], &wide.0);
    tag(&mut b, "Map", "NameProperty", &[], &fstr("/Game/Maps/Overworld"));
    bool_prop(&mut b, "bHardcore", true);
    bool_prop(&mut b, "bTutorialDone", false);
    tag(&mut b, "Lives", "ByteProperty", &fstr("None"), &[3]);
    tag(&mut b, "Difficulty", "ByteProperty", &fstr("EDifficulty"), &fstr("EDifficulty::Hard"));
    tag(&mut b, "Class", "EnumProperty", &fstr("EPlayerClass"), &fstr("EPlayerClass::Mage"));

    // Vector, float (UE4) or double (UE5 LWC), plus a float one in both.
    let mut v = B::default();
    if double {
        for x in [100.5f64, -20.25, 3.0] {
            v.bytes(&x.to_le_bytes());
        }
    } else {
        for x in [100.5f32, -20.25, 3.0] {
            v.bytes(&x.to_le_bytes());
        }
    }
    struct_prop(&mut b, "Location", "Vector", &v.0);
    let mut vf = B::default();
    for x in [1.0f32, 2.0] {
        vf.bytes(&x.to_le_bytes());
    }
    struct_prop(&mut b, "UV", "Vector2D", &vf.0);
    struct_prop(&mut b, "PlayerGuid", "Guid", &guid(9));
    struct_prop(&mut b, "SavedAt", "DateTime", &638_000_000_000_000_000i64.to_le_bytes());

    // Nested property struct.
    let mut stats = B::default();
    int_prop(&mut stats, "Strength", 12);
    tag(&mut stats, "Speed", "FloatProperty", &[], &1.25f32.to_le_bytes());
    let mut rot = B::default();
    for x in [0.0f32, 90.0, 0.0] {
        rot.bytes(&x.to_le_bytes());
    }
    struct_prop(&mut stats, "Facing", "Rotator", &rot.0);
    none(&mut stats);
    struct_prop(&mut b, "Stats", "PlayerStats", &stats.0);

    // Array<int>.
    let mut ints = B::default();
    ints.i32(4);
    for x in [10, 20, 30, 40] {
        ints.i32(x);
    }
    array_prop(&mut b, "Scores", "IntProperty", &ints.0);

    // Array<FString>.
    let mut strs = B::default();
    strs.i32(3).s("alpha").s("").s("gamma");
    array_prop(&mut b, "Tags", "StrProperty", &strs.0);

    // Array<struct ItemData> with inner tag.
    let items: Vec<Vec<u8>> = vec![item("Sword", 1), item("Potion", 5), item("Arrow", 64)];
    let body: Vec<u8> = items.concat();
    let mut arr = B::default();
    arr.i32(items.len() as i32)
        .s("Inventory")
        .s("StructProperty")
        .i32(body.len() as i32)
        .i32(0)
        .s("ItemData")
        .bytes(&guid(4))
        .u8(0)
        .bytes(&body);
    array_prop(&mut b, "Inventory", "StructProperty", &arr.0);

    // Array<struct Vector> (binary elements, inner tag).
    let mut vbody = B::default();
    for x in [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0] {
        vbody.bytes(&x.to_le_bytes());
    }
    let mut varr = B::default();
    varr.i32(2)
        .s("Waypoints")
        .s("StructProperty")
        .i32(vbody.0.len() as i32)
        .i32(0)
        .s("Vector")
        .bytes(&[0; 16])
        .u8(0)
        .bytes(&vbody.0);
    array_prop(&mut b, "Waypoints", "StructProperty", &varr.0);

    // Set<Name>.
    let mut set = B::default();
    set.i32(0).i32(2).s("QuestA").s("QuestB");
    tag(&mut b, "Completed", "SetProperty", &fstr("NameProperty"), &set.0);

    // Map<FString, int>.
    let mut map = B::default();
    map.i32(0).i32(2).s("wood").i32(12).s("stone").i32(3);
    let mut kv = fstr("StrProperty");
    kv.extend_from_slice(&fstr("IntProperty"));
    tag(&mut b, "Resources", "MapProperty", &kv, &map.0);

    // Map<Guid, struct> (struct values are property lists).
    let mut gmap = B::default();
    gmap.i32(0).i32(2);
    for (seed, name, lvl) in [(20u8, "Bob", 3), (21, "Alice", 9)] {
        gmap.bytes(&guid(seed));
        let mut npc = B::default();
        str_prop(&mut npc, "Name", name);
        int_prop(&mut npc, "Level", lvl);
        none(&mut npc);
        gmap.bytes(&npc.0);
    }
    let mut kv = fstr("StructProperty");
    kv.extend_from_slice(&fstr("StructProperty"));
    tag(&mut b, "Npcs", "MapProperty", &kv, &gmap.0);

    // Object / soft object.
    tag(&mut b, "Weapon", "ObjectProperty", &[], &fstr("/Game/Items/BP_Sword.BP_Sword_C"));
    let mut soft = B::default();
    soft.s("/Game/Textures/T_Icon.T_Icon").s("");
    tag(&mut b, "Icon", "SoftObjectProperty", &[], &soft.0);

    // TextProperty: kept raw.
    let mut text = B::default();
    text.u32(0).u8(0xff).i32(1).s("Hello");
    tag(&mut b, "Greeting", "TextProperty", &[], &text.0);

    none(&mut b);
    b.bytes(&[0, 0, 0, 0]);
    b.0
}

fn ctx() -> DetectContext {
    DetectContext::for_file("Slot.sav")
}

fn p(s: &str) -> NodePath {
    NodePath::parse(s).unwrap()
}

fn get(root: &Node, path: &str) -> Node {
    root.get(&p(path)).cloned().unwrap_or_else(|| panic!("missing {path}"))
}

fn assert_lossless(bytes: &[u8]) -> savelens_codecs::OpenedSave {
    let s = EnvelopePipeline::builtin().open(bytes, &ctx()).unwrap();
    assert_eq!(s.codec_id(), "gvas", "notes: {:?}", s.envelope().notes);
    assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert_eq!(s.encode().unwrap(), bytes);
    s
}

// ---------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------

#[test]
fn ue4_save_round_trips_bit_for_bit() {
    let bytes = build_save(None);
    let s = assert_lossless(&bytes);
    let r = s.root();
    assert_eq!(get(&r, "Level"), Node::I32(7));
    assert_eq!(get(&r, "Seed"), Node::U32(4_000_000_000));
    assert_eq!(get(&r, "PlayTimeTicks"), Node::I64(-1_234_567_890_123));
    assert_eq!(get(&r, "Health"), Node::F32(87.5));
    assert_eq!(get(&r, "Precision"), Node::F64(0.1));
    assert_eq!(get(&r, "PlayerName"), Node::String("Hero".into()));
    assert_eq!(get(&r, "Motto"), Node::String("Jos\u{e9} \u{4e16}\u{754c}".into()));
    assert_eq!(get(&r, "Map"), Node::String("/Game/Maps/Overworld".into()));
    assert_eq!(get(&r, "bHardcore"), Node::Bool(true));
    assert_eq!(get(&r, "bTutorialDone"), Node::Bool(false));
    assert_eq!(get(&r, "Lives"), Node::U8(3));
    assert_eq!(get(&r, "Difficulty"), Node::String("EDifficulty::Hard".into()));
    assert_eq!(get(&r, "Class"), Node::String("EPlayerClass::Mage".into()));
    assert_eq!(get(&r, "Location.X"), Node::F32(100.5));
    assert_eq!(get(&r, "Location.Y"), Node::F32(-20.25));
    assert_eq!(get(&r, "UV.Y"), Node::F32(2.0));
    assert!(matches!(get(&r, "PlayerGuid"), Node::String(g) if g.len() == 32));
    assert_eq!(get(&r, "SavedAt"), Node::I64(638_000_000_000_000_000));
    assert_eq!(get(&r, "Stats.Strength"), Node::I32(12));
    assert_eq!(get(&r, "Stats.Facing.Yaw"), Node::F32(90.0));
    assert_eq!(get(&r, "Scores[2]"), Node::I32(30));
    assert_eq!(get(&r, "Tags[1]"), Node::String(String::new()));
    assert_eq!(get(&r, "Inventory[1].ItemId"), Node::String("Potion".into()));
    assert_eq!(get(&r, "Inventory[2].Count"), Node::I32(64));
    assert_eq!(get(&r, "Waypoints[1].Z"), Node::F32(6.0));
    assert_eq!(get(&r, "Completed[1]"), Node::String("QuestB".into()));
    assert_eq!(get(&r, "Resources[1].key"), Node::String("stone".into()));
    assert_eq!(get(&r, "Resources[1].value"), Node::I32(3));
    assert!(matches!(get(&r, "Npcs[0].key"), Node::String(g) if g.len() == 32));
    assert_eq!(get(&r, "Npcs[1].value.Name"), Node::String("Alice".into()));
    assert_eq!(get(&r, "Weapon"), Node::String("/Game/Items/BP_Sword.BP_Sword_C".into()));
    assert_eq!(get(&r, "Icon.AssetPath"), Node::String("/Game/Textures/T_Icon.T_Icon".into()));
    assert!(matches!(get(&r, "Greeting"), Node::Bytes(b) if !b.is_empty()));
    let notes = &s.envelope().notes;
    assert!(notes.iter().any(|n| n.contains("UE 4.27.2 SaveGameVersion 2")), "{notes:?}");
    assert!(notes.iter().any(|n| n.contains("trailing")), "{notes:?}");
}

#[test]
fn ue5_save_round_trips_bit_for_bit() {
    let bytes = build_save(Some(1009));
    let s = assert_lossless(&bytes);
    let r = s.root();
    assert_eq!(get(&r, "Location.X"), Node::F64(100.5));
    assert_eq!(get(&r, "Location.Z"), Node::F64(3.0));
    assert_eq!(get(&r, "UV.X"), Node::F32(1.0), "size decides float vs double");
    assert_eq!(get(&r, "Inventory[0].ItemId"), Node::String("Sword".into()));
    assert!(s.envelope().notes.iter().any(|n| n.contains("UE 5.3.2 SaveGameVersion 3")));
    assert!(s.envelope().notes.iter().any(|n| n.contains("UE5 1009")));
}

// ---------------------------------------------------------------------------
// Edits
// ---------------------------------------------------------------------------

/// Applies `edits`, verifies, reopens and checks that only the edited paths changed.
fn edit_and_check(bytes: &[u8], edits: &[(&str, Node)]) -> Vec<u8> {
    let pipeline = EnvelopePipeline::builtin();
    let mut s = pipeline.open(bytes, &ctx()).unwrap();
    let before = s.root();
    for (path, value) in edits {
        s.set(&p(path), value.clone()).unwrap();
    }
    let out = s.encode_verified(&pipeline, &ctx()).unwrap();
    let re = assert_lossless(&out);
    let after = re.root();
    let mut expected = before.clone();
    for (path, value) in edits {
        expected.set(&p(path), value.clone(), false).unwrap();
    }
    assert!(bit_equal(&after, &expected), "only the edited values may change");
    out
}

#[test]
fn edit_int_float_string_and_struct_array_element() {
    for ue5 in [None, Some(1009)] {
        let bytes = build_save(ue5);
        let out = edit_and_check(
            &bytes,
            &[
                ("Level", Node::I32(99)),
                ("Health", Node::F32(250.75)),
                ("PlayerName", Node::String("Sir Lancelot the Brave".into())),
                ("Inventory[1].Count", Node::I32(999)),
            ],
        );
        // The longer string grows the file by exactly the extra characters:
        // the property Size was recomputed.
        assert_eq!(out.len(), bytes.len() + "Sir Lancelot the Brave".len() - "Hero".len());
        let needle = fstr("Sir Lancelot the Brave");
        let pos = out.windows(needle.len()).position(|w| w == needle.as_slice()).unwrap();
        let size = i32::from_le_bytes(out[pos - 9..pos - 5].try_into().unwrap());
        assert_eq!(size as usize, needle.len(), "StrProperty Size recomputed");
        // Untouched bytes before the first edit are identical.
        let level_tag = fstr("Level");
        let lpos = bytes.windows(level_tag.len()).position(|w| w == level_tag.as_slice()).unwrap();
        assert_eq!(out[..lpos], bytes[..lpos]);
    }
}

#[test]
fn numeric_edits_are_coerced_and_range_checked() {
    let bytes = build_save(None);
    let pipeline = EnvelopePipeline::builtin();
    let mut s = pipeline.open(&bytes, &ctx()).unwrap();
    s.set(&p("Level"), Node::I64(12)).unwrap();
    s.set(&p("Lives"), Node::I64(9)).unwrap();
    assert_eq!(get(&s.root(), "Level"), Node::I32(12));
    assert_eq!(get(&s.root(), "Lives"), Node::U8(9));
    assert!(s.set(&p("Lives"), Node::I64(300)).is_err());
    assert!(s.set(&p("Level"), Node::String("abc".into())).is_err());
    assert!(s.set(&p("Greeting"), Node::Bytes(vec![1, 2])).is_err(), "raw values keep their length");
}

#[test]
fn edit_bools_enums_structs_maps_and_sets() {
    let bytes = build_save(Some(1009));
    edit_and_check(
        &bytes,
        &[
            ("bHardcore", Node::Bool(false)),
            ("bTutorialDone", Node::Bool(true)),
            ("Difficulty", Node::String("EDifficulty::Nightmare".into())),
            ("Class", Node::String("EPlayerClass::Rogue".into())),
            ("Location.Y", Node::F64(-999.5)),
            ("Stats.Facing.Pitch", Node::F32(45.0)),
            ("SavedAt", Node::I64(638_100_000_000_000_000)),
            ("Waypoints[0].X", Node::F32(-1.0)),
            ("Completed[0]", Node::String("QuestZ".into())),
            ("Resources[0].value", Node::I32(500)),
            ("Npcs[1].value.Level", Node::I32(60)),
            ("Npcs[0].value.Name", Node::String("Robert".into())),
            ("PlayerGuid", Node::String("0123456789ABCDEF0123456789ABCDEF".into())),
            ("Icon.AssetPath", Node::String("/Game/Textures/T_Other.T_Other".into())),
        ],
    );
}

#[test]
fn utf16_strings_stay_utf16_and_ascii_upgrades_when_needed() {
    let bytes = build_save(None);
    let out = edit_and_check(
        &bytes,
        &[
            ("Motto", Node::String("plain ascii now".into())),
            ("Map", Node::String("/Game/Maps/\u{30c6}\u{30b9}\u{30c8}".into())),
        ],
    );
    let mut w = B::default();
    w.w("plain ascii now");
    assert!(out.windows(w.0.len()).any(|x| x == w.0.as_slice()), "UTF-16 encoding preserved");
    let mut w = B::default();
    w.w("/Game/Maps/\u{30c6}\u{30b9}\u{30c8}");
    assert!(out.windows(w.0.len()).any(|x| x == w.0.as_slice()), "switched to UTF-16");
}

#[test]
fn primitive_and_string_arrays_can_change_length() {
    let bytes = build_save(None);
    let pipeline = EnvelopePipeline::builtin();
    let mut s = pipeline.open(&bytes, &ctx()).unwrap();
    s.set(
        &p("Scores"),
        Node::Array(vec![Node::I32(1), Node::I32(2), Node::I32(3), Node::I32(4), Node::I32(5), Node::I32(6)]),
    )
    .unwrap();
    s.set(&p("Tags"), Node::Array(vec![Node::String("only".into())])).unwrap();
    s.set(
        &p("Completed"),
        Node::Array(vec![Node::String("QuestA".into()), Node::String("QuestB".into()), Node::String("QuestC".into())]),
    )
    .unwrap();
    let out = s.encode_verified(&pipeline, &ctx()).unwrap();
    let re = assert_lossless(&out);
    let r = re.root();
    assert_eq!(get(&r, "Scores[5]"), Node::I32(6));
    assert_eq!(get(&r, "Tags"), Node::Array(vec![Node::String("only".into())]));
    assert_eq!(get(&r, "Completed[2]"), Node::String("QuestC".into()));
    assert_eq!(get(&r, "Inventory[2].Count"), Node::I32(64));
    assert_eq!(get(&r, "Greeting"), get(&s.root(), "Greeting"));
}

#[test]
fn unrepresentable_structural_changes_are_rejected() {
    let bytes = build_save(None);
    let pipeline = EnvelopePipeline::builtin();
    let mut s = pipeline.open(&bytes, &ctx()).unwrap();
    // Struct-array elements are property lists: cannot be appended.
    let mut inv = match get(&s.root(), "Inventory") {
        Node::Array(a) => a,
        other => panic!("{other:?}"),
    };
    inv.push(inv[0].clone());
    assert!(s.set(&p("Inventory"), Node::Array(inv)).is_err());
    // Map entries cannot be added.
    assert!(s.set(&p("Resources"), Node::Array(Vec::new())).is_err());
    // New top-level property.
    let mut root = s.root();
    root.set(&p("NewProp"), Node::I32(1), true).unwrap();
    assert!(s.apply_ast(&root).is_err());
    // Nested struct fields cannot be renamed.
    let mut root = s.root();
    if let Some(Node::Map(m)) = root.get_mut(&p("Location")) {
        m.insert("W".into(), Node::F32(0.0));
    }
    assert!(s.apply_ast(&root).is_err());
    assert_eq!(s.encode().unwrap(), bytes, "rejected edits leave the save untouched");
}

#[test]
fn zlib_compressed_save_edit_and_recompress() {
    let raw = build_save(Some(1009));
    let packed = deflate(&raw, 6, true).unwrap();
    let pipeline = EnvelopePipeline::builtin();
    let mut s = pipeline.open(&packed, &ctx()).unwrap();
    assert_eq!(s.codec_id(), "gvas");
    assert!(!s.layers().is_empty(), "compression layer detected");
    assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert_eq!(s.encode().unwrap(), packed);
    s.set(&p("Level"), Node::I32(31)).unwrap();
    s.set(&p("PlayerName"), Node::String("Compressed Hero".into())).unwrap();
    let out = s.encode_verified(&pipeline, &ctx()).unwrap();
    assert_ne!(out, packed);
    assert!(!out.starts_with(b"GVAS"), "output is recompressed");
    let re = pipeline.open(&out, &ctx()).unwrap();
    assert_eq!(re.codec_id(), "gvas");
    assert_eq!(re.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert_eq!(get(&re.root(), "Level"), Node::I32(31));
    assert_eq!(get(&re.root(), "PlayerName"), Node::String("Compressed Hero".into()));
    assert_eq!(get(&re.root(), "Inventory[1].Count"), Node::I32(5));
    assert_eq!(re.payload().len(), raw.len() + "Compressed Hero".len() - "Hero".len());
}

#[test]
fn malformed_and_truncated_files_never_panic() {
    let bytes = build_save(None);
    let pipeline = EnvelopePipeline::builtin();
    // Truncation inside the property list: GVAS rejects it, raw takes over losslessly.
    for cut in [10, 40, 120, bytes.len() / 2, bytes.len() - 20] {
        let t = &bytes[..cut];
        let s = pipeline.open(t, &ctx()).unwrap();
        assert_eq!(s.encode().unwrap(), t);
        assert!(s.codec_id() == "raw" || s.envelope().round_trip == Some(RoundTripStatus::Lossless));
    }
    let s = pipeline.open(&bytes[..bytes.len() / 2], &ctx()).unwrap();
    assert_eq!(s.codec_id(), "raw");
    assert!(s.envelope().notes.iter().any(|n| n.contains("gvas")), "{:?}", s.envelope().notes);
    // Absurd count / size fields.
    let mut bad = bytes.clone();
    let pos = bad.windows(4).position(|w| w == b"Leve").unwrap();
    // Size field of `Level` -> huge.
    let size_at = pos + 6 + 4 + 12;
    bad[size_at..size_at + 4].copy_from_slice(&i32::MAX.to_le_bytes());
    let s = pipeline.open(&bad, &ctx()).unwrap();
    assert_eq!(s.codec_id(), "raw");
    assert_eq!(s.encode().unwrap(), bad);
    // Garbage after the magic.
    let mut junk = b"GVAS".to_vec();
    junk.extend((0..200u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8));
    let s = pipeline.open(&junk, &ctx()).unwrap();
    assert_eq!(s.encode().unwrap(), junk);
    // Direct codec use reports an error instead of panicking.
    assert!(pipeline.open_with_codec(&bytes[..60], &ctx(), "gvas").is_err());
}
