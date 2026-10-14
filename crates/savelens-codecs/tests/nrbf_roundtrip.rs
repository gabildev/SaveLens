//! End-to-end tests of the NRBF codec through the envelope pipeline.
//!
//! Fixtures are produced by an independent, test-side MS-NRBF writer (it does
//! not use the codec's serializer).

use savelens_codecs::layers::compression::StreamParams;
use savelens_codecs::{DetectContext, EnvelopePipeline};
use savelens_core::{Node, NodePath, RoundTripStatus};

const ASSEMBLY: &str = "Assembly-CSharp, Version=0.0.0.0, Culture=neutral, PublicKeyToken=null";
const LIST_OF_STRING: &str = "System.Collections.Generic.List`1[[System.String, mscorlib, Version=4.0.0.0, Culture=neutral, PublicKeyToken=b77a5c561934e089]]";

/// Minimal MS-NRBF writer for fixtures.
#[derive(Default)]
struct W(Vec<u8>);

// BinaryTypeEnumeration
const BT_PRIMITIVE: u8 = 0;
const BT_STRING: u8 = 1;
const BT_SYSTEM_CLASS: u8 = 3;
const BT_CLASS: u8 = 4;
const BT_STRING_ARRAY: u8 = 6;
const BT_PRIMITIVE_ARRAY: u8 = 7;
// PrimitiveTypeEnumeration
const PT_BOOLEAN: u8 = 1;
const PT_DOUBLE: u8 = 6;
const PT_INT32: u8 = 8;
const PT_INT64: u8 = 9;
const PT_SINGLE: u8 = 11;

