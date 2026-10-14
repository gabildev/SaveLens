//! MessagePack integration tests: a test-side writer emits every marker
//! family (including non-minimal encodings) and the files go through the full
//! `EnvelopePipeline`.

use savelens_codecs::layers::compression::StreamParams;
use savelens_codecs::{DetectContext, EnvelopePipeline};
use savelens_core::{Node, NodePath, RoundTripStatus};

/// Test-side MessagePack writer with explicit marker choice.
#[derive(Default)]
struct W(Vec<u8>);

impl W {
    fn raw(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(b);
        self
    }
    fn fixstr(&mut self, s: &str) -> &mut Self {
        assert!(s.len() < 32);
        self.0.push(0xa0 | s.len() as u8);
        self.raw(s.as_bytes())
    }
    fn str8(&mut self, s: &str) -> &mut Self {
        self.0.extend_from_slice(&[0xd9, s.len() as u8]);
        self.raw(s.as_bytes())
    }
    fn str16(&mut self, s: &str) -> &mut Self {
        self.0.push(0xda);
        self.0.extend_from_slice(&(s.len() as u16).to_be_bytes());
        self.raw(s.as_bytes())
    }
    fn str32(&mut self, s: &str) -> &mut Self {
        self.0.push(0xdb);
        self.0.extend_from_slice(&(s.len() as u32).to_be_bytes());
        self.raw(s.as_bytes())
    }
    fn bin8(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(&[0xc4, b.len() as u8]);
        self.raw(b)
    }
    fn bin16(&mut self, b: &[u8]) -> &mut Self {
        self.0.push(0xc5);
        self.0.extend_from_slice(&(b.len() as u16).to_be_bytes());
        self.raw(b)
    }
    fn bin32(&mut self, b: &[u8]) -> &mut Self {
        self.0.push(0xc6);
        self.0.extend_from_slice(&(b.len() as u32).to_be_bytes());
        self.raw(b)
    }
    fn fixmap(&mut self, n: u8) -> &mut Self {
        self.raw(&[0x80 | n])
    }
    fn map16(&mut self, n: u16) -> &mut Self {
        self.0.push(0xde);
        self.raw(&n.to_be_bytes())
    }
    fn map32(&mut self, n: u32) -> &mut Self {
        self.0.push(0xdf);
        self.raw(&n.to_be_bytes())
    }
    fn fixarray(&mut self, n: u8) -> &mut Self {
        self.raw(&[0x90 | n])
    }
    fn array16(&mut self, n: u16) -> &mut Self {
        self.0.push(0xdc);
        self.raw(&n.to_be_bytes())
    }
    fn array32(&mut self, n: u32) -> &mut Self {
        self.0.push(0xdd);
        self.raw(&n.to_be_bytes())
    }
    fn u8(&mut self, v: u8) -> &mut Self {
        self.raw(&[0xcc, v])
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.push(0xcd);
        self.raw(&v.to_be_bytes())
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.push(0xce);
        self.raw(&v.to_be_bytes())
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.push(0xcf);
        self.raw(&v.to_be_bytes())
    }
    fn i8(&mut self, v: i8) -> &mut Self {
        self.raw(&[0xd0, v as u8])
    }
    fn i16(&mut self, v: i16) -> &mut Self {
        self.0.push(0xd1);
        self.raw(&v.to_be_bytes())
    }
    fn i32(&mut self, v: i32) -> &mut Self {
        self.0.push(0xd2);
        self.raw(&v.to_be_bytes())
    }
    fn i64(&mut self, v: i64) -> &mut Self {
        self.0.push(0xd3);
        self.raw(&v.to_be_bytes())
    }
    fn f32(&mut self, v: f32) -> &mut Self {
        self.0.push(0xca);
        self.raw(&v.to_be_bytes())
    }
    fn f64(&mut self, v: f64) -> &mut Self {
        self.0.push(0xcb);
        self.raw(&v.to_be_bytes())
    }
    fn ext(&mut self, marker: u8, ty: i8, data: &[u8]) -> &mut Self {
        self.0.push(marker);
        match marker {
            0xc7 => self.0.push(data.len() as u8),
            0xc8 => self.0.extend_from_slice(&(data.len() as u16).to_be_bytes()),
            0xc9 => self.0.extend_from_slice(&(data.len() as u32).to_be_bytes()),
            _ => {}
        }
        self.0.push(ty as u8);
        self.raw(data)
    }
}

