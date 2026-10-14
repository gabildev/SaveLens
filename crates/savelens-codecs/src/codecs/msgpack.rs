//! Lossless MessagePack codec.
//!
//! A purpose-built parser that remembers the exact marker of every value:
//! a `5` stored as `uint32`, a short string stored as `str16` or a map stored
//! as `map16` are written back exactly as they were. Edited values keep their
//! original marker whenever the new value still fits it; otherwise the
//! smallest standard encoding of the same family is used.
//!
//! Some games append several MessagePack values to one file; such documents
//! are projected as an array of the top-level values.

use savelens_core::{bit_equal, Endianness, Node, NodeMap, NodePath, PathSeg};

use crate::document::{coerce_like, Capabilities, Confidence, DetectContext, Document, StructuralCodec};
use crate::error::{CodecError, CodecResult};
use crate::io::ByteReader;

const CODEC: &str = "msgpack";
/// Maximum container nesting (bounded so that hostile input cannot exhaust the stack).
const MAX_DEPTH: usize = 256;

/// Integer marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntFmt {
    PosFix,
    NegFix,
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
}

impl IntFmt {
    fn range(self) -> (i128, i128) {
        match self {
            IntFmt::PosFix => (0, 127),
            IntFmt::NegFix => (-32, -1),
            IntFmt::U8 => (0, u8::MAX as i128),
            IntFmt::U16 => (0, u16::MAX as i128),
            IntFmt::U32 => (0, u32::MAX as i128),
            IntFmt::U64 => (0, u64::MAX as i128),
            IntFmt::I8 => (i8::MIN as i128, i8::MAX as i128),
            IntFmt::I16 => (i16::MIN as i128, i16::MAX as i128),
            IntFmt::I32 => (i32::MIN as i128, i32::MAX as i128),
            IntFmt::I64 => (i64::MIN as i128, i64::MAX as i128),
        }
    }

    fn fits(self, v: i128) -> bool {
        let (lo, hi) = self.range();
        (lo..=hi).contains(&v)
    }

    fn smallest(v: i128) -> IntFmt {
        let order: &[IntFmt] = if v >= 0 {
            &[IntFmt::PosFix, IntFmt::U8, IntFmt::U16, IntFmt::U32, IntFmt::U64]
        } else {
            &[IntFmt::NegFix, IntFmt::I8, IntFmt::I16, IntFmt::I32, IntFmt::I64]
        };
        order.iter().copied().find(|f| f.fits(v)).unwrap_or(IntFmt::I64)
    }
}

/// Length marker of str/bin/array/map values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LenFmt {
    Fix,
    W8,
    W16,
    W32,
}

/// Value family, deciding which length markers exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Str,
    Bin,
    Array,
    Map,
}

impl Family {
    fn max(self, fmt: LenFmt) -> Option<usize> {
        match (self, fmt) {
            (Family::Str, LenFmt::Fix) => Some(31),
            (Family::Array | Family::Map, LenFmt::Fix) => Some(15),
            (Family::Str | Family::Bin, LenFmt::W8) => Some(u8::MAX as usize),
            (_, LenFmt::W16) => Some(u16::MAX as usize),
            (_, LenFmt::W32) => Some(u32::MAX as usize),
            _ => None,
        }
    }

    fn fits(self, fmt: LenFmt, n: usize) -> bool {
        self.max(fmt).is_some_and(|m| n <= m)
    }

    fn choose(self, original: Option<LenFmt>, n: usize, path: &NodePath) -> CodecResult<LenFmt> {
        if let Some(f) = original {
            if self.fits(f, n) {
                return Ok(f);
            }
        }
        [LenFmt::Fix, LenFmt::W8, LenFmt::W16, LenFmt::W32]
            .into_iter()
            .find(|f| self.fits(*f, n))
            .ok_or_else(|| CodecError::unencodable(CODEC, path, format!("length {n} exceeds the MessagePack 32-bit limit")))
    }
}

/// Ext marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtFmt {
    Fix1,
    Fix2,
    Fix4,
    Fix8,
    Fix16,
    E8,
    E16,
    E32,
}

impl ExtFmt {
    fn fits(self, n: usize) -> bool {
        match self {
            ExtFmt::Fix1 => n == 1,
            ExtFmt::Fix2 => n == 2,
            ExtFmt::Fix4 => n == 4,
            ExtFmt::Fix8 => n == 8,
            ExtFmt::Fix16 => n == 16,
            ExtFmt::E8 => n <= u8::MAX as usize,
            ExtFmt::E16 => n <= u16::MAX as usize,
            ExtFmt::E32 => n <= u32::MAX as usize,
        }
    }

    fn choose(original: Option<ExtFmt>, n: usize, path: &NodePath) -> CodecResult<ExtFmt> {
        if let Some(f) = original {
            if f.fits(n) {
                return Ok(f);
            }
        }
        [
            ExtFmt::Fix1,
            ExtFmt::Fix2,
            ExtFmt::Fix4,
            ExtFmt::Fix8,
            ExtFmt::Fix16,
            ExtFmt::E8,
            ExtFmt::E16,
            ExtFmt::E32,
        ]
        .into_iter()
        .find(|f| f.fits(n))
        .ok_or_else(|| CodecError::unencodable(CODEC, path, format!("ext data of {n} bytes exceeds the 32-bit limit")))
    }
}