impl W {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn i64(&mut self, v: i64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn f32(&mut self, v: f32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn f64(&mut self, v: f64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    /// LengthPrefixedString with a minimal 7-bit length.
    fn str(&mut self, s: &str) -> &mut Self {
        let mut n = s.len();
        loop {
            let b = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                self.0.push(b);
                break;
            }
            self.0.push(b | 0x80);
        }
        self.0.extend_from_slice(s.as_bytes());
        self
    }
    /// LengthPrefixedString with a deliberately non-minimal 2-byte length.
    fn str_wide(&mut self, s: &str) -> &mut Self {
        assert!(s.len() < 128);
        self.0.push(s.len() as u8 | 0x80);
        self.0.push(0);
        self.0.extend_from_slice(s.as_bytes());
        self
    }
    fn header(&mut self, root: i32) -> &mut Self {
        self.u8(0).i32(root).i32(-1).i32(1).i32(0)
    }
    fn library(&mut self, id: i32, name: &str) -> &mut Self {
        self.u8(12).i32(id).str(name)
    }
    fn object_string(&mut self, id: i32, s: &str) -> &mut Self {
        self.u8(6).i32(id).str(s)
    }
    fn reference(&mut self, id: i32) -> &mut Self {
        self.u8(9).i32(id)
    }
    /// Class header for record 4/5: ClassInfo + MemberTypeInfo.
    /// `members` = (name, binary type, additional info writer).
    fn class_header(
        &mut self,
        rtype: u8,
        id: i32,
        name: &str,
        members: &[(&str, u8, Extra)],
        library: Option<i32>,
    ) -> &mut Self {
        self.u8(rtype).i32(id).str(name).i32(members.len() as i32);
        for (n, _, _) in members {
            self.str(n);
        }
        for (_, bt, _) in members {
            self.u8(*bt);
        }
        for (_, _, extra) in members {
            match extra {
                Extra::None => {}
                Extra::Prim(p) => {
                    self.u8(*p);
                }
                Extra::SystemClass(n) => {
                    self.str(n);
                }
                Extra::Class(n, lib) => {
                    self.str(n).i32(*lib);
                }
            }
        }
        if let Some(lib) = library {
            self.i32(lib);
        }
        self
    }
}

enum Extra {
    None,
    Prim(u8),
    SystemClass(&'static str),
    Class(&'static str, i32),
}

/// A realistic Unity save: `PlayerData` serialised with BinaryFormatter.
fn unity_save() -> Vec<u8> {
    let mut w = W::default();
    w.header(1).library(2, ASSEMBLY);
    w.class_header(
        5,
        1,
        "PlayerData",
        &[
            ("gold", BT_PRIMITIVE, Extra::Prim(PT_INT32)),
            ("hp", BT_PRIMITIVE, Extra::Prim(PT_SINGLE)),
            ("flag", BT_PRIMITIVE, Extra::Prim(PT_BOOLEAN)),
            ("name", BT_STRING, Extra::None),
            ("stats", BT_CLASS, Extra::Class("Stats", 2)),
            ("stats2", BT_CLASS, Extra::Class("Stats", 2)),
            ("scores", BT_PRIMITIVE_ARRAY, Extra::Prim(PT_INT32)),
            ("tags", BT_STRING_ARRAY, Extra::None),
            ("inventory", BT_SYSTEM_CLASS, Extra::SystemClass(LIST_OF_STRING)),
            ("title", BT_STRING, Extra::None),
            ("title2", BT_STRING, Extra::None),
        ],
        Some(2),
    );
    // gold, hp, flag
    w.i32(1500).f32(87.5).u8(1);
    // name
    w.object_string(3, "Hero");
    // stats: nested ClassWithMembersAndTypes
    w.class_header(
        5,
        4,
        "Stats",
        &[
            ("level", BT_PRIMITIVE, Extra::Prim(PT_INT32)),
            ("xp", BT_PRIMITIVE, Extra::Prim(PT_INT64)),
            ("speed", BT_PRIMITIVE, Extra::Prim(PT_DOUBLE)),
        ],
        Some(2),
    );
    w.i32(7).i64(12345).f64(1.25);
    // stats2: ClassWithId reusing the metadata of object 4
    w.u8(1).i32(5).i32(4).i32(8).i64(999).f64(2.5);
    // scores, tags, inventory are deferred top-level objects
    w.reference(6).reference(7).reference(8);
    // title + title2 sharing the same string object
    w.u8(6).i32(9).str("Champion");
    w.reference(9);

    // ArraySinglePrimitive of Int32
    w.u8(15).i32(6).i32(4).u8(PT_INT32).i32(10).i32(20).i32(30).i32(40);
    // ArraySingleString with a null run and a shared reference
    w.u8(17).i32(7).i32(6);
    w.object_string(10, "sword");
    w.u8(13).u8(2); // ObjectNullMultiple256 (2 nulls)
    w.u8(6).i32(11).str_wide("shield"); // non-minimal length prefix
    w.reference(3); // shares "Hero"
    w.u8(10); // ObjectNull
              // List<string>
    w.class_header(
        4,
        8,
        LIST_OF_STRING,
        &[
            ("_items", BT_STRING_ARRAY, Extra::None),
            ("_size", BT_PRIMITIVE, Extra::Prim(PT_INT32)),
            ("_version", BT_PRIMITIVE, Extra::Prim(PT_INT32)),
        ],
        None,
    );
    w.reference(12).i32(2).i32(2);
    // BinaryArray (Single, rank 1) of strings with a trailing null run
    w.u8(7).i32(12).u8(0).i32(1).i32(4).u8(BT_STRING);
    w.object_string(13, "potion").object_string(14, "elixir");
    w.u8(13).u8(2);
    // MessageEnd
    w.u8(11);
    w.0
}

fn p(s: &str) -> NodePath {
    NodePath::parse(s).unwrap()
}

fn s(v: &str) -> Node {
    Node::String(v.into())
}

fn find(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn opens_losslessly_with_expected_projection() {
    let bytes = unity_save();
    let pipeline = EnvelopePipeline::builtin();
    let ctx = DetectContext::for_file("save.dat");
    let opened = pipeline.open(&bytes, &ctx).unwrap();
    assert_eq!(opened.codec_id(), "nrbf");
    assert!(opened.layers().is_empty());
    assert_eq!(opened.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert_eq!(opened.encode().unwrap(), bytes);

    let root = opened.root();
    let get = |path: &str| root.get(&p(path)).cloned().unwrap_or_else(|| panic!("missing {path}"));
    assert_eq!(root.get(&NodePath::root().key("$type")), Some(&s("PlayerData")));
    assert_eq!(get("gold"), Node::I32(1500));
    assert_eq!(get("hp"), Node::F32(87.5));
    assert_eq!(get("flag"), Node::Bool(true));
    assert_eq!(get("name"), s("Hero"));
    assert_eq!(get("stats.level"), Node::I32(7));
    assert_eq!(get("stats.xp"), Node::I64(12345));
    assert_eq!(get("stats.speed"), Node::F64(1.25));
    assert_eq!(root.get(&p("stats2").key("$type")), Some(&s("Stats")));
    assert_eq!(get("stats2.level"), Node::I32(8));
    assert_eq!(get("stats2.xp"), Node::I64(999));
    assert_eq!(get("stats2.speed"), Node::F64(2.5));
    assert_eq!(get("scores"), Node::Array(vec![Node::I32(10), Node::I32(20), Node::I32(30), Node::I32(40)]));
    assert_eq!(get("tags"), Node::Array(vec![s("sword"), Node::Null, Node::Null, s("shield"), s("Hero"), Node::Null]));
    assert_eq!(root.get(&p("inventory").key("$type")), Some(&s(LIST_OF_STRING)));
    assert_eq!(get("inventory._items"), Node::Array(vec![s("potion"), s("elixir"), Node::Null, Node::Null]));
    assert_eq!(get("inventory._size"), Node::I32(2));
    assert_eq!(get("inventory._version"), Node::I32(2));
    assert_eq!(get("title"), s("Champion"));
    assert_eq!(get("title2"), s("Champion"));
    let notes = opened.envelope().notes.join("\n");
    assert!(notes.contains("referenced from several places"), "{notes}");
    assert!(notes.contains(ASSEMBLY), "{notes}");
}

#[test]
fn edits_values_and_reencodes() {
    let bytes = unity_save();
    let pipeline = EnvelopePipeline::builtin();
    let ctx = DetectContext::for_file("save.dat");
    let mut opened = pipeline.open(&bytes, &ctx).unwrap();
    let before = opened.root();

    opened.set(&p("gold"), Node::I64(99_999)).unwrap();
    opened.set(&p("hp"), Node::F32(250.75)).unwrap();
    opened.set(&p("name"), s("Sir Lancelot of Camelot, Knight of the Round Table")).unwrap();
    opened.set(&p("scores[2]"), Node::I64(333)).unwrap();
    opened.set(&p("stats2.speed"), Node::F64(9.5)).unwrap();
    opened.set(&p("inventory._items[1]"), s("mega elixir")).unwrap();
    opened.set(&p("flag"), Node::Bool(false)).unwrap();

    let out = opened.encode_verified(&pipeline, &ctx).unwrap();
    assert_ne!(out, bytes);
    // Untouched non-minimal length prefix survives.
    assert!(find(&out, &[0x86, 0x00, b's', b'h', b'i', b'e', b'l', b'd']));

    let reopened = pipeline.open(&out, &ctx).unwrap();
    assert_eq!(reopened.codec_id(), "nrbf");
    assert_eq!(reopened.envelope().round_trip, Some(RoundTripStatus::Lossless));
    let after = reopened.root();
    let mut expected = before.clone();
    let long = s("Sir Lancelot of Camelot, Knight of the Round Table");
    expected.set(&p("gold"), Node::I32(99_999), false).unwrap();
    expected.set(&p("hp"), Node::F32(250.75), false).unwrap();
    expected.set(&p("name"), long.clone(), false).unwrap();
    // The "Hero" string object is shared with tags[4]: it changes there too.
    expected.set(&p("tags[4]"), long, false).unwrap();
    expected.set(&p("scores[2]"), Node::I32(333), false).unwrap();
    expected.set(&p("stats2.speed"), Node::F64(9.5), false).unwrap();
    expected.set(&p("inventory._items[1]"), s("mega elixir"), false).unwrap();
    expected.set(&p("flag"), Node::Bool(false), false).unwrap();
    assert_eq!(after, expected);
    // Length of the file grew exactly by the string growth (+ varint).
    let grow = "Sir Lancelot of Camelot, Knight of the Round Table".len() - "Hero".len() + "mega elixir".len()
        - "elixir".len();
    assert_eq!(out.len(), bytes.len() + grow);

    // Shared string edited through its second occurrence changes both.
    let mut again = pipeline.open(&bytes, &ctx).unwrap();
    again.set(&p("title2"), s("Legend")).unwrap();
    let out2 = again.encode_verified(&pipeline, &ctx).unwrap();
    let r2 = pipeline.open(&out2, &ctx).unwrap().root();
    assert_eq!(r2.get(&p("title")), Some(&s("Legend")));
    assert_eq!(r2.get(&p("title2")), Some(&s("Legend")));
}

#[test]
fn rejects_type_and_structure_changes() {
    let bytes = unity_save();
    let pipeline = EnvelopePipeline::builtin();
    let ctx = DetectContext::default();
    let mut opened = pipeline.open(&bytes, &ctx).unwrap();
    assert!(opened.set(&p("gold"), s("lots")).is_err());
    assert!(opened.set(&p("gold"), Node::I64(1 << 40)).is_err());
    assert!(opened.set(&NodePath::root().key("$type"), s("Hacked")).is_err());
    assert!(opened.set(&p("tags[1]"), s("filled")).is_err());
    assert!(opened.set(&p("scores"), Node::Array(vec![Node::I32(1)])).is_err());
    let mut root = opened.root();
    root.set(&p("extra"), Node::I32(1), true).unwrap();
    assert!(opened.apply_ast(&root).is_err());
    // Nothing was changed by the failed edits.
    assert_eq!(opened.encode().unwrap(), bytes);
}

#[test]
fn gzip_wrapped_save() {
    let bytes = unity_save();
    let gz =
        StreamParams::Gzip { header: vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3], level: 6 }.compress(&bytes).unwrap();
    let pipeline = EnvelopePipeline::builtin();
    let ctx = DetectContext::for_file("save.dat");
    let mut opened = pipeline.open(&gz, &ctx).unwrap();
    assert_eq!(opened.codec_id(), "nrbf");
    assert_eq!(opened.layers().len(), 1);
    assert_eq!(opened.payload(), bytes.as_slice());
    assert_eq!(opened.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert_eq!(opened.encode().unwrap(), gz);

    opened.set(&p("gold"), Node::I32(123_456)).unwrap();
    opened.set(&p("stats.level"), Node::I32(42)).unwrap();
    let out = opened.encode_verified(&pipeline, &ctx).unwrap();
    assert_eq!(&out[..2], &[0x1f, 0x8b]);
    let reopened = pipeline.open(&out, &ctx).unwrap();
    assert_eq!(reopened.codec_id(), "nrbf");
    assert_eq!(reopened.root().get(&p("gold")), Some(&Node::I32(123_456)));
    assert_eq!(reopened.root().get(&p("stats.level")), Some(&Node::I32(42)));
    assert_eq!(reopened.root().get(&p("name")), Some(&s("Hero")));
}

#[test]
fn truncated_input_never_panics() {
    let bytes = unity_save();
    let pipeline = EnvelopePipeline::builtin();
    let ctx = DetectContext::default();
    let codec = pipeline.registry().get("nrbf").unwrap();
    for n in 0..bytes.len() {
        let cut = &bytes[..n];
        assert!(codec.decode(cut, &ctx).is_err(), "prefix of {n} bytes decoded");
        assert_eq!(codec.detect(cut, &ctx), 0);
        let opened = pipeline.open(cut, &ctx).unwrap();
        assert_ne!(opened.codec_id(), "nrbf");
        assert_eq!(opened.encode().unwrap(), cut);
    }
    // Corrupt bytes (every single-byte flip) never panic either.
    for i in 0..bytes.len() {
        let mut b = bytes.clone();
        b[i] ^= 0xff;
        let _ = codec.decode(&b, &ctx);
    }
}

#[test]
fn method_call_record_falls_back_to_raw() {
    let mut w = W::default();
    w.header(1);
    // BinaryMethodCall: MessageEnum flags, method name, type name (as
    // StringValueWithCode records).
    w.u8(21).i32(0x0000_0011).u8(18).str("Save").u8(18).str("Game.SaveSystem");
    w.u8(11);
    let bytes = w.0;
    let pipeline = EnvelopePipeline::builtin();
    let ctx = DetectContext::default();
    let codec = pipeline.registry().get("nrbf").unwrap();
    let err = codec.decode(&bytes, &ctx).unwrap_err();
    assert!(err.to_string().contains("unsupported"), "{err}");
    let opened = pipeline.open(&bytes, &ctx).unwrap();
    assert_eq!(opened.codec_id(), "raw");
    assert_eq!(opened.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert_eq!(opened.encode().unwrap(), bytes);
}