/// A save covering every marker family.
fn save() -> Vec<u8> {
    let mut w = W::default();
    w.map16(9); // non-minimal map marker
    w.fixstr("player").fixmap(6);
    {
        w.fixstr("name").str16("Hero"); // short string as str16
        w.fixstr("level").u32(5); // 5 as uint32
        w.fixstr("gold").u64(1_000_000);
        w.fixstr("hp").f32(87.5);
        w.fixstr("pos").fixarray(3).f64(1.25).f64(-2.0).f64(1e300);
        w.fixstr("alive").raw(&[0xc3]);
    }
    w.str8("ints").array16(12);
    {
        w.raw(&[0x07, 0xe0]); // positive / negative fixint
        w.u8(200).u16(60_000).i8(-100).i16(-30_000).i32(-2_000_000_000).i64(i64::MIN);
        w.i8(3); // 3 as int8
        w.u64(u64::MAX).raw(&[0xc0, 0xc2]);
    }
    w.str32("blobs").array32(3).bin8(&[1, 2, 3]).bin16(&[4; 300]).bin32(&[5, 6]);
    w.fixstr("exts").fixarray(8);
    {
        w.ext(0xd4, 1, &[9]);
        w.ext(0xd5, 2, &[9, 8]);
        w.ext(0xd6, -1, &[0, 0, 0, 42]);
        w.ext(0xd7, 3, &[7; 8]);
        w.ext(0xd8, 4, &[6; 16]);
        w.ext(0xc7, 5, &[1, 2, 3]);
        w.ext(0xc8, 6, &[2; 260]);
        w.ext(0xc9, 7, &[]);
    }
    w.fixstr("bykey").map32(2).raw(&[0x01]).fixstr("one").raw(&[0xcd, 0x00, 0x02]).fixstr("two");
    w.fixstr("odd").fixmap(1).fixarray(1).raw(&[0x01]).fixstr("array key");
    w.fixstr("bad").raw(&[0xa2, 0xff, 0xfe]); // invalid UTF-8 str
    w.fixstr("long").str16(&"x".repeat(1000));
    w.fixstr("empty").fixmap(0);
    w.0
}

fn p(s: &str) -> NodePath {
    NodePath::parse(s).unwrap()
}

#[test]
fn every_marker_roundtrips_and_projects() {
    let file = save();
    let ctx = DetectContext::for_file("save.bin");
    let pipeline = EnvelopePipeline::builtin();
    let s = pipeline.open(&file, &ctx).unwrap();
    assert_eq!(s.codec_id(), "msgpack", "{:?}", s.envelope().notes);
    assert!(s.layers().is_empty());
    assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert_eq!(s.encode().unwrap(), file);

    let r = s.root();
    assert_eq!(r.get(&p("player.name")), Some(&Node::String("Hero".into())));
    assert_eq!(r.get(&p("player.level")), Some(&Node::I64(5)));
    assert_eq!(r.get(&p("player.hp")), Some(&Node::F32(87.5)));
    assert_eq!(r.get(&p("player.pos[2]")), Some(&Node::F64(1e300)));
    assert_eq!(r.get(&p("player.alive")), Some(&Node::Bool(true)));
    let ints: Vec<Node> = vec![
        Node::I64(7),
        Node::I64(-32),
        Node::I64(200),
        Node::I64(60_000),
        Node::I64(-100),
        Node::I64(-30_000),
        Node::I64(-2_000_000_000),
        Node::I64(i64::MIN),
        Node::I64(3),
        Node::U64(u64::MAX),
        Node::Null,
        Node::Bool(false),
    ];
    assert_eq!(r.get(&p("ints")), Some(&Node::Array(ints)));
    assert_eq!(r.get(&p("blobs[1]")), Some(&Node::Bytes(vec![4; 300])));
    assert_eq!(
        r.get(&p("exts[2]")),
        Some(&Node::map_from([("type", Node::I8(-1)), ("data", Node::Bytes(vec![0, 0, 0, 42]))]))
    );
    assert_eq!(r.get(&p("bykey")).map(|n| n.child_count()), Some(2));
    assert_eq!(r.get(&NodePath::root().key("bykey").key("2")), Some(&Node::String("two".into())));
    assert_eq!(r.get(&p("odd[0].key")), Some(&Node::Array(vec![Node::I64(1)])));
    assert_eq!(r.get(&p("bad")), Some(&Node::Bytes(vec![0xff, 0xfe])));
    assert_eq!(r.get(&p("empty")), Some(&Node::map()));
}

