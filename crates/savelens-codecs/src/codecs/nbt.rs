//! Lossless Named Binary Tag (Minecraft NBT) codec.
//!
//! Supports both flavours of the format:
//!
//! * **Java edition**: big-endian numbers, strings in Java "Modified UTF-8"
//!   (NUL as `C0 80`, supplementary characters as two 3-byte surrogates).
//!   Files are usually gzip-compressed; the compression layer of the
//!   pipeline peels that before this codec sees the payload.
//! * **Bedrock edition**: little-endian numbers and UTF-8 strings, optionally
//!   preceded by the 8-byte `level.dat` header (`i32` storage version +
//!   `i32` payload length). The header is preserved and its length field is
//!   recomputed when an edit changes the payload size.
//!
//! The document keeps every layout detail needed for a bit-exact rewrite:
//! the raw bytes of every string (re-encoded only when the value is edited),
//! the element type id of empty lists, the root name, duplicated compound
//! names and any trailing bytes after the root compound.

use savelens_core::{bit_equal, Endianness, Node, NodeMap, NodePath, PathSeg};

use crate::document::{coerce_like, Capabilities, Confidence, DetectContext, Document, StructuralCodec};
use crate::error::{CodecError, CodecResult};
use crate::io::ByteReader;

const CODEC: &str = "nbt";
const MAX_DEPTH: usize = 512;

/// Tag type ids.
pub mod tag {
    pub const END: u8 = 0;
    pub const BYTE: u8 = 1;
    pub const SHORT: u8 = 2;
    pub const INT: u8 = 3;
    pub const LONG: u8 = 4;
    pub const FLOAT: u8 = 5;
    pub const DOUBLE: u8 = 6;
    pub const BYTE_ARRAY: u8 = 7;
    pub const STRING: u8 = 8;
    pub const LIST: u8 = 9;
    pub const COMPOUND: u8 = 10;
    pub const INT_ARRAY: u8 = 11;
    pub const LONG_ARRAY: u8 = 12;
}

/// Human name of a tag id.
pub fn tag_name(id: u8) -> &'static str {
    match id {
        tag::END => "TAG_End",
        tag::BYTE => "TAG_Byte",
        tag::SHORT => "TAG_Short",
        tag::INT => "TAG_Int",
        tag::LONG => "TAG_Long",
        tag::FLOAT => "TAG_Float",
        tag::DOUBLE => "TAG_Double",
        tag::BYTE_ARRAY => "TAG_Byte_Array",
        tag::STRING => "TAG_String",
        tag::LIST => "TAG_List",
        tag::COMPOUND => "TAG_Compound",
        tag::INT_ARRAY => "TAG_Int_Array",
        tag::LONG_ARRAY => "TAG_Long_Array",
        _ => "unknown tag",
    }
}

/// NBT dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NbtFlavor {
    /// Java edition: big-endian, Modified UTF-8 strings.
    Java,
    /// Bedrock edition: little-endian, UTF-8 strings.
    Bedrock,
}

impl NbtFlavor {
    fn big_endian(self) -> bool {
        self == NbtFlavor::Java
    }
}

/// The 8-byte header of Bedrock `level.dat` files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BedrockHeader {
    /// Storage format version.
    pub storage_version: i32,
    /// Declared payload length as found in the file.
    pub declared_len: i32,
}

// ---------------------------------------------------------------------------
// Modified UTF-8
// ---------------------------------------------------------------------------

/// Decodes Java Modified UTF-8. Returns `None` for invalid sequences
/// (including 4-byte UTF-8 sequences, which MUTF-8 never contains).
pub fn mutf8_decode(raw: &[u8]) -> Option<String> {
    let mut units: Vec<u16> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let b = raw[i];
        if b < 0x80 {
            units.push(b as u16);
            i += 1;
        } else if b & 0xe0 == 0xc0 {
            let b2 = *raw.get(i + 1)?;
            if b2 & 0xc0 != 0x80 {
                return None;
            }
            units.push(((b as u16 & 0x1f) << 6) | (b2 as u16 & 0x3f));
            i += 2;
        } else if b & 0xf0 == 0xe0 {
            let b2 = *raw.get(i + 1)?;
            let b3 = *raw.get(i + 2)?;
            if b2 & 0xc0 != 0x80 || b3 & 0xc0 != 0x80 {
                return None;
            }
            units.push(((b as u16 & 0x0f) << 12) | ((b2 as u16 & 0x3f) << 6) | (b3 as u16 & 0x3f));
            i += 3;
        } else {
            return None;
        }
    }
    String::from_utf16(&units).ok()
}

/// Encodes a string as Java Modified UTF-8.
pub fn mutf8_encode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() + 4);
    for unit in s.encode_utf16() {
        match unit {
            1..=0x7f => out.push(unit as u8),
            0 | 0x80..=0x7ff => {
                out.push(0xc0 | (unit >> 6) as u8);
                out.push(0x80 | (unit & 0x3f) as u8);
            }
            _ => {
                out.push(0xe0 | (unit >> 12) as u8);
                out.push(0x80 | ((unit >> 6) & 0x3f) as u8);
                out.push(0x80 | (unit & 0x3f) as u8);
            }
        }
    }
    out
}