/// A MessagePack value with its exact encoding.
#[derive(Debug, Clone)]
enum MVal {
    Nil,
    Bool(bool),
    Int { fmt: IntFmt, v: i128 },
    F32(f32),
    F64(f64),
    Str { fmt: LenFmt, raw: Vec<u8> },
    Bin { fmt: LenFmt, data: Vec<u8> },
    Array { fmt: LenFmt, items: Vec<MVal> },
    Map { fmt: LenFmt, entries: Vec<(MVal, MVal)> },
    Ext { fmt: ExtFmt, ty: i8, data: Vec<u8> },
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

struct Parser<'a> {
    r: ByteReader<'a>,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn len(&mut self, fmt: LenFmt, fix: u8, ctx: &str) -> CodecResult<usize> {
        Ok(match fmt {
            LenFmt::Fix => fix as usize,
            LenFmt::W8 => self.r.u8(ctx)? as usize,
            LenFmt::W16 => self.r.u16_be(ctx)? as usize,
            LenFmt::W32 => self.r.u32_be(ctx)? as usize,
        })
    }

    fn bytes(&mut self, n: usize, ctx: &str) -> CodecResult<Vec<u8>> {
        Ok(self.r.take(n, ctx)?.to_vec())
    }

    fn container(&mut self, map: bool, fmt: LenFmt, fix: u8) -> CodecResult<MVal> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.r.malformed("nesting too deep"));
        }
        let ctx = if map { "map length" } else { "array length" };
        let n = self.len(fmt, fix, ctx)?;
        self.r.check_count(n, if map { 2 } else { 1 }, ctx)?;
        let v = if map {
            let mut entries = Vec::with_capacity(n);
            for _ in 0..n {
                let k = self.value()?;
                let v = self.value()?;
                entries.push((k, v));
            }
            MVal::Map { fmt, entries }
        } else {
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                items.push(self.value()?);
            }
            MVal::Array { fmt, items }
        };
        self.depth -= 1;
        Ok(v)
    }

    fn ext(&mut self, fmt: ExtFmt) -> CodecResult<MVal> {
        let n = match fmt {
            ExtFmt::Fix1 => 1,
            ExtFmt::Fix2 => 2,
            ExtFmt::Fix4 => 4,
            ExtFmt::Fix8 => 8,
            ExtFmt::Fix16 => 16,
            ExtFmt::E8 => self.r.u8("ext length")? as usize,
            ExtFmt::E16 => self.r.u16_be("ext length")? as usize,
            ExtFmt::E32 => self.r.u32_be("ext length")? as usize,
        };
        let ty = self.r.i8("ext type")?;
        let data = self.bytes(n, "ext data")?;
        Ok(MVal::Ext { fmt, ty, data })
    }

    /// Reads one value. Containers recurse; everything else is decoded by
    /// [`Parser::scalar`], kept out of line so the recursive frame stays small.
    fn value(&mut self) -> CodecResult<MVal> {
        let m = self.r.u8("marker")?;
        match m {
            0x80..=0x8f => self.container(true, LenFmt::Fix, m & 0x0f),
            0x90..=0x9f => self.container(false, LenFmt::Fix, m & 0x0f),
            0xdc => self.container(false, LenFmt::W16, 0),
            0xdd => self.container(false, LenFmt::W32, 0),
            0xde => self.container(true, LenFmt::W16, 0),
            0xdf => self.container(true, LenFmt::W32, 0),
            _ => self.scalar(m),
        }
    }

    #[inline(never)]
    fn scalar(&mut self, m: u8) -> CodecResult<MVal> {
        Ok(match m {
            0x00..=0x7f => MVal::Int {
                fmt: IntFmt::PosFix,
                v: m as i128,
            },
            0x80..=0x9f | 0xdc..=0xdf => return Err(self.r.malformed(format!("container marker {m:#04x} in scalar position"))),
            0xa0..=0xbf => MVal::Str {
                fmt: LenFmt::Fix,
                raw: self.bytes((m & 0x1f) as usize, "fixstr")?,
            },
            0xc0 => MVal::Nil,
            0xc1 => return Err(self.r.malformed("reserved marker 0xc1")),
            0xc2 => MVal::Bool(false),
            0xc3 => MVal::Bool(true),
            0xc4..=0xc6 => {
                let fmt = [LenFmt::W8, LenFmt::W16, LenFmt::W32][(m - 0xc4) as usize];
                let n = self.len(fmt, 0, "bin length")?;
                MVal::Bin {
                    fmt,
                    data: self.bytes(n, "bin data")?,
                }
            }
            0xc7 => self.ext(ExtFmt::E8)?,
            0xc8 => self.ext(ExtFmt::E16)?,
            0xc9 => self.ext(ExtFmt::E32)?,
            0xca => MVal::F32(self.r.f32_be("float32")?),
            0xcb => MVal::F64(self.r.f64_be("float64")?),
            0xcc => MVal::Int {
                fmt: IntFmt::U8,
                v: self.r.u8("uint8")? as i128,
            },
            0xcd => MVal::Int {
                fmt: IntFmt::U16,
                v: self.r.u16_be("uint16")? as i128,
            },
            0xce => MVal::Int {
                fmt: IntFmt::U32,
                v: self.r.u32_be("uint32")? as i128,
            },
            0xcf => MVal::Int {
                fmt: IntFmt::U64,
                v: self.r.u64_be("uint64")? as i128,
            },
            0xd0 => MVal::Int {
                fmt: IntFmt::I8,
                v: self.r.i8("int8")? as i128,
            },
            0xd1 => MVal::Int {
                fmt: IntFmt::I16,
                v: self.r.i16_be("int16")? as i128,
            },
            0xd2 => MVal::Int {
                fmt: IntFmt::I32,
                v: self.r.i32_be("int32")? as i128,
            },
            0xd3 => MVal::Int {
                fmt: IntFmt::I64,
                v: self.r.i64_be("int64")? as i128,
            },
            0xd4 => self.ext(ExtFmt::Fix1)?,
            0xd5 => self.ext(ExtFmt::Fix2)?,
            0xd6 => self.ext(ExtFmt::Fix4)?,
            0xd7 => self.ext(ExtFmt::Fix8)?,
            0xd8 => self.ext(ExtFmt::Fix16)?,
            0xd9..=0xdb => {
                let fmt = [LenFmt::W8, LenFmt::W16, LenFmt::W32][(m - 0xd9) as usize];
                let n = self.len(fmt, 0, "str length")?;
                MVal::Str {
                    fmt,
                    raw: self.bytes(n, "str data")?,
                }
            }
            0xe0..=0xff => MVal::Int {
                fmt: IntFmt::NegFix,
                v: (m as i8) as i128,
            },
        })
    }
}