#[test]
fn edits_keep_markers_and_verify() {
    let file = save();
    let ctx = DetectContext::for_file("save.bin");
    let pipeline = EnvelopePipeline::builtin();
    let mut s = pipeline.open(&file, &ctx).unwrap();

    s.set(&p("player.level"), Node::I64(99)).unwrap(); // stays uint32
    s.set(&p("player.name"), Node::String("Heroine".into())).unwrap(); // stays str16
    s.set(&p("player.hp"), Node::F64(12.25)).unwrap(); // stays float32
    s.set(&p("ints[8]"), Node::I64(-5)).unwrap(); // int8 keeps marker
    s.set(&p("ints[2]"), Node::I64(-1)).unwrap(); // uint8 cannot hold -1: negative fixint
    s.set(&p("ints[0]"), Node::I64(1000)).unwrap(); // fixint grows to uint16
    s.set(&p("exts[0].data"), Node::Bytes(vec![1])).unwrap(); // fixext1 kept
    s.set(&p("exts[1].data"), Node::Bytes(vec![1, 2, 3])).unwrap(); // fixext2 -> ext8
    s.set(&NodePath::root().key("bykey").key("1"), Node::String("uno".into())).unwrap();
    s.set(&p("odd[0].value"), Node::String("changed".into())).unwrap();
    s.set(&p("bad"), Node::Bytes(vec![0xfd])).unwrap();
    let mut root = s.root();
    match root.get_mut(&p("player.pos")) {
        Some(Node::Array(a)) => {
            a.pop();
        }
        other => panic!("{other:?}"),
    }
    match root.get_mut(&p("bykey")) {
        Some(Node::Map(m)) => {
            m.insert("3".into(), Node::String("three".into()));
        }
        other => panic!("{other:?}"),
    }
    match root.get_mut(&p("empty")) {
        Some(Node::Map(m)) => {
            m.insert("added".into(), Node::Array(vec![Node::I64(1), Node::F32(2.0), Node::Null]));
        }
        other => panic!("{other:?}"),
    }
    match &mut root {
        Node::Map(m) => {
            m.shift_remove("long");
        }
        other => panic!("{other:?}"),
    }
    s.apply_ast(&root).unwrap();
    assert!(s.set(&p("player.level"), Node::String("x".into())).is_err());
    assert!(s.set(&p("player.name"), Node::I64(1)).is_err());

    let out = s.encode_verified(&pipeline, &ctx).unwrap();
    // Marker checks on the raw output.
    let find = |needle: &[u8]| out.windows(needle.len()).any(|w| w == needle);
    assert!(find(&[0xa5, b'l', b'e', b'v', b'e', b'l', 0xce, 0, 0, 0, 99]));
    assert!(find(b"\xa4name\xda\x00\x07Heroine"));
    assert!(find(&[0xa2, b'h', b'p', 0xca]));
    assert!(find(&[0xdc, 0x00, 0x0c, 0xcd, 0x03, 0xe8, 0xe0, 0xff]));
    assert!(find(&[0xd0, 0xfb, 0xcf]));
    assert!(find(&[0xd4, 1, 1, 0xc7, 3, 2, 1, 2, 3]));
    assert!(find(&[0xdf, 0, 0, 0, 3, 0x01, 0xa3, b'u', b'n', b'o']));
    assert!(find(&[0x03, 0xa5, b't', b'h', b'r', b'e', b'e']));
    assert!(find(&[0xa3, b'b', b'a', b'd', 0xa1, 0xfd]));
    assert_eq!(&out[..3], &[0xde, 0x00, 0x08]);

    let re = pipeline.open(&out, &ctx).unwrap();
    assert_eq!(re.codec_id(), "msgpack");
    assert_eq!(re.envelope().round_trip, Some(RoundTripStatus::Lossless));
    let r = re.root();
    assert_eq!(r, s.root());
    assert_eq!(r.get(&p("player.hp")), Some(&Node::F32(12.25)));
    assert_eq!(r.get(&p("player.pos")).map(|n| n.child_count()), Some(2));
    assert_eq!(r.get(&p("empty.added[1]")), Some(&Node::F32(2.0)));
    assert!(r.get(&p("long")).is_none());
}