fn decode_text(raw: &[u8], flavor: NbtFlavor) -> String {
    let utf8 = || std::str::from_utf8(raw).ok().map(str::to_string);
    let decoded = match flavor {
        NbtFlavor::Java => mutf8_decode(raw).or_else(utf8),
        NbtFlavor::Bedrock => utf8().or_else(|| mutf8_decode(raw)),
    };
    decoded.unwrap_or_else(|| String::from_utf8_lossy(raw).into_owned())
}

// ---------------------------------------------------------------------------
// Document model
// ---------------------------------------------------------------------------

/// A string with its original encoded bytes.
#[derive(Debug, Clone)]
struct NStr {
    raw: Vec<u8>,
    value: String,
}

impl NStr {
    fn new(value: &str, flavor: NbtFlavor, path: &NodePath) -> CodecResult<NStr> {
        let raw = match flavor {
            NbtFlavor::Java => mutf8_encode(value),
            NbtFlavor::Bedrock => value.as_bytes().to_vec(),
        };
        if raw.len() > u16::MAX as usize {
            return Err(CodecError::unencodable(
                CODEC,
                path,
                format!("NBT strings are limited to 65535 encoded bytes, this one needs {}", raw.len()),
            ));
        }
        Ok(NStr { raw, value: value.to_string() })
    }
}

type Entry = (NStr, Tag);

#[derive(Debug, Clone)]
enum Tag {
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    ByteArray(Vec<u8>),
    String(NStr),
    List { elem: u8, items: Vec<Tag> },
    Compound(Vec<Entry>),
    IntArray(Vec<i32>),
    LongArray(Vec<i64>),
}

impl Tag {
    fn id(&self) -> u8 {
        match self {
            Tag::Byte(_) => tag::BYTE,
            Tag::Short(_) => tag::SHORT,
            Tag::Int(_) => tag::INT,
            Tag::Long(_) => tag::LONG,
            Tag::Float(_) => tag::FLOAT,
            Tag::Double(_) => tag::DOUBLE,
            Tag::ByteArray(_) => tag::BYTE_ARRAY,
            Tag::String(_) => tag::STRING,
            Tag::List { .. } => tag::LIST,
            Tag::Compound(_) => tag::COMPOUND,
            Tag::IntArray(_) => tag::INT_ARRAY,
            Tag::LongArray(_) => tag::LONG_ARRAY,
        }
    }
}

/// Minimum encoded payload size of a tag id (used to bound list counts).
fn min_payload_size(id: u8) -> usize {
    match id {
        tag::BYTE | tag::COMPOUND => 1,
        tag::SHORT | tag::STRING => 2,
        tag::INT | tag::FLOAT | tag::BYTE_ARRAY | tag::INT_ARRAY | tag::LONG_ARRAY => 4,
        tag::LONG | tag::DOUBLE => 8,
        tag::LIST => 5,
        _ => 1,
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

struct Reader<'a> {
    r: ByteReader<'a>,
    flavor: NbtFlavor,
    depth: usize,
}

macro_rules! endian_read {
    ($name:ident, $t:ty, $be:ident, $le:ident) => {
        fn $name(&mut self, ctx: &str) -> CodecResult<$t> {
            if self.flavor.big_endian() {
                self.r.$be(ctx)
            } else {
                self.r.$le(ctx)
            }
        }
    };
}

impl<'a> Reader<'a> {
    endian_read!(u16, u16, u16_be, u16_le);
    endian_read!(i16, i16, i16_be, i16_le);
    endian_read!(i32, i32, i32_be, i32_le);
    endian_read!(i64, i64, i64_be, i64_le);
    endian_read!(f32, f32, f32_be, f32_le);
    endian_read!(f64, f64, f64_be, f64_le);

    fn string(&mut self) -> CodecResult<NStr> {
        let n = self.u16("string length")? as usize;
        let raw = self.r.take(n, "string bytes")?.to_vec();
        let value = decode_text(&raw, self.flavor);
        Ok(NStr { raw, value })
    }

    fn count(&mut self, ctx: &str, elem_size: usize) -> CodecResult<usize> {
        let n = self.i32(ctx)?;
        if n < 0 {
            return Err(self.r.malformed(format!("{ctx}: negative length {n}")));
        }
        let n = n as usize;
        self.r.check_count(n, elem_size, ctx)?;
        Ok(n)
    }

    fn enter(&mut self) -> CodecResult<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.r.malformed("nesting too deep"));
        }
        Ok(())
    }

    /// Reads a payload. Lists and compounds recurse; every other tag is read
    /// by [`Reader::leaf_payload`], kept out of line so the recursive frame
    /// stays small.
    fn payload(&mut self, id: u8) -> CodecResult<Tag> {
        match id {
            tag::LIST => self.list(),
            tag::COMPOUND => {
                self.enter()?;
                let entries = self.compound_body()?;
                self.depth -= 1;
                Ok(Tag::Compound(entries))
            }
            _ => self.leaf_payload(id),
        }
    }

    fn list(&mut self) -> CodecResult<Tag> {
        self.enter()?;
        let elem = self.r.u8("TAG_List element type")?;
        if elem > tag::LONG_ARRAY {
            return Err(self.r.malformed(format!("unknown list element tag id {elem}")));
        }
        let n = self.count("TAG_List length", min_payload_size(elem))?;
        if elem == tag::END && n > 0 {
            return Err(self.r.malformed(format!("list of TAG_End with {n} elements")));
        }
        let mut items = Vec::with_capacity(n);
        for _ in 0..n {
            items.push(self.payload(elem)?);
        }
        self.depth -= 1;
        Ok(Tag::List { elem, items })
    }

    #[inline(never)]
    fn leaf_payload(&mut self, id: u8) -> CodecResult<Tag> {
        Ok(match id {
            tag::BYTE => Tag::Byte(self.r.i8("TAG_Byte")?),
            tag::SHORT => Tag::Short(self.i16("TAG_Short")?),
            tag::INT => Tag::Int(self.i32("TAG_Int")?),
            tag::LONG => Tag::Long(self.i64("TAG_Long")?),
            tag::FLOAT => Tag::Float(self.f32("TAG_Float")?),
            tag::DOUBLE => Tag::Double(self.f64("TAG_Double")?),
            tag::BYTE_ARRAY => {
                let n = self.count("TAG_Byte_Array length", 1)?;
                Tag::ByteArray(self.r.take(n, "TAG_Byte_Array data")?.to_vec())
            }
            tag::STRING => Tag::String(self.string()?),
            tag::INT_ARRAY => {
                let n = self.count("TAG_Int_Array length", 4)?;
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    v.push(self.i32("TAG_Int_Array element")?);
                }
                Tag::IntArray(v)
            }
            tag::LONG_ARRAY => {
                let n = self.count("TAG_Long_Array length", 8)?;
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    v.push(self.i64("TAG_Long_Array element")?);
                }
                Tag::LongArray(v)
            }
            other => return Err(self.r.malformed(format!("unknown tag id {other}"))),
        })
    }

    fn compound_body(&mut self) -> CodecResult<Vec<Entry>> {
        let mut entries = Vec::new();
        loop {
            let id = self.r.u8("compound entry tag id")?;
            if id == tag::END {
                return Ok(entries);
            }
            if id > tag::LONG_ARRAY {
                return Err(self.r.malformed(format!("unknown tag id {id} in compound")));
            }
            let name = self.string()?;
            let value = self.payload(id)?;
            entries.push((name, value));
        }
    }
}

