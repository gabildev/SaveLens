//! NBT integration tests: a test-side NBT writer (independent of the codec
//! serializer) builds Minecraft-like `level.dat` files that go through the
//! full `EnvelopePipeline`.

use savelens_codecs::layers::compression::StreamParams;
use savelens_codecs::{DetectContext, EnvelopePipeline};
use savelens_core::{CompressionAlgorithm, LayerDescriptor, Node, NodePath, RoundTripStatus};

/// Test-side NBT value.
enum T {
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Bytes(Vec<i8>),
    Str(&'static str),
    List(u8, Vec<T>),
    Compound(Vec<(&'static str, T)>),
    Ints(Vec<i32>),
    Longs(Vec<i64>),
}

/// Test-side writer. `java` selects big-endian + Modified UTF-8.
struct W {
    out: Vec<u8>,
    java: bool,
}

impl W {
    fn num<const N: usize>(&mut self, be: [u8; N], le: [u8; N]) {
        self.out.extend_from_slice(if self.java { &be } else { &le });
    }

    fn text(&mut self, s: &str) {
        let bytes: Vec<u8> = if self.java {
            // Hand-rolled Modified UTF-8.
            let mut v = Vec::new();
            for u in s.encode_utf16() {
                if u != 0 && u < 0x80 {
                    v.push(u as u8);
                } else if u < 0x800 {
                    v.push(0xc0 | (u >> 6) as u8);
                    v.push(0x80 | (u & 0x3f) as u8);
                } else {
                    v.push(0xe0 | (u >> 12) as u8);
                    v.push(0x80 | ((u >> 6) & 0x3f) as u8);
                    v.push(0x80 | (u & 0x3f) as u8);
                }
            }
            v
        } else {
            s.as_bytes().to_vec()
        };
        let n = bytes.len() as u16;
        self.num(n.to_be_bytes(), n.to_le_bytes());
        self.out.extend_from_slice(&bytes);
    }

    fn id(t: &T) -> u8 {
        match t {
            T::Byte(_) => 1,
            T::Short(_) => 2,
            T::Int(_) => 3,
            T::Long(_) => 4,
            T::Float(_) => 5,
            T::Double(_) => 6,
            T::Bytes(_) => 7,
            T::Str(_) => 8,
            T::List(..) => 9,
            T::Compound(_) => 10,
            T::Ints(_) => 11,
            T::Longs(_) => 12,
        }
    }

    fn len(&mut self, n: usize) {
        let n = n as i32;
        self.num(n.to_be_bytes(), n.to_le_bytes());
    }

    fn payload(&mut self, t: &T) {
        match t {
            T::Byte(v) => self.out.push(*v as u8),
            T::Short(v) => self.num(v.to_be_bytes(), v.to_le_bytes()),
            T::Int(v) => self.num(v.to_be_bytes(), v.to_le_bytes()),
            T::Long(v) => self.num(v.to_be_bytes(), v.to_le_bytes()),
            T::Float(v) => self.num(v.to_be_bytes(), v.to_le_bytes()),
            T::Double(v) => self.num(v.to_be_bytes(), v.to_le_bytes()),
            T::Bytes(b) => {
                self.len(b.len());
                self.out.extend(b.iter().map(|x| *x as u8));
            }
            T::Str(s) => self.text(s),
            T::List(elem, items) => {
                self.out.push(*elem);
                self.len(items.len());
                for it in items {
                    self.payload(it);
                }
            }
            T::Compound(entries) => {
                for (name, v) in entries {
                    self.out.push(Self::id(v));
                    self.text(name);
                    self.payload(v);
                }
                self.out.push(0);
            }
            T::Ints(v) => {
                self.len(v.len());
                for x in v {
                    self.num(x.to_be_bytes(), x.to_le_bytes());
                }
            }
            T::Longs(v) => {
                self.len(v.len());
                for x in v {
                    self.num(x.to_be_bytes(), x.to_le_bytes());
                }
            }
        }
    }

    fn root(name: &str, body: &T, java: bool) -> Vec<u8> {
        let mut w = W { out: vec![10], java };
        w.text(name);
        w.payload(body);
        w.out
    }
}

const PLAYER_NAME: &str = "Alex\u{0}\u{1f600}\u{e9}";

fn item(slot: i8, id: &'static str, count: i8) -> T {
    T::Compound(vec![("Slot", T::Byte(slot)), ("id", T::Str(id)), ("Count", T::Byte(count))])
}

fn level() -> T {
    T::Compound(vec![(
        "Data",
        T::Compound(vec![
            ("LevelName", T::Str("New World")),
            ("version", T::Int(19133)),
            ("RandomSeed", T::Long(-4_172_144_997_902_289_642)),
            ("raining", T::Byte(0)),
            ("SpawnY", T::Short(64)),
            (
                "Player",
                T::Compound(vec![
                    ("Name", T::Str(PLAYER_NAME)),
                    ("Health", T::Float(20.0)),
                    ("XpLevel", T::Int(7)),
                    ("XpP", T::Float(0.25)),
                    ("Pos", T::List(6, vec![T::Double(12.5), T::Double(64.0), T::Double(-301.75)])),
                    (
                        "Inventory",
                        T::List(10, vec![item(0, "minecraft:diamond_sword", 1), item(1, "minecraft:torch", 64)]),
                    ),
                    ("EnderItems", T::List(10, vec![])),
                    ("ActiveEffects", T::List(0, vec![])),
                    ("UUID", T::Ints(vec![-1, 2, 3, i32::MIN])),
                ]),
            ),
            ("Biomes", T::Bytes(vec![1, -2, 3, 127, -128])),
            ("Heightmap", T::Longs(vec![i64::MIN, 0, 1 << 40])),
        ]),
    )])
}

fn java_bytes() -> Vec<u8> {
    W::root("", &level(), true)
}

fn bedrock_bytes() -> Vec<u8> {
    let body = W::root("", &level(), false);
    let mut out = 10i32.to_le_bytes().to_vec();
    out.extend_from_slice(&(body.len() as i32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

fn p(s: &str) -> NodePath {
    NodePath::parse(s).unwrap()
}

/// Opens, checks the lossless round trip, edits, verifies and reopens.
fn exercise(file: &[u8], ctx: &DetectContext) -> Vec<u8> {
    let pipeline = EnvelopePipeline::builtin();
    let mut s = pipeline.open(file, ctx).unwrap();
    assert_eq!(s.codec_id(), "nbt", "notes: {:?}", s.envelope().notes);
    assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert_eq!(s.encode().unwrap(), file);

    let root = s.root();
    assert_eq!(root.get(&p("Data.Player.Health")), Some(&Node::F32(20.0)));
    assert_eq!(root.get(&p("Data.Player.Name")), Some(&Node::String(PLAYER_NAME.into())));
    assert_eq!(root.get(&p("Data.Player.Pos[2]")), Some(&Node::F64(-301.75)));
    assert_eq!(root.get(&p("Data.Player.UUID[3]")), Some(&Node::I32(i32::MIN)));
    assert_eq!(root.get(&p("Data.Heightmap[2]")), Some(&Node::I64(1 << 40)));
    assert_eq!(root.get(&p("Data.Biomes")), Some(&Node::Bytes(vec![1, 0xfe, 3, 127, 0x80])));
    assert_eq!(root.get(&p("Data.Player.Inventory[1].id")), Some(&Node::String("minecraft:torch".into())));
    assert_eq!(root.get(&p("Data.Player.ActiveEffects")), Some(&Node::Array(vec![])));

    // Edits: float (given as f64), int (given as i64), MUTF-8 string, new inventory item.
    s.set(&p("Data.Player.Health"), Node::F64(13.5)).unwrap();
    s.set(&p("Data.Player.XpLevel"), Node::I64(30)).unwrap();
    let new_name = "Steve\u{0}\u{1f525} the \u{2603}";
    s.set(&p("Data.Player.Name"), Node::String(new_name.into())).unwrap();
    let mut root = s.root();
    match root.get_mut(&p("Data.Player.Inventory")) {
        Some(Node::Array(items)) => items.push(Node::map_from([
            ("Slot", Node::I8(2)),
            ("id", Node::String("minecraft:golden_apple".into())),
            ("Count", Node::I8(3)),
        ])),
        other => panic!("{other:?}"),
    }
    s.apply_ast(&root).unwrap();
    // Type changes are refused.
    assert!(s.set(&p("Data.Player.Health"), Node::String("full".into())).is_err());
    assert!(s.set(&p("Data.Player.XpLevel"), Node::I64(i64::MAX)).is_err());

    let out = s.encode_verified(&pipeline, ctx).unwrap();
    assert_ne!(out, file);

    let re = pipeline.open(&out, ctx).unwrap();
    assert_eq!(re.codec_id(), "nbt");
    assert_eq!(re.envelope().round_trip, Some(RoundTripStatus::Lossless));
    let r = re.root();
    assert_eq!(r.get(&p("Data.Player.Health")), Some(&Node::F32(13.5)));
    assert_eq!(r.get(&p("Data.Player.XpLevel")), Some(&Node::I32(30)));
    assert_eq!(r.get(&p("Data.Player.Name")), Some(&Node::String(new_name.into())));
    assert_eq!(r.get(&p("Data.Player.Inventory[2].Count")), Some(&Node::I8(3)));
    assert_eq!(r.get(&p("Data.Player.Inventory[2].id")), Some(&Node::String("minecraft:golden_apple".into())));
    // Untouched values survive.
    assert_eq!(r.get(&p("Data.RandomSeed")), Some(&Node::I64(-4_172_144_997_902_289_642)));
    assert_eq!(r, s.root());
    out
}

fn layer_algorithms(file: &[u8], ctx: &DetectContext) -> Vec<CompressionAlgorithm> {
    EnvelopePipeline::builtin()
        .open(file, ctx)
        .unwrap()
        .layers()
        .into_iter()
        .filter_map(|l| match l {
            LayerDescriptor::Compression { algorithm, .. } => Some(algorithm),
            _ => None,
        })
        .collect()
}

#[test]
fn java_uncompressed_level_dat() {
    let file = java_bytes();
    let ctx = DetectContext::for_file("level.dat");
    assert!(layer_algorithms(&file, &ctx).is_empty());
    let s = EnvelopePipeline::builtin().open(&file, &ctx).unwrap();
    assert_eq!(s.envelope().endianness, savelens_core::Endianness::Big);
    let out = exercise(&file, &ctx);
    // MUTF-8: the NUL is C0 80 and the emoji is a 6-byte surrogate pair.
    let needle = [b'S', b't', b'e', b'v', b'e', 0xc0, 0x80, 0xed, 0xa0, 0xbd, 0xed, 0xb4, 0xa5];
    assert!(out.windows(needle.len()).any(|w| w == needle));
}

#[test]
fn java_gzip_level_dat() {
    let raw = java_bytes();
    let gz = StreamParams::Gzip {
        header: vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0],
        level: 6,
    }
    .compress(&raw)
    .unwrap();
    let ctx = DetectContext::for_file("level.dat");
    assert_eq!(layer_algorithms(&gz, &ctx), vec![CompressionAlgorithm::Gzip]);
    let out = exercise(&gz, &ctx);
    assert_eq!(&out[..2], &[0x1f, 0x8b]);
    assert_eq!(layer_algorithms(&out, &ctx), vec![CompressionAlgorithm::Gzip]);
}

#[test]
fn bedrock_level_dat_with_header() {
    let file = bedrock_bytes();
    let ctx = DetectContext::for_file("level.dat");
    let s = EnvelopePipeline::builtin().open(&file, &ctx).unwrap();
    assert_eq!(s.envelope().endianness, savelens_core::Endianness::Little);
    assert!(s.envelope().notes.iter().any(|n| n.contains("storage version 10")), "{:?}", s.envelope().notes);
    let out = exercise(&file, &ctx);
    assert_eq!(&out[..4], &10i32.to_le_bytes());
    let declared = i32::from_le_bytes([out[4], out[5], out[6], out[7]]);
    assert_eq!(declared as usize, out.len() - 8);
    // Bedrock strings are plain UTF-8.
    assert!(out.windows(4).any(|w| w == "\u{1f525}".as_bytes()));
}

#[test]
fn bedrock_without_header_and_trailing_bytes() {
    let mut file = W::root("root", &level(), false);
    file.extend_from_slice(&[0, 0, 0, 0]);
    let ctx = DetectContext::default();
    let pipeline = EnvelopePipeline::builtin();
    let s = pipeline.open(&file, &ctx).unwrap();
    assert_eq!(s.codec_id(), "nbt");
    assert_eq!(s.envelope().endianness, savelens_core::Endianness::Little);
    assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
    assert!(s.envelope().notes.iter().any(|n| n.contains("\"root\"")));
    exercise(&file, &ctx);
}

#[test]
fn empty_list_types_preserved_after_edit() {
    let file = java_bytes();
    let ctx = DetectContext::default();
    let pipeline = EnvelopePipeline::builtin();
    let mut s = pipeline.open(&file, &ctx).unwrap();
    s.set(&p("Data.SpawnY"), Node::I16(70)).unwrap();
    let out = s.encode_verified(&pipeline, &ctx).unwrap();
    assert_eq!(out.len(), file.len());
    // Empty compound list keeps element type 10, empty End list keeps 0.
    let ender = [9u8, 0, 10, b'E', b'n', b'd', b'e', b'r', b'I', b't', b'e', b'm', b's', 10, 0, 0, 0, 0];
    assert!(out.windows(ender.len()).any(|w| w == ender));
    let effects_name = b"ActiveEffects";
    let pos = out.windows(effects_name.len()).position(|w| w == effects_name).unwrap();
    assert_eq!(&out[pos + effects_name.len()..pos + effects_name.len() + 5], &[0, 0, 0, 0, 0]);

    // Filling the End-typed list infers the element type; removing an entry works.
    let mut root = s.root();
    root.set(&p("Data.Player.ActiveEffects"), Node::Array(vec![Node::map_from([("Id", Node::I8(1))])]), false)
        .unwrap();
    root.remove(&p("Data.raining")).unwrap();
    root.set(&p("Data.Player.UUID"), Node::Array(vec![Node::I32(9), Node::I32(8)]), false).unwrap();
    s.apply_ast(&root).unwrap();
    let out = s.encode_verified(&pipeline, &ctx).unwrap();
    let re = pipeline.open(&out, &ctx).unwrap();
    assert!(re.root().get(&p("Data.raining")).is_none());
    assert_eq!(re.root().get(&p("Data.Player.ActiveEffects[0].Id")), Some(&Node::I8(1)));
    assert_eq!(re.root().get(&p("Data.Player.UUID")), Some(&Node::Array(vec![Node::I32(9), Node::I32(8)])));
    let pos = out.windows(effects_name.len()).position(|w| w == effects_name).unwrap();
    assert_eq!(out[pos + effects_name.len()], 10);
}

#[test]
fn truncated_input_never_panics() {
    let file = java_bytes();
    let pipeline = EnvelopePipeline::builtin();
    let ctx = DetectContext::default();
    for n in (0..file.len()).step_by(7) {
        let s = pipeline.open(&file[..n], &ctx).unwrap();
        assert_ne!(s.codec_id(), "nbt", "prefix {n}");
        assert!(pipeline.open_with_codec(&file[..n], &ctx, "nbt").is_err());
    }
    let bed = bedrock_bytes();
    for n in (0..bed.len()).step_by(5) {
        let _ = pipeline.open(&bed[..n], &ctx).unwrap();
    }
    // Corrupt bytes in the middle never panic either.
    let mut corrupt = file.clone();
    for i in (3..corrupt.len()).step_by(11) {
        corrupt[i] ^= 0xa5;
        let _ = pipeline.open(&corrupt, &ctx).unwrap();
    }
}