#[test]
fn multiple_top_level_values() {
    let mut w = W::default();
    w.fixmap(1).fixstr("v").u16(2); // header value
    w.fixarray(2).i32(10).str8("x"); // payload value
    w.u32(7); // trailing counter
    let file = w.0;
    let ctx = DetectContext::for_file("slot.msgpack");
    let pipeline = EnvelopePipeline::builtin();
    let mut s = pipeline.open(&file, &ctx).unwrap();
    assert_eq!(s.codec_id(), "msgpack");
    assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert!(s.envelope().notes.iter().any(|n| n.contains("3 concatenated")));
    assert_eq!(s.root().child_count(), 3);
    assert_eq!(s.root().get(&p("[1][1]")), Some(&Node::String("x".into())));

    s.set(&p("[2]"), Node::I64(8)).unwrap();
    s.set(&p("[1][0]"), Node::I64(-10)).unwrap();
    let mut root = s.root();
    if let Node::Array(a) = &mut root {
        a.push(Node::String("appended".into()));
    }
    s.apply_ast(&root).unwrap();
    let out = s.encode_verified(&pipeline, &ctx).unwrap();
    let mut expect = W::default();
    expect.fixmap(1).fixstr("v").u16(2).fixarray(2).i32(-10).str8("x").u32(8).fixstr("appended");
    assert_eq!(out, expect.0);
    // Collapsing to a single value would change the document shape.
    assert!(s.apply_ast(&Node::Array(vec![Node::I64(1)])).is_err());
}

#[test]
fn gzip_wrapped_msgpack_edit() {
    let file =
        StreamParams::Gzip { header: vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3], level: 6 }.compress(&save()).unwrap();
    let ctx = DetectContext::default();
    let pipeline = EnvelopePipeline::builtin();
    let mut s = pipeline.open(&file, &ctx).unwrap();
    assert_eq!(s.codec_id(), "msgpack");
    assert_eq!(s.layers().len(), 1);
    assert_eq!(s.encode().unwrap(), file);
    s.set(&p("player.gold"), Node::I64(5)).unwrap();
    let out = s.encode_verified(&pipeline, &ctx).unwrap();
    let re = pipeline.open(&out, &ctx).unwrap();
    assert_eq!(re.root().get(&p("player.gold")), Some(&Node::I64(5)));
}

#[test]
fn text_is_not_msgpack() {
    let pipeline = EnvelopePipeline::builtin();
    let json = br#"{"player": {"gold": 10, "name": "Hero"}, "list": [1, 2, 3]}"#;
    let s = pipeline.open(json, &DetectContext::for_file("save.dat")).unwrap();
    assert_eq!(s.codec_id(), "json");
    let codec = pipeline.registry().get("msgpack").unwrap();
    assert_eq!(codec.detect(json, &DetectContext::for_file("save.dat")), 0);
    assert_eq!(codec.detect(b"[1, 2]", &DetectContext::default()), 0);
    assert_eq!(codec.detect(b"key=value\nother=1\n", &DetectContext::for_file("a.msgpack")), 0);
    // Truncated msgpack is rejected without panicking.
    let file = save();
    for n in (0..file.len()).step_by(13) {
        assert_eq!(codec.detect(&file[..n], &DetectContext::for_file("x.msgpack")), 0, "prefix {n}");
        assert!(pipeline.open_with_codec(&file[..n], &DetectContext::default(), "msgpack").is_err());
    }
}