/// Parses a root compound starting at `data[0]`; returns the name, entries and consumed size.
fn parse_root(data: &[u8], flavor: NbtFlavor) -> CodecResult<(NStr, Vec<Entry>, usize)> {
    let mut rd = Reader { r: ByteReader::new(data, CODEC), flavor, depth: 0 };
    let id = rd.r.u8("root tag id")?;
    if id != tag::COMPOUND {
        return Err(CodecError::malformed(
            CODEC,
            0,
            format!("root tag is {} ({id}), expected TAG_Compound", tag_name(id)),
        ));
    }
    let name = rd.string()?;
    rd.enter()?;
    let entries = rd.compound_body()?;
    Ok((name, entries, rd.r.pos()))
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

struct Writer {
    out: Vec<u8>,
    big: bool,
}

macro_rules! endian_write {
    ($name:ident, $t:ty) => {
        fn $name(&mut self, v: $t) {
            if self.big {
                self.out.extend_from_slice(&v.to_be_bytes());
            } else {
                self.out.extend_from_slice(&v.to_le_bytes());
            }
        }
    };
}

impl Writer {
    endian_write!(u16, u16);
    endian_write!(i16, i16);
    endian_write!(i32, i32);
    endian_write!(i64, i64);
    endian_write!(f32, f32);
    endian_write!(f64, f64);

    fn len(&mut self, n: usize, what: &str) -> CodecResult<()> {
        let v = i32::try_from(n)
            .map_err(|_| CodecError::unsupported(CODEC, format!("{what} with {n} elements exceeds i32")))?;
        self.i32(v);
        Ok(())
    }

    fn string(&mut self, s: &NStr) -> CodecResult<()> {
        let n = u16::try_from(s.raw.len())
            .map_err(|_| CodecError::unsupported(CODEC, format!("string of {} bytes exceeds 65535", s.raw.len())))?;
        self.u16(n);
        self.out.extend_from_slice(&s.raw);
        Ok(())
    }

    fn payload(&mut self, t: &Tag) -> CodecResult<()> {
        match t {
            Tag::Byte(v) => self.out.push(*v as u8),
            Tag::Short(v) => self.i16(*v),
            Tag::Int(v) => self.i32(*v),
            Tag::Long(v) => self.i64(*v),
            Tag::Float(v) => self.f32(*v),
            Tag::Double(v) => self.f64(*v),
            Tag::ByteArray(b) => {
                self.len(b.len(), "TAG_Byte_Array")?;
                self.out.extend_from_slice(b);
            }
            Tag::String(s) => self.string(s)?,
            Tag::List { elem, items } => {
                self.out.push(*elem);
                self.len(items.len(), "TAG_List")?;
                for it in items {
                    self.payload(it)?;
                }
            }
            Tag::Compound(entries) => self.compound_body(entries)?,
            Tag::IntArray(v) => {
                self.len(v.len(), "TAG_Int_Array")?;
                for x in v {
                    self.i32(*x);
                }
            }
            Tag::LongArray(v) => {
                self.len(v.len(), "TAG_Long_Array")?;
                for x in v {
                    self.i64(*x);
                }
            }
        }
        Ok(())
    }

    fn compound_body(&mut self, entries: &[Entry]) -> CodecResult<()> {
        for (name, value) in entries {
            self.out.push(value.id());
            self.string(name)?;
            self.payload(value)?;
        }
        self.out.push(tag::END);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Projection
// ---------------------------------------------------------------------------

/// AST keys of a compound, de-duplicating repeated names as `name#2`, `name#3`…
fn ast_keys(entries: &[Entry]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    entries
        .iter()
        .map(|(name, _)| {
            let mut k = name.value.clone();
            if !seen.insert(k.clone()) {
                let mut n = 2;
                loop {
                    let cand = format!("{}#{n}", name.value);
                    if seen.insert(cand.clone()) {
                        k = cand;
                        break;
                    }
                    n += 1;
                }
            }
            k
        })
        .collect()
}

fn project_entries(entries: &[Entry]) -> Node {
    let keys = ast_keys(entries);
    let mut m = NodeMap::with_capacity(entries.len());
    for (k, (_, v)) in keys.into_iter().zip(entries) {
        m.insert(k, project(v));
    }
    Node::Map(m)
}

fn project(t: &Tag) -> Node {
    match t {
        Tag::Byte(v) => Node::I8(*v),
        Tag::Short(v) => Node::I16(*v),
        Tag::Int(v) => Node::I32(*v),
        Tag::Long(v) => Node::I64(*v),
        Tag::Float(v) => Node::F32(*v),
        Tag::Double(v) => Node::F64(*v),
        Tag::ByteArray(b) => Node::Bytes(b.clone()),
        Tag::String(s) => Node::String(s.value.clone()),
        Tag::List { items, .. } => Node::Array(items.iter().map(project).collect()),
        Tag::Compound(entries) => project_entries(entries),
        Tag::IntArray(v) => Node::Array(v.iter().map(|x| Node::I32(*x)).collect()),
        Tag::LongArray(v) => Node::Array(v.iter().map(|x| Node::I64(*x)).collect()),
    }
}

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

/// Tag id inferred from the kind of a new node.
fn infer_id(node: &Node, path: &NodePath) -> CodecResult<u8> {
    Ok(match node {
        Node::Bool(_) | Node::I8(_) => tag::BYTE,
        Node::I16(_) => tag::SHORT,
        Node::I32(_) => tag::INT,
        Node::I64(_) => tag::LONG,
        Node::F32(_) => tag::FLOAT,
        Node::F64(_) => tag::DOUBLE,
        Node::String(_) => tag::STRING,
        Node::Bytes(_) | Node::RawRegion(_) => tag::BYTE_ARRAY,
        Node::Map(_) => tag::COMPOUND,
        Node::Array(_) => tag::LIST,
        other => {
            return Err(CodecError::unencodable(
                CODEC,
                path,
                format!(
                    "NBT has no tag for {} values (use i8/i16/i32/i64/f32/f64/string/bytes/array/map)",
                    other.kind().name()
                ),
            ))
        }
    })
}

/// Projection prototype of a scalar tag id.
fn scalar_proto(id: u8) -> Option<Node> {
    Some(match id {
        tag::BYTE => Node::I8(0),
        tag::SHORT => Node::I16(0),
        tag::INT => Node::I32(0),
        tag::LONG => Node::I64(0),
        tag::FLOAT => Node::F32(0.0),
        tag::DOUBLE => Node::F64(0.0),
        _ => return None,
    })
}

fn scalar_tag(n: &Node, path: &NodePath) -> CodecResult<Tag> {
    Ok(match n {
        Node::I8(v) => Tag::Byte(*v),
        Node::I16(v) => Tag::Short(*v),
        Node::I32(v) => Tag::Int(*v),
        Node::I64(v) => Tag::Long(*v),
        Node::F32(v) => Tag::Float(*v),
        Node::F64(v) => Tag::Double(*v),
        other => return Err(CodecError::type_mismatch(CODEC, path, "NBT number", other.kind().name())),
    })
}

fn as_array<'n>(node: &'n Node, path: &NodePath, expected: &str) -> CodecResult<&'n [Node]> {
    match node {
        Node::Array(a) => Ok(a),
        other => Err(CodecError::type_mismatch(CODEC, path, expected, other.kind().name())),
    }
}

struct Editor {
    flavor: NbtFlavor,
}

impl Editor {
    fn coerce_vec<T>(
        &self,
        items: &[Node],
        path: &mut NodePath,
        proto: &Node,
        f: impl Fn(&Node) -> Option<T>,
    ) -> CodecResult<Vec<T>> {
        let mut out = Vec::with_capacity(items.len());
        for (i, it) in items.iter().enumerate() {
            path.push(PathSeg::Index(i));
            let v = coerce_like(CODEC, path, proto, it)?;
            let x =
                f(&v).ok_or_else(|| CodecError::type_mismatch(CODEC, &*path, proto.kind().name(), it.kind().name()))?;
            out.push(x);
            path.pop();
        }
        Ok(out)
    }

    /// Builds a new tag for `node`. `expected` forces the tag id (list
    /// element type). Only lists and compounds recurse; leaves are built out
    /// of line so that each nesting level costs little stack.
    fn build(&self, expected: Option<u8>, node: &Node, path: &mut NodePath) -> CodecResult<Tag> {
        let id = match expected {
            Some(id) if id != tag::END => id,
            _ => infer_id(node, path)?,
        };
        match id {
            tag::LIST => self.build_list(node, path),
            tag::COMPOUND => self.build_compound(node, path),
            _ => self.build_leaf(id, node, path),
        }
    }

    #[inline(never)]
    fn build_list(&self, node: &Node, path: &mut NodePath) -> CodecResult<Tag> {
        let a = as_array(node, path, "array")?;
        let elem = match a.first() {
            Some(first) => infer_id(first, path)?,
            None => tag::END,
        };
        let mut items = Vec::with_capacity(a.len());
        for (i, child) in a.iter().enumerate() {
            path.push(PathSeg::Index(i));
            items.push(self.build(Some(elem), child, path)?);
            path.pop();
        }
        Ok(Tag::List { elem, items })
    }

    #[inline(never)]
    fn build_compound(&self, node: &Node, path: &mut NodePath) -> CodecResult<Tag> {
        let Node::Map(m) = node else {
            return Err(CodecError::type_mismatch(CODEC, &*path, "map", node.kind().name()));
        };
        let mut entries = Vec::with_capacity(m.len());
        for (k, child) in m {
            path.push(PathSeg::Key(k.clone()));
            let t = self.build(None, child, path)?;
            entries.push((NStr::new(k, self.flavor, path)?, t));
            path.pop();
        }
        Ok(Tag::Compound(entries))
    }

    #[inline(never)]
    fn build_leaf(&self, id: u8, node: &Node, path: &mut NodePath) -> CodecResult<Tag> {
        if let Some(proto) = scalar_proto(id) {
            let v = coerce_like(CODEC, path, &proto, node)?;
            return scalar_tag(&v, path);
        }
        Ok(match id {
            tag::BYTE_ARRAY => match coerce_like(CODEC, path, &Node::Bytes(Vec::new()), node)? {
                Node::Bytes(b) => Tag::ByteArray(b),
                other => return Err(CodecError::type_mismatch(CODEC, &*path, "bytes", other.kind().name())),
            },
            tag::STRING => match node {
                Node::String(s) => Tag::String(NStr::new(s, self.flavor, path)?),
                other => return Err(CodecError::type_mismatch(CODEC, &*path, "string", other.kind().name())),
            },
            tag::INT_ARRAY => {
                let a = as_array(node, path, "array of i32")?;
                Tag::IntArray(self.coerce_vec(a, path, &Node::I32(0), |n| match n {
                    Node::I32(v) => Some(*v),
                    _ => None,
                })?)
            }
            tag::LONG_ARRAY => {
                let a = as_array(node, path, "array of i64")?;
                Tag::LongArray(self.coerce_vec(a, path, &Node::I64(0), |n| match n {
                    Node::I64(v) => Some(*v),
                    _ => None,
                })?)
            }
            other => {
                return Err(CodecError::unencodable(
                    CODEC,
                    &*path,
                    format!("cannot build a {} tag here", tag_name(other)),
                ));
            }
        })
    }

    fn reconcile_entries(&self, entries: &mut Vec<Entry>, m: &NodeMap, path: &mut NodePath) -> CodecResult<()> {
        let keys = ast_keys(entries);
        let mut old: Vec<Option<Entry>> = std::mem::take(entries).into_iter().map(Some).collect();
        let mut out = Vec::with_capacity(m.len());
        for (k, child) in m {
            path.push(PathSeg::Key(k.clone()));
            match keys.iter().position(|x| x == k).and_then(|i| old.get_mut(i).and_then(Option::take)) {
                Some((name, mut t)) => {
                    self.reconcile(&mut t, child, path)?;
                    out.push((name, t));
                }
                None => {
                    let t = self.build(None, child, path)?;
                    out.push((NStr::new(k, self.flavor, path)?, t));
                }
            }
            path.pop();
        }
        *entries = out;
        Ok(())
    }

    /// Applies an edited node to a tag. Only lists and compounds recurse.
    fn reconcile(&self, t: &mut Tag, node: &Node, path: &mut NodePath) -> CodecResult<()> {
        if bit_equal(&project(t), node) {
            return Ok(());
        }
        match t {
            Tag::Compound(entries) => self.reconcile_compound(entries, node, path),
            Tag::List { elem, items } => self.reconcile_list(elem, items, node, path),
            leaf => self.reconcile_leaf(leaf, node, path),
        }
    }

    #[inline(never)]
    fn reconcile_compound(&self, entries: &mut Vec<Entry>, node: &Node, path: &mut NodePath) -> CodecResult<()> {
        match node {
            Node::Map(m) => self.reconcile_entries(entries, m, path),
            other => Err(CodecError::type_mismatch(CODEC, &*path, "map (TAG_Compound)", other.kind().name())),
        }
    }

    #[inline(never)]
    fn reconcile_list(&self, elem: &mut u8, items: &mut Vec<Tag>, node: &Node, path: &mut NodePath) -> CodecResult<()> {
        let a = as_array(node, path, "array (TAG_List)")?;
        items.truncate(a.len());
        if items.is_empty() && *elem == tag::END {
            if let Some(first) = a.first() {
                *elem = infer_id(first, path)?;
            }
        }
        for (i, child) in a.iter().enumerate() {
            path.push(PathSeg::Index(i));
            if i < items.len() {
                self.reconcile(&mut items[i], child, path)?;
            } else {
                items.push(self.build(Some(*elem), child, path)?);
            }
            path.pop();
        }
        Ok(())
    }

    #[inline(never)]
    fn reconcile_leaf(&self, t: &mut Tag, node: &Node, path: &mut NodePath) -> CodecResult<()> {
        match t {
            Tag::String(s) => match node {
                Node::String(v) => {
                    if *v != s.value {
                        *s = NStr::new(v, self.flavor, path)?;
                    }
                    Ok(())
                }
                other => Err(CodecError::type_mismatch(CODEC, &*path, "string (TAG_String)", other.kind().name())),
            },
            Tag::ByteArray(_) | Tag::IntArray(_) | Tag::LongArray(_) => {
                *t = self.build_leaf(t.id(), node, path)?;
                Ok(())
            }
            Tag::Compound(_) | Tag::List { .. } => {
                Err(CodecError::structure(CODEC, &*path, "container reached the leaf editor"))
            }
            _ => {
                let v = coerce_like(CODEC, path, &project(t), node)?;
                *t = scalar_tag(&v, path)?;
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Document
// ---------------------------------------------------------------------------

/// A parsed NBT file.
#[derive(Debug, Clone)]
pub struct NbtDocument {
    flavor: NbtFlavor,
    header: Option<BedrockHeader>,
    root_name: NStr,
    root: Vec<Entry>,
    trailing: Vec<u8>,
    /// Encoded size of the root compound when the file was read.
    original_root_len: usize,
}

impl NbtDocument {
    fn parse_as(data: &[u8], flavor: NbtFlavor, header: Option<BedrockHeader>) -> CodecResult<NbtDocument> {
        let skip = if header.is_some() { 8 } else { 0 };
        let body = data.get(skip..).ok_or_else(|| CodecError::malformed(CODEC, 0, "input shorter than header"))?;
        let (root_name, root, used) = parse_root(body, flavor)?;
        Ok(NbtDocument { flavor, header, root_name, root, trailing: body[used..].to_vec(), original_root_len: used })
    }

    /// Parses NBT bytes, detecting Java (big-endian) vs Bedrock (little-endian,
    /// with or without the 8-byte `level.dat` header). The candidate that
    /// parses completely with the fewest trailing bytes wins; ties prefer the
    /// Bedrock header, then Java.
    pub fn parse(data: &[u8]) -> CodecResult<NbtDocument> {
        let mut candidates: Vec<(NbtFlavor, Option<BedrockHeader>)> = Vec::new();
        if data.len() >= 11 && data[8] == tag::COMPOUND {
            let mut v = [0u8; 4];
            v.copy_from_slice(&data[0..4]);
            let storage_version = i32::from_le_bytes(v);
            v.copy_from_slice(&data[4..8]);
            let declared_len = i32::from_le_bytes(v);
            if declared_len >= 0 && declared_len as usize <= data.len() - 8 {
                candidates.push((NbtFlavor::Bedrock, Some(BedrockHeader { storage_version, declared_len })));
            }
        }
        candidates.push((NbtFlavor::Java, None));
        candidates.push((NbtFlavor::Bedrock, None));

        let mut best: Option<NbtDocument> = None;
        let mut first_err: Option<CodecError> = None;
        for (flavor, header) in candidates {
            match Self::parse_as(data, flavor, header) {
                Ok(doc) => {
                    let better = match &best {
                        None => true,
                        Some(b) => doc.trailing.len() < b.trailing.len(),
                    };
                    if better {
                        best = Some(doc);
                    }
                }
                Err(e) => {
                    if first_err.is_none() || (flavor == NbtFlavor::Java && header.is_none()) {
                        first_err = Some(e);
                    }
                }
            }
        }
        match (best, first_err) {
            (Some(doc), _) => Ok(doc),
            (None, Some(e)) => Err(e),
            (None, None) => Err(CodecError::malformed(CODEC, 0, "empty input")),
        }
    }

    /// Dialect of the file.
    pub fn flavor(&self) -> NbtFlavor {
        self.flavor
    }

    /// Bedrock `level.dat` header, if present.
    pub fn header(&self) -> Option<BedrockHeader> {
        self.header
    }

    /// Name of the root compound.
    pub fn root_name(&self) -> &str {
        &self.root_name.value
    }

    /// Bytes after the root compound, kept verbatim.
    pub fn trailing(&self) -> &[u8] {
        &self.trailing
    }

    fn encode_root(&self) -> CodecResult<Vec<u8>> {
        let mut w = Writer { out: Vec::with_capacity(self.original_root_len + 16), big: self.flavor.big_endian() };
        w.out.push(tag::COMPOUND);
        w.string(&self.root_name)?;
        w.compound_body(&self.root)?;
        Ok(w.out)
    }
}

impl Document for NbtDocument {
    fn codec_id(&self) -> &'static str {
        CODEC
    }

    fn to_ast(&self) -> Node {
        project_entries(&self.root)
    }

    fn apply_ast(&mut self, root: &Node) -> CodecResult<()> {
        let Node::Map(m) = root else {
            return Err(CodecError::type_mismatch(CODEC, "$", "map (root TAG_Compound)", root.kind().name()));
        };
        let editor = Editor { flavor: self.flavor };
        let mut work = self.root.clone();
        editor.reconcile_entries(&mut work, m, &mut NodePath::root())?;
        let previous = std::mem::replace(&mut self.root, work);
        // Make sure the header length can still be represented.
        if self.header.is_some() {
            if let Err(e) = self.encode() {
                self.root = previous;
                return Err(e);
            }
        }
        Ok(())
    }

    fn encode(&self) -> CodecResult<Vec<u8>> {
        let body = self.encode_root()?;
        let mut out = Vec::with_capacity(body.len() + self.trailing.len() + 8);
        if let Some(h) = self.header {
            let delta = body.len() as i64 - self.original_root_len as i64;
            let declared = i32::try_from(h.declared_len as i64 + delta).map_err(|_| {
                CodecError::unsupported(CODEC, "payload too large for the Bedrock level.dat header length")
            })?;
            out.extend_from_slice(&h.storage_version.to_le_bytes());
            out.extend_from_slice(&declared.to_le_bytes());
        }
        out.extend_from_slice(&body);
        out.extend_from_slice(&self.trailing);
        Ok(out)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::FULL
    }

    fn endianness(&self) -> Endianness {
        if self.flavor.big_endian() {
            Endianness::Big
        } else {
            Endianness::Little
        }
    }

    fn notes(&self) -> Vec<String> {
        let mut n = vec![match self.flavor {
            NbtFlavor::Java => "NBT Java edition (big-endian, Modified UTF-8 strings)".to_string(),
            NbtFlavor::Bedrock => "NBT Bedrock edition (little-endian, UTF-8 strings)".to_string(),
        }];
        n.push(format!("root compound name: {:?}", self.root_name.value));
        if let Some(h) = self.header {
            n.push(format!(
                "Bedrock level.dat header: storage version {}, declared payload length {}",
                h.storage_version, h.declared_len
            ));
        }
        if !self.trailing.is_empty() {
            n.push(format!("{} trailing bytes after the root compound kept verbatim", self.trailing.len()));
        }
        n
    }

    fn clone_box(&self) -> Box<dyn Document> {
        Box::new(self.clone())
    }
}

/// The NBT codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct NbtCodec;

impl StructuralCodec for NbtCodec {
    fn id(&self) -> &'static str {
        CODEC
    }

    fn name(&self) -> &'static str {
        "Named Binary Tag (Minecraft NBT)"
    }

    fn detect(&self, bytes: &[u8], _ctx: &DetectContext) -> Confidence {
        let plausible_start = bytes.first() == Some(&tag::COMPOUND) || bytes.get(8) == Some(&tag::COMPOUND);
        if !plausible_start {
            return 0;
        }
        match NbtDocument::parse(bytes) {
            Ok(doc) if doc.trailing.iter().all(|&b| b == 0) => 90,
            // Some trailing data: only plausible when the tree has content.
            Ok(doc) if !doc.root.is_empty() && doc.trailing.len() <= doc.original_root_len => 50,
            _ => 0,
        }
    }

    fn decode(&self, bytes: &[u8], _ctx: &DetectContext) -> CodecResult<Box<dyn Document>> {
        Ok(Box::new(NbtDocument::parse(bytes)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal big-endian sample: {"hello world": {name: "Bananrama"}}.
    fn hello_world() -> Vec<u8> {
        let mut v = vec![0x0a, 0x00, 0x0b];
        v.extend_from_slice(b"hello world");
        v.extend_from_slice(&[0x08, 0x00, 0x04]);
        v.extend_from_slice(b"name");
        v.extend_from_slice(&[0x00, 0x09]);
        v.extend_from_slice(b"Bananrama");
        v.push(0x00);
        v
    }

    #[test]
    fn mutf8_roundtrip() {
        let s = "a\u{0}b\u{e9}\u{20ac}\u{1f600}";
        let enc = mutf8_encode(s);
        assert_eq!(&enc[1..3], &[0xc0, 0x80]);
        // Emoji as two 3-byte surrogates.
        assert_eq!(enc.len(), 1 + 2 + 1 + 2 + 3 + 6);
        assert_eq!(mutf8_decode(&enc).as_deref(), Some(s));
        // 4-byte UTF-8 is not MUTF-8.
        assert!(mutf8_decode("\u{1f600}".as_bytes()).is_none());
        assert!(mutf8_decode(&[0xe0, 0x80]).is_none());
    }

    #[test]
    fn hello_world_roundtrip_and_edit() {
        let data = hello_world();
        let mut doc = NbtDocument::parse(&data).unwrap();
        assert_eq!(doc.flavor(), NbtFlavor::Java);
        assert_eq!(doc.root_name(), "hello world");
        assert_eq!(doc.encode().unwrap(), data);
        assert_eq!(doc.to_ast(), Node::map_from([("name", Node::String("Bananrama".into()))]));
        assert_eq!(NbtCodec.detect(&data, &DetectContext::default()), 90);

        let edited = Node::map_from([
            ("name", Node::String("Steve\u{0}".into())),
            ("hp", Node::F32(20.0)),
            ("list", Node::Array(vec![Node::I16(1), Node::I64(2)])),
        ]);
        doc.apply_ast(&edited).unwrap();
        let out = doc.encode().unwrap();
        let re = NbtDocument::parse(&out).unwrap();
        assert_eq!(
            re.to_ast(),
            Node::map_from([
                ("name", Node::String("Steve\u{0}".into())),
                ("hp", Node::F32(20.0)),
                ("list", Node::Array(vec![Node::I16(1), Node::I16(2)])),
            ])
        );
        // NUL encoded as C0 80.
        assert!(out.windows(7).any(|w| w == b"Steve\xc0\x80"));
    }

    #[test]
    fn little_endian_with_header() {
        let mut body = vec![0x0a, 0x00, 0x00, 0x03, 0x03, 0x00];
        body.extend_from_slice(b"abc");
        body.extend_from_slice(&7i32.to_le_bytes());
        body.push(0);
        let mut data = 10i32.to_le_bytes().to_vec();
        data.extend_from_slice(&(body.len() as i32).to_le_bytes());
        data.extend_from_slice(&body);
        let mut doc = NbtDocument::parse(&data).unwrap();
        assert_eq!(doc.flavor(), NbtFlavor::Bedrock);
        assert!(doc.header().is_some());
        assert_eq!(doc.encode().unwrap(), data);
        assert_eq!(doc.endianness(), Endianness::Little);
        doc.apply_ast(&Node::map_from([("abc", Node::I32(9)), ("name", Node::String("x".into()))])).unwrap();
        let out = doc.encode().unwrap();
        let declared = i32::from_le_bytes([out[4], out[5], out[6], out[7]]);
        assert_eq!(declared as usize, out.len() - 8);
        let re = NbtDocument::parse(&out).unwrap();
        assert_eq!(re.to_ast().get(&NodePath::parse("abc").unwrap()), Some(&Node::I32(9)));
    }

    #[test]
    fn empty_list_type_and_trailing_preserved() {
        // {a: List<Compound>[], b: List<End>[]} + trailing bytes.
        let mut data = vec![0x0a, 0x00, 0x00];
        data.extend_from_slice(&[0x09, 0x00, 0x01, b'a', 0x0a, 0, 0, 0, 0]);
        data.extend_from_slice(&[0x09, 0x00, 0x01, b'b', 0x00, 0, 0, 0, 0]);
        data.push(0);
        let mut full = data.clone();
        full.extend_from_slice(&[0, 0, 0, 0]);
        let mut doc = NbtDocument::parse(&full).unwrap();
        assert_eq!(doc.encode().unwrap(), full);
        assert_eq!(doc.trailing().len(), 4);
        // Fill the End-typed list: element type inferred from the node.
        doc.apply_ast(&Node::map_from([("a", Node::Array(vec![])), ("b", Node::Array(vec![Node::F64(1.5)]))])).unwrap();
        let out = doc.encode().unwrap();
        assert!(out.windows(9).any(|w| w == [0x09, 0x00, 0x01, b'b', 0x06, 0, 0, 0, 1]));
        assert!(out.ends_with(&[0, 0, 0, 0]));
    }

    #[test]
    fn type_errors_and_truncation() {
        let data = hello_world();
        let mut doc = NbtDocument::parse(&data).unwrap();
        assert!(doc.apply_ast(&Node::map_from([("name", Node::I32(1))])).is_err());
        assert!(doc.apply_ast(&Node::map_from([("x", Node::Null)])).is_err());
        assert!(doc.apply_ast(&Node::I32(1)).is_err());
        assert_eq!(doc.encode().unwrap(), data);
        for n in 0..data.len() {
            assert!(NbtDocument::parse(&data[..n]).is_err(), "prefix {n}");
            assert_eq!(NbtCodec.detect(&data[..n], &DetectContext::default()), 0);
        }
        // Huge declared list length does not allocate.
        let bad = [0x0a, 0, 0, 0x09, 0, 1, b'l', 0x03, 0x7f, 0xff, 0xff, 0xff, 0];
        assert!(NbtDocument::parse(&bad).is_err());
        assert_eq!(NbtCodec.detect(b"{\"a\":1}", &DetectContext::default()), 0);
    }
    /// `{l: [[[...]]]}` with `lists` nested lists (big-endian).
    fn nested_lists(lists: usize) -> Vec<u8> {
        let mut v = vec![0x0a, 0, 0, 0x09, 0, 1, b'l'];
        for _ in 1..lists {
            v.extend_from_slice(&[0x09, 0, 0, 0, 1]);
        }
        v.extend_from_slice(&[0x00, 0, 0, 0, 0, 0x00]);
        v
    }

    #[test]
    fn deep_nesting_is_bounded() {
        let ok = nested_lists(MAX_DEPTH - 1);
        let mut doc = NbtDocument::parse(&ok).unwrap();
        assert_eq!(doc.encode().unwrap(), ok);
        let mut ast = doc.to_ast();
        let mut path = NodePath::root().key("l");
        for _ in 1..MAX_DEPTH - 1 {
            path = path.index(0);
        }
        ast.set(&path, Node::Array(vec![Node::I32(5)]), false).unwrap();
        doc.apply_ast(&ast).unwrap();
        assert_eq!(NbtDocument::parse(&doc.encode().unwrap()).unwrap().to_ast(), ast);
        assert!(NbtDocument::parse(&nested_lists(MAX_DEPTH + 10)).is_err());
    }
}