/// Parses every top-level value of `data`.
fn parse_all(data: &[u8]) -> CodecResult<Vec<MVal>> {
    if data.is_empty() {
        return Err(CodecError::malformed(CODEC, 0, "empty input"));
    }
    let mut p = Parser {
        r: ByteReader::new(data, CODEC),
        depth: 0,
    };
    let mut values = Vec::new();
    while !p.r.is_empty() {
        values.push(p.value()?);
    }
    Ok(values)
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

fn write_len(out: &mut Vec<u8>, fmt: LenFmt, fix_base: u8, markers: [u8; 3], n: usize) -> CodecResult<()> {
    let too_long = || CodecError::unsupported(CODEC, format!("length {n} does not fit its marker"));
    match fmt {
        LenFmt::Fix => {
            let v = u8::try_from(n).map_err(|_| too_long())?;
            out.push(fix_base | v);
        }
        LenFmt::W8 => {
            out.push(markers[0]);
            out.push(u8::try_from(n).map_err(|_| too_long())?);
        }
        LenFmt::W16 => {
            out.push(markers[1]);
            out.extend_from_slice(&u16::try_from(n).map_err(|_| too_long())?.to_be_bytes());
        }
        LenFmt::W32 => {
            out.push(markers[2]);
            out.extend_from_slice(&u32::try_from(n).map_err(|_| too_long())?.to_be_bytes());
        }
    }
    Ok(())
}

/// Serialises a value. Only containers recurse; leaves are written out of
/// line so that each nesting level costs little stack.
fn write(v: &MVal, out: &mut Vec<u8>) -> CodecResult<()> {
    match v {
        MVal::Array { fmt, items } => {
            if *fmt == LenFmt::W8 {
                return Err(CodecError::unsupported(CODEC, "arrays have no 8-bit length marker"));
            }
            write_len(out, *fmt, 0x90, [0, 0xdc, 0xdd], items.len())?;
            for it in items {
                write(it, out)?;
            }
            Ok(())
        }
        MVal::Map { fmt, entries } => {
            if *fmt == LenFmt::W8 {
                return Err(CodecError::unsupported(CODEC, "maps have no 8-bit length marker"));
            }
            write_len(out, *fmt, 0x80, [0, 0xde, 0xdf], entries.len())?;
            for (k, v) in entries {
                write(k, out)?;
                write(v, out)?;
            }
            Ok(())
        }
        leaf => write_leaf(leaf, out),
    }
}

#[inline(never)]
fn write_leaf(v: &MVal, out: &mut Vec<u8>) -> CodecResult<()> {
    match v {
        MVal::Nil => out.push(0xc0),
        MVal::Bool(b) => out.push(if *b { 0xc3 } else { 0xc2 }),
        MVal::Int { fmt, v } => {
            if !fmt.fits(*v) {
                return Err(CodecError::unsupported(CODEC, format!("integer {v} does not fit {fmt:?}")));
            }
            // Range checked above: the truncating casts below are exact.
            match fmt {
                IntFmt::PosFix => out.push(*v as u8),
                IntFmt::NegFix => out.push(*v as i8 as u8),
                IntFmt::U8 => out.extend_from_slice(&[0xcc, *v as u8]),
                IntFmt::U16 => {
                    out.push(0xcd);
                    out.extend_from_slice(&(*v as u16).to_be_bytes());
                }
                IntFmt::U32 => {
                    out.push(0xce);
                    out.extend_from_slice(&(*v as u32).to_be_bytes());
                }
                IntFmt::U64 => {
                    out.push(0xcf);
                    out.extend_from_slice(&(*v as u64).to_be_bytes());
                }
                IntFmt::I8 => out.extend_from_slice(&[0xd0, *v as i8 as u8]),
                IntFmt::I16 => {
                    out.push(0xd1);
                    out.extend_from_slice(&(*v as i16).to_be_bytes());
                }
                IntFmt::I32 => {
                    out.push(0xd2);
                    out.extend_from_slice(&(*v as i32).to_be_bytes());
                }
                IntFmt::I64 => {
                    out.push(0xd3);
                    out.extend_from_slice(&(*v as i64).to_be_bytes());
                }
            }
        }
        MVal::F32(f) => {
            out.push(0xca);
            out.extend_from_slice(&f.to_be_bytes());
        }
        MVal::F64(f) => {
            out.push(0xcb);
            out.extend_from_slice(&f.to_be_bytes());
        }
        MVal::Str { fmt, raw } => {
            write_len(out, *fmt, 0xa0, [0xd9, 0xda, 0xdb], raw.len())?;
            out.extend_from_slice(raw);
        }
        MVal::Bin { fmt, data } => {
            if *fmt == LenFmt::Fix {
                return Err(CodecError::unsupported(CODEC, "bin values have no fixed-length marker"));
            }
            write_len(out, *fmt, 0, [0xc4, 0xc5, 0xc6], data.len())?;
            out.extend_from_slice(data);
        }
        MVal::Array { .. } | MVal::Map { .. } => {
            return Err(CodecError::unsupported(CODEC, "container reached the leaf writer"));
        }
        MVal::Ext { fmt, ty, data } => {
            if !fmt.fits(data.len()) {
                return Err(CodecError::unsupported(CODEC, format!("ext data of {} bytes does not fit {fmt:?}", data.len())));
            }
            match fmt {
                ExtFmt::Fix1 => out.push(0xd4),
                ExtFmt::Fix2 => out.push(0xd5),
                ExtFmt::Fix4 => out.push(0xd6),
                ExtFmt::Fix8 => out.push(0xd7),
                ExtFmt::Fix16 => out.push(0xd8),
                ExtFmt::E8 => out.extend_from_slice(&[0xc7, data.len() as u8]),
                ExtFmt::E16 => {
                    out.push(0xc8);
                    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
                }
                ExtFmt::E32 => {
                    out.push(0xc9);
                    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
                }
            }
            out.push(*ty as u8);
            out.extend_from_slice(data);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Projection
// ---------------------------------------------------------------------------

/// String form of a simple map key (UTF-8 string or integer).
fn key_text(k: &MVal) -> Option<String> {
    match k {
        MVal::Str { raw, .. } => std::str::from_utf8(raw).ok().map(str::to_string),
        MVal::Int { v, .. } => Some(v.to_string()),
        _ => None,
    }
}

fn keys_simple(entries: &[(MVal, MVal)]) -> bool {
    entries.iter().all(|(k, _)| key_text(k).is_some())
}

/// AST keys of a simple map, de-duplicating collisions as `key#2`, `key#3`…
fn ast_keys(entries: &[(MVal, MVal)]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    entries
        .iter()
        .map(|(k, _)| {
            let base = key_text(k).unwrap_or_default();
            let mut key = base.clone();
            if !seen.insert(key.clone()) {
                let mut n = 2;
                loop {
                    let cand = format!("{base}#{n}");
                    if seen.insert(cand.clone()) {
                        key = cand;
                        break;
                    }
                    n += 1;
                }
            }
            key
        })
        .collect()
}

fn project_int(v: i128) -> Node {
    match i64::try_from(v) {
        Ok(x) => Node::I64(x),
        Err(_) => Node::U64(u64::try_from(v).unwrap_or(u64::MAX)),
    }
}

fn project(v: &MVal) -> Node {
    match v {
        MVal::Nil => Node::Null,
        MVal::Bool(b) => Node::Bool(*b),
        MVal::Int { v, .. } => project_int(*v),
        MVal::F32(f) => Node::F32(*f),
        MVal::F64(f) => Node::F64(*f),
        MVal::Str { raw, .. } => match std::str::from_utf8(raw) {
            Ok(s) => Node::String(s.to_string()),
            Err(_) => Node::Bytes(raw.clone()),
        },
        MVal::Bin { data, .. } => Node::Bytes(data.clone()),
        MVal::Array { items, .. } => Node::Array(items.iter().map(project).collect()),
        MVal::Map { entries, .. } => {
            if keys_simple(entries) {
                let keys = ast_keys(entries);
                let mut m = NodeMap::with_capacity(entries.len());
                for (k, (_, val)) in keys.into_iter().zip(entries) {
                    m.insert(k, project(val));
                }
                Node::Map(m)
            } else {
                Node::Array(
                    entries
                        .iter()
                        .map(|(k, val)| Node::map_from([("key", project(k)), ("value", project(val))]))
                        .collect(),
                )
            }
        }
        MVal::Ext { ty, data, .. } => Node::map_from([("type", Node::I8(*ty)), ("data", Node::Bytes(data.clone()))]),
    }
}

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

fn int_value(node: &Node, path: &NodePath) -> CodecResult<i128> {
    use savelens_core::ScalarValue;
    let v = match node {
        Node::String(_) | Node::Null | Node::Bytes(_) | Node::Array(_) | Node::Map(_) | Node::RawRegion(_) => None,
        other => other.as_scalar_lenient(),
    };
    let i = match v {
        Some(ScalarValue::Int(i)) => i,
        Some(ScalarValue::Float(f)) if f.is_finite() && f.fract() == 0.0 => f as i128,
        _ => return Err(CodecError::type_mismatch(CODEC, path, "integer", node.kind().name())),
    };
    if !(i64::MIN as i128..=u64::MAX as i128).contains(&i) {
        return Err(CodecError::unencodable(CODEC, path, format!("{i} is outside the MessagePack integer range")));
    }
    Ok(i)
}

fn mismatch(path: &NodePath, expected: &str, node: &Node) -> CodecError {
    CodecError::type_mismatch(CODEC, path, expected, node.kind().name())
}

fn ext_parts(m: &NodeMap, path: &NodePath, ty: i8) -> CodecResult<(i8, Vec<u8>)> {
    if m.len() != 2 || !m.contains_key("type") || !m.contains_key("data") {
        return Err(CodecError::structure(CODEC, path, "ext values are maps with exactly the keys `type` and `data`"));
    }
    let ty = match m.get("type").map(|t| coerce_like(CODEC, &path.key("type"), &Node::I8(ty), t)) {
        Some(Ok(Node::I8(t))) => t,
        Some(Err(e)) => return Err(e),
        _ => return Err(CodecError::type_mismatch(CODEC, path.key("type"), "i8", "other")),
    };
    let data = match m.get("data").map(|d| coerce_like(CODEC, &path.key("data"), &Node::Bytes(Vec::new()), d)) {
        Some(Ok(Node::Bytes(b))) => b,
        Some(Err(e)) => return Err(e),
        _ => return Err(CodecError::type_mismatch(CODEC, path.key("data"), "bytes", "other")),
    };
    Ok((ty, data))
}

/// Builds a fresh value (smallest encodings) for a node.
fn build(node: &Node, path: &mut NodePath) -> CodecResult<MVal> {
    match node {
        Node::Array(a) => build_array(a, path),
        Node::Map(m) => build_map(m, path),
        leaf => build_leaf(leaf, path),
    }
}

#[inline(never)]
fn build_array(a: &[Node], path: &mut NodePath) -> CodecResult<MVal> {
    let mut items = Vec::with_capacity(a.len());
    for (i, child) in a.iter().enumerate() {
        path.push(PathSeg::Index(i));
        items.push(build(child, path)?);
        path.pop();
    }
    Ok(MVal::Array {
        fmt: Family::Array.choose(None, items.len(), path)?,
        items,
    })
}

#[inline(never)]
fn build_map(m: &NodeMap, path: &mut NodePath) -> CodecResult<MVal> {
    let mut entries = Vec::with_capacity(m.len());
    for (k, child) in m {
        path.push(PathSeg::Key(k.clone()));
        let key = build_leaf(&Node::String(k.clone()), path)?;
        entries.push((key, build(child, path)?));
        path.pop();
    }
    Ok(MVal::Map {
        fmt: Family::Map.choose(None, entries.len(), path)?,
        entries,
    })
}

#[inline(never)]
fn build_leaf(node: &Node, path: &mut NodePath) -> CodecResult<MVal> {
    Ok(match node {
        Node::Null => MVal::Nil,
        Node::Bool(b) => MVal::Bool(*b),
        Node::I8(_) | Node::I16(_) | Node::I32(_) | Node::I64(_) | Node::U8(_) | Node::U16(_) | Node::U32(_) | Node::U64(_) => {
            let v = int_value(node, path)?;
            MVal::Int {
                fmt: IntFmt::smallest(v),
                v,
            }
        }
        Node::F32(f) => MVal::F32(*f),
        Node::F64(f) => MVal::F64(*f),
        Node::String(s) => MVal::Str {
            fmt: Family::Str.choose(None, s.len(), path)?,
            raw: s.as_bytes().to_vec(),
        },
        Node::Bytes(b) => MVal::Bin {
            fmt: Family::Bin.choose(None, b.len(), path)?,
            data: b.clone(),
        },
        Node::RawRegion(r) => MVal::Bin {
            fmt: Family::Bin.choose(None, r.bytes.len(), path)?,
            data: r.bytes.clone(),
        },
        Node::Array(_) | Node::Map(_) => {
            return Err(CodecError::structure(CODEC, &*path, "container reached the leaf builder"));
        }
    })
}

/// Key value for a new entry of a simple map: integer keys stay integers
/// when every existing key is one.
fn new_key(k: &str, all_int_keys: bool, path: &mut NodePath) -> CodecResult<MVal> {
    if all_int_keys {
        if let Ok(v) = k.parse::<i128>() {
            if (i64::MIN as i128..=u64::MAX as i128).contains(&v) {
                return Ok(MVal::Int {
                    fmt: IntFmt::smallest(v),
                    v,
                });
            }
        }
    }
    build(&Node::String(k.to_string()), path)
}

fn pair_of<'n>(node: &'n Node, path: &NodePath) -> CodecResult<(&'n Node, &'n Node)> {
    match node {
        Node::Map(m) if m.len() == 2 => match (m.get("key"), m.get("value")) {
            (Some(k), Some(v)) => Ok((k, v)),
            _ => Err(CodecError::structure(CODEC, path, "map entries are `{key, value}` maps")),
        },
        _ => Err(CodecError::structure(CODEC, path, "map entries are `{key, value}` maps")),
    }
}

/// Applies an edited node to a value. Only containers recurse; the helpers
/// are kept out of line so that each nesting level costs little stack.
fn reconcile(v: &mut MVal, node: &Node, path: &mut NodePath) -> CodecResult<()> {
    if bit_equal(&project(v), node) {
        return Ok(());
    }
    match v {
        MVal::Array { fmt, items } => reconcile_array(fmt, items, node, path),
        MVal::Map { fmt, entries } if keys_simple(entries) => reconcile_simple_map(fmt, entries, node, path),
        MVal::Map { fmt, entries } => reconcile_pair_map(fmt, entries, node, path),
        leaf => reconcile_leaf(leaf, node, path),
    }
}

#[inline(never)]
fn reconcile_array(fmt: &mut LenFmt, items: &mut Vec<MVal>, node: &Node, path: &mut NodePath) -> CodecResult<()> {
    let Node::Array(a) = node else {
        return Err(mismatch(path, "array", node));
    };
    items.truncate(a.len());
    for (i, child) in a.iter().enumerate() {
        path.push(PathSeg::Index(i));
        if i < items.len() {
            reconcile(&mut items[i], child, path)?;
        } else {
            items.push(build(child, path)?);
        }
        path.pop();
    }
    *fmt = Family::Array.choose(Some(*fmt), items.len(), path)?;
    Ok(())
}

#[inline(never)]
fn reconcile_simple_map(fmt: &mut LenFmt, entries: &mut Vec<(MVal, MVal)>, node: &Node, path: &mut NodePath) -> CodecResult<()> {
    let Node::Map(m) = node else {
        return Err(mismatch(path, "map", node));
    };
    let keys = ast_keys(entries);
    let all_int_keys = !entries.is_empty() && entries.iter().all(|(k, _)| matches!(k, MVal::Int { .. }));
    let mut old: Vec<Option<(MVal, MVal)>> = std::mem::take(entries).into_iter().map(Some).collect();
    let mut out = Vec::with_capacity(m.len());
    for (k, child) in m {
        path.push(PathSeg::Key(k.clone()));
        match keys.iter().position(|x| x == k).and_then(|i| old.get_mut(i).and_then(Option::take)) {
            Some((key, mut val)) => {
                reconcile(&mut val, child, path)?;
                out.push((key, val));
            }
            None => {
                let key = new_key(k, all_int_keys, path)?;
                out.push((key, build(child, path)?));
            }
        }
        path.pop();
    }
    *fmt = Family::Map.choose(Some(*fmt), out.len(), path)?;
    *entries = out;
    Ok(())
}

#[inline(never)]
fn reconcile_pair_map(fmt: &mut LenFmt, entries: &mut Vec<(MVal, MVal)>, node: &Node, path: &mut NodePath) -> CodecResult<()> {
    let Node::Array(a) = node else {
        return Err(mismatch(path, "array of {key, value} entries", node));
    };
    entries.truncate(a.len());
    for (i, child) in a.iter().enumerate() {
        path.push(PathSeg::Index(i));
        let (kn, vn) = pair_of(child, path)?;
        if i < entries.len() {
            path.push(PathSeg::Key("key".into()));
            reconcile(&mut entries[i].0, kn, path)?;
            path.pop();
            path.push(PathSeg::Key("value".into()));
            reconcile(&mut entries[i].1, vn, path)?;
            path.pop();
        } else {
            let k = build(kn, path)?;
            let v = build(vn, path)?;
            entries.push((k, v));
        }
        path.pop();
    }
    if keys_simple(entries) {
        return Err(CodecError::structure(
            CODEC,
            &*path,
            "the edit leaves this map without non-string keys; it would change shape (edit it as a map instead)",
        ));
    }
    *fmt = Family::Map.choose(Some(*fmt), entries.len(), path)?;
    Ok(())
}

#[inline(never)]
fn reconcile_leaf(v: &mut MVal, node: &Node, path: &mut NodePath) -> CodecResult<()> {
    match v {
        MVal::Nil => Err(mismatch(path, "null", node)),
        MVal::Bool(b) => match coerce_like(CODEC, path, &Node::Bool(*b), node)? {
            Node::Bool(x) => {
                *b = x;
                Ok(())
            }
            other => Err(mismatch(path, "bool", &other)),
        },
        MVal::Int { fmt, v } => {
            let new = int_value(node, path)?;
            if !fmt.fits(new) {
                *fmt = IntFmt::smallest(new);
            }
            *v = new;
            Ok(())
        }
        MVal::F32(f) => match coerce_like(CODEC, path, &Node::F32(*f), node)? {
            Node::F32(x) => {
                *f = x;
                Ok(())
            }
            other => Err(mismatch(path, "f32", &other)),
        },
        MVal::F64(f) => match coerce_like(CODEC, path, &Node::F64(*f), node)? {
            Node::F64(x) => {
                *f = x;
                Ok(())
            }
            other => Err(mismatch(path, "f64", &other)),
        },
        MVal::Str { fmt, raw } => {
            let new = match node {
                Node::String(s) => s.as_bytes().to_vec(),
                // Non-UTF-8 str payloads are projected as bytes; they stay `str` on disk.
                Node::Bytes(b) if std::str::from_utf8(raw).is_err() => b.clone(),
                other => return Err(mismatch(path, "string", other)),
            };
            *fmt = Family::Str.choose(Some(*fmt), new.len(), path)?;
            *raw = new;
            Ok(())
        }
        MVal::Bin { fmt, data } => match coerce_like(CODEC, path, &Node::Bytes(Vec::new()), node)? {
            Node::Bytes(b) => {
                *fmt = Family::Bin.choose(Some(*fmt), b.len(), path)?;
                *data = b;
                Ok(())
            }
            other => Err(mismatch(path, "bytes", &other)),
        },
        MVal::Ext { fmt, ty, data } => {
            let Node::Map(m) = node else {
                return Err(mismatch(path, "ext map {type, data}", node));
            };
            let (t, d) = ext_parts(m, path, *ty)?;
            *fmt = ExtFmt::choose(Some(*fmt), d.len(), path)?;
            *ty = t;
            *data = d;
            Ok(())
        }
        MVal::Array { .. } | MVal::Map { .. } => Err(CodecError::structure(CODEC, &*path, "container reached the leaf editor")),
    }
}

// ---------------------------------------------------------------------------
// Document
// ---------------------------------------------------------------------------

/// A parsed MessagePack document (one or several concatenated values).
#[derive(Debug, Clone)]
pub struct MsgPackDocument {
    values: Vec<MVal>,
}

impl MsgPackDocument {
    /// Parses MessagePack bytes; the whole input must be consumed.
    pub fn parse(data: &[u8]) -> CodecResult<MsgPackDocument> {
        Ok(MsgPackDocument { values: parse_all(data)? })
    }

    /// Number of top-level values.
    pub fn value_count(&self) -> usize {
        self.values.len()
    }

    /// True when the file is a concatenation of several values.
    pub fn is_multi(&self) -> bool {
        self.values.len() > 1
    }
}

impl Document for MsgPackDocument {
    fn codec_id(&self) -> &'static str {
        CODEC
    }

    fn to_ast(&self) -> Node {
        match self.values.as_slice() {
            [single] => project(single),
            many => Node::Array(many.iter().map(project).collect()),
        }
    }

    fn apply_ast(&mut self, root: &Node) -> CodecResult<()> {
        let mut work = self.values.clone();
        let mut path = NodePath::root();
        if work.len() == 1 {
            reconcile(&mut work[0], root, &mut path)?;
        } else {
            let Node::Array(a) = root else {
                return Err(mismatch(&path, "array of top-level values", root));
            };
            if a.len() < 2 {
                return Err(CodecError::structure(
                    CODEC,
                    "$",
                    "a multi-value MessagePack file must keep at least two top-level values",
                ));
            }
            work.truncate(a.len());
            for (i, child) in a.iter().enumerate() {
                path.push(PathSeg::Index(i));
                if i < work.len() {
                    reconcile(&mut work[i], child, &mut path)?;
                } else {
                    work.push(build(child, &mut path)?);
                }
                path.pop();
            }
        }
        self.values = work;
        Ok(())
    }

    fn encode(&self) -> CodecResult<Vec<u8>> {
        let mut out = Vec::new();
        for v in &self.values {
            write(v, &mut out)?;
        }
        Ok(out)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::FULL
    }

    fn endianness(&self) -> Endianness {
        Endianness::Big
    }

    fn notes(&self) -> Vec<String> {
        if self.is_multi() {
            vec![format!(
                "{} concatenated top-level MessagePack values (projected as an array)",
                self.values.len()
            )]
        } else {
            vec!["single MessagePack value".into()]
        }
    }

    fn clone_box(&self) -> Box<dyn Document> {
        Box::new(self.clone())
    }
}

/// True when the input is printable text (UTF-8 without control characters
/// other than whitespace) — JSON, INI, XML, …
fn is_printable_text(bytes: &[u8]) -> bool {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.chars().all(|c| !c.is_control() || c.is_whitespace()),
        Err(_) => false,
    }
}

/// The MessagePack codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct MsgPackCodec;

impl StructuralCodec for MsgPackCodec {
    fn id(&self) -> &'static str {
        CODEC
    }

    fn name(&self) -> &'static str {
        "MessagePack"
    }

    fn detect(&self, bytes: &[u8], ctx: &DetectContext) -> Confidence {
        if is_printable_text(bytes) {
            return 0;
        }
        let Ok(values) = parse_all(bytes) else { return 0 };
        let rich_root = match values.as_slice() {
            [MVal::Map { entries, .. }] => entries.len() >= 2,
            [MVal::Array { items, .. }] => items.len() >= 2,
            _ => false,
        };
        if rich_root {
            return 70;
        }
        match ctx.extension().as_deref() {
            Some("msgpack" | "mpk" | "dat") => 30,
            _ => 0,
        }
    }

    fn decode(&self, bytes: &[u8], _ctx: &DetectContext) -> CodecResult<Box<dyn Document>> {
        Ok(Box::new(MsgPackDocument::parse(bytes)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(bytes: &[u8]) -> MsgPackDocument {
        let doc = MsgPackDocument::parse(bytes).unwrap();
        assert_eq!(doc.encode().unwrap(), bytes);
        doc
    }

    #[test]
    fn markers_roundtrip() {
        // {"a": uint32 5, "b": str16 "hi", "c": [nil, true, -3, int64 -1]}
        let mut d = vec![0x83, 0xa1, b'a', 0xce, 0, 0, 0, 5, 0xa1, b'b', 0xda, 0, 2, b'h', b'i', 0xa1, b'c', 0x94, 0xc0, 0xc3, 0xfd, 0xd3];
        d.extend_from_slice(&(-1i64).to_be_bytes());
        let doc = rt(&d);
        assert_eq!(
            doc.to_ast(),
            Node::map_from([
                ("a", Node::I64(5)),
                ("b", Node::String("hi".into())),
                (
                    "c",
                    Node::Array(vec![Node::Null, Node::Bool(true), Node::I64(-3), Node::I64(-1)])
                ),
            ])
        );
        assert_eq!(MsgPackCodec.detect(&d, &DetectContext::default()), 70);
    }

    #[test]
    fn edit_keeps_marker_or_grows() {
        let d = vec![0x82, 0xa1, b'a', 0xd1, 0, 5, 0xa1, b's', 0xd9, 1, b'x'];
        let mut doc = rt(&d);
        doc.apply_ast(&Node::map_from([("a", Node::I64(300)), ("s", Node::String("yy".into()))]))
            .unwrap();
        assert_eq!(doc.encode().unwrap(), vec![0x82, 0xa1, b'a', 0xd1, 1, 44, 0xa1, b's', 0xd9, 2, b'y', b'y']);
        doc.apply_ast(&Node::map_from([("a", Node::I64(70000)), ("s", Node::String("yy".into()))]))
            .unwrap();
        assert_eq!(&doc.encode().unwrap()[3..8], &[0xce, 0, 1, 0x11, 0x70]);
        doc.apply_ast(&Node::map_from([
            ("a", Node::I64(-1)),
            ("s", Node::String("yy".into())),
            ("new", Node::F32(1.5)),
        ]))
        .unwrap();
        let out = doc.encode().unwrap();
        assert_eq!(out[0], 0x83);
        assert_eq!(out[3], 0xff);
    }

    #[test]
    fn int_keys_and_complex_keys() {
        // {1: "a", "1": "b"} -> keys "1" and "1#2"
        let d = vec![0x82, 0x01, 0xa1, b'a', 0xa1, b'1', 0xa1, b'b'];
        let doc = rt(&d);
        let ast = doc.to_ast();
        assert_eq!(ast.get(&NodePath::root().key("1#2")), Some(&Node::String("b".into())));
        // {[1]: 2, nil: 3} -> array of {key, value}
        let d = vec![0x82, 0x91, 0x01, 0x02, 0xc0, 0x03];
        let mut doc = rt(&d);
        let ast = doc.to_ast();
        assert!(matches!(ast, Node::Array(ref a) if a.len() == 2));
        let mut edited = ast.clone();
        edited.set(&NodePath::root().index(1).key("value"), Node::I64(9), false).unwrap();
        doc.apply_ast(&edited).unwrap();
        assert_eq!(doc.encode().unwrap(), vec![0x82, 0x91, 0x01, 0x02, 0xc0, 0x09]);
    }

    #[test]
    fn uint64_and_ext_and_multi() {
        let mut d = vec![0xcf];
        d.extend_from_slice(&u64::MAX.to_be_bytes());
        d.extend_from_slice(&[0xd6, 0xff, 1, 2, 3, 4]);
        let mut doc = rt(&d);
        assert!(doc.is_multi());
        let ast = doc.to_ast();
        assert_eq!(
            ast,
            Node::Array(vec![
                Node::U64(u64::MAX),
                Node::map_from([("type", Node::I8(-1)), ("data", Node::Bytes(vec![1, 2, 3, 4]))]),
            ])
        );
        doc.apply_ast(&Node::Array(vec![
            Node::I64(1),
            Node::map_from([("type", Node::I8(5)), ("data", Node::Bytes(vec![1, 2, 3]))]),
        ]))
        .unwrap();
        assert_eq!(doc.encode().unwrap(), vec![0xcf, 0, 0, 0, 0, 0, 0, 0, 1, 0xc7, 3, 5, 1, 2, 3]);
        assert!(doc.apply_ast(&Node::Array(vec![Node::I64(1)])).is_err());
    }

    #[test]
    fn errors_and_detection() {
        assert!(MsgPackDocument::parse(&[]).is_err());
        assert!(MsgPackDocument::parse(&[0xc1]).is_err());
        assert!(MsgPackDocument::parse(&[0xdd, 0xff, 0xff, 0xff, 0xff]).is_err());
        assert!(MsgPackDocument::parse(&[0xda, 0x00]).is_err());
        let ctx = DetectContext::default();
        assert_eq!(MsgPackCodec.detect(b"{\"a\": 1, \"b\": 2}", &ctx), 0);
        assert_eq!(MsgPackCodec.detect(b"[1,2]", &ctx), 0);
        assert_eq!(MsgPackCodec.detect(&[0xcc, 5], &DetectContext::for_file("x.msgpack")), 30);
        assert_eq!(MsgPackCodec.detect(&[0xcc, 5], &ctx), 0);
        let mut doc = rt(&[0x92, 0x01, 0xa1, b'x']);
        assert!(doc.apply_ast(&Node::Array(vec![Node::String("a".into()), Node::I64(1)])).is_err());
        assert!(doc.apply_ast(&Node::I64(1)).is_err());
    }
    #[test]
    fn deep_nesting_is_bounded() {
        let mut ok = vec![0x91; MAX_DEPTH - 1];
        ok.push(0x01);
        let mut doc = rt(&ok);
        let mut ast = doc.to_ast();
        let mut path = NodePath::root();
        for _ in 0..MAX_DEPTH - 1 {
            path = path.index(0);
        }
        ast.set(&path, Node::I64(300), false).unwrap();
        doc.apply_ast(&ast).unwrap();
        assert_eq!(MsgPackDocument::parse(&doc.encode().unwrap()).unwrap().to_ast(), ast);
        let mut deep = vec![0x81; 100_000];
        deep.push(0x01);
        assert!(MsgPackDocument::parse(&deep).is_err());
        assert_eq!(MsgPackCodec.detect(&deep, &DetectContext::default()), 0);
    }
}
