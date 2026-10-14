//! .NET BinaryFormatter (MS-NRBF) codec, as used by many Unity games.
//!
//! The parser keeps the record stream as an ordered arena of records (in
//! stream order) and a list of top-level records. Records that appear inline
//! as member values or array elements are referenced from their parent by
//! arena index, so the serializer walks exactly the same tree the parser read
//! and reproduces the input bit for bit:
//!
//! * every length-prefixed string keeps its original 7-bit varint bytes (a
//!   non-minimal length survives an unedited round trip),
//! * `ObjectNullMultiple` / `ObjectNullMultiple256` counts are kept as is,
//! * `Single` / `Double` values are stored as raw bit patterns (NaN payloads
//!   survive), `Boolean` keeps its raw byte, `DateTime` keeps its kind bits,
//! * `BinaryLibrary` records interleaved before member records stay in place,
//! * bytes after `MessageEnd` are kept verbatim.
//!
//! **Projection.** The AST starts at the header's `RootId` and resolves the
//! object graph into a tree: classes become maps (with a read-only `"$type"`
//! entry holding the class name), strings become `String`, primitives typed
//! nodes, arrays `Array` (rectangular arrays are flattened in row-major
//! order, null runs are expanded) and null records `Null`. A reference to an
//! object that was already projected becomes the read-only string
//! `"$ref:<id>"`. Strings are leaves and are projected with their value at
//! every place that references them; they are all bound to the same
//! `BinaryObjectString` record, so **editing a shared string changes it
//! everywhere it is referenced**.
//!
//! **Edits.** Every primitive member, primitive array element, boxed
//! primitive (`MemberPrimitiveTyped`) and string can be edited; the field
//! type is preserved through [`coerce_like`]. Structural edits are rejected.

use std::collections::{HashMap, HashSet};

use savelens_core::{Node, NodeMap, NodePath};

use crate::document::{apply_via_diff, coerce_like, Capabilities, Confidence, DetectContext, Document, PathEditable, StructuralCodec};
use crate::error::{CodecError, CodecResult};
use crate::io::{ByteReader, WriteExt};

const CODEC: &str = "nrbf";
/// Maximum nesting of inline records while parsing (and of the projection).
const MAX_DEPTH: usize = 256;
/// Maximum array rank accepted by `BinaryArray`.
const MAX_RANK: i32 = 32;
/// Upper bound for the number of null slots expanded from
/// `ObjectNullMultiple` records (protects the projection from hostile counts).
const MAX_NULL_SLOTS: usize = 1 << 24;

// ---------------------------------------------------------------------------
// Primitive building blocks
// ---------------------------------------------------------------------------

/// A `LengthPrefixedString` with its exact length prefix bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Lps {
    prefix: Vec<u8>,
    text: String,
}

fn encode_varint(mut len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(5);
    loop {
        let byte = (len & 0x7f) as u8;
        len >>= 7;
        if len == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

impl Lps {
    fn read(r: &mut ByteReader<'_>) -> CodecResult<Lps> {
        let mut prefix = Vec::with_capacity(2);
        let mut len: u64 = 0;
        for i in 0..5 {
            let b = r.u8("length-prefixed string length")?;
            prefix.push(b);
            len |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                break;
            }
            if i == 4 {
                return Err(r.malformed("string length prefix is longer than 5 bytes"));
            }
        }
        if len > i32::MAX as u64 {
            return Err(r.malformed(format!("string length {len} exceeds Int32.MaxValue")));
        }
        let start = r.pos();
        let bytes = r.take(len as usize, "length-prefixed string")?;
        let text = std::str::from_utf8(bytes)
            .map_err(|_| CodecError::malformed(CODEC, start, "string is not valid UTF-8"))?
            .to_string();
        Ok(Lps { prefix, text })
    }

    fn new(text: &str) -> Result<Lps, String> {
        if text.len() > i32::MAX as usize {
            return Err("string is longer than Int32.MaxValue bytes".into());
        }
        Ok(Lps {
            prefix: encode_varint(text.len()),
            text: text.to_string(),
        })
    }

    /// Replaces the text; the length prefix is re-encoded minimally only when
    /// the text actually changes.
    fn set(&mut self, text: &str) -> Result<(), String> {
        if self.text != text {
            *self = Lps::new(text)?;
        }
        Ok(())
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.prefix);
        out.extend_from_slice(self.text.as_bytes());
    }

    fn is_minimal(&self) -> bool {
        self.prefix == encode_varint(self.text.len())
    }
}

/// A primitive value (`PrimitiveTypeEnumeration`).
#[derive(Debug, Clone, PartialEq)]
enum Prim {
    Bool(u8),
    Byte(u8),
    Char(char),
    Decimal(Lps),
    Double(u64),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    SByte(i8),
    Single(u32),
    TimeSpan(i64),
    DateTime(i64),
    UInt16(u16),
    UInt32(u32),
    UInt64(u64),
    Null,
    Str(Lps),
}

fn valid_prim_code(code: u8) -> bool {
    matches!(code, 1..=3 | 5..=18)
}

/// Minimum encoded size of a primitive (for allocation guards).
fn prim_min_size(code: u8) -> usize {
    match code {
        7 | 14 => 2,
        8 | 11 | 15 => 4,
        6 | 9 | 12 | 13 | 16 => 8,
        17 => 0,
        _ => 1,
    }
}

fn prim_name(code: u8) -> &'static str {
    match code {
        1 => "Boolean",
        2 => "Byte",
        3 => "Char",
        5 => "Decimal",
        6 => "Double",
        7 => "Int16",
        8 => "Int32",
        9 => "Int64",
        10 => "SByte",
        11 => "Single",
        12 => "TimeSpan",
        13 => "DateTime",
        14 => "UInt16",
        15 => "UInt32",
        16 => "UInt64",
        17 => "Null",
        18 => "String",
        _ => "invalid",
    }
}

fn is_decimal_text(s: &str) -> bool {
    let body = s.strip_prefix('-').unwrap_or(s);
    let (int, frac) = match body.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (body, None),
    };
    !int.is_empty()
        && int.bytes().all(|b| b.is_ascii_digit())
        && frac.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
}

impl Prim {
    fn read(r: &mut ByteReader<'_>, code: u8) -> CodecResult<Prim> {
        Ok(match code {
            1 => Prim::Bool(r.u8("Boolean")?),
            2 => Prim::Byte(r.u8("Byte")?),
            3 => {
                let start = r.pos();
                let first = r.u8("Char")?;
                let len = match first {
                    0x00..=0x7f => 1,
                    0xc0..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf7 => 4,
                    _ => return Err(CodecError::malformed(CODEC, start, "invalid UTF-8 lead byte in Char")),
                };
                let mut buf = vec![first];
                buf.extend_from_slice(r.take(len - 1, "Char")?);
                let ch = std::str::from_utf8(&buf)
                    .ok()
                    .and_then(|s| s.chars().next())
                    .ok_or_else(|| CodecError::malformed(CODEC, start, "Char is not valid UTF-8"))?;
                Prim::Char(ch)
            }
            5 => Prim::Decimal(Lps::read(r)?),
            6 => Prim::Double(r.u64_le("Double")?),
            7 => Prim::Int16(r.i16_le("Int16")?),
            8 => Prim::Int32(r.i32_le("Int32")?),
            9 => Prim::Int64(r.i64_le("Int64")?),
            10 => Prim::SByte(r.i8("SByte")?),
            11 => Prim::Single(r.u32_le("Single")?),
            12 => Prim::TimeSpan(r.i64_le("TimeSpan")?),
            13 => Prim::DateTime(r.i64_le("DateTime")?),
            14 => Prim::UInt16(r.u16_le("UInt16")?),
            15 => Prim::UInt32(r.u32_le("UInt32")?),
            16 => Prim::UInt64(r.u64_le("UInt64")?),
            17 => Prim::Null,
            18 => Prim::Str(Lps::read(r)?),
            other => return Err(r.malformed(format!("invalid PrimitiveTypeEnumeration {other}"))),
        })
    }

    fn code(&self) -> u8 {
        match self {
            Prim::Bool(_) => 1,
            Prim::Byte(_) => 2,
            Prim::Char(_) => 3,
            Prim::Decimal(_) => 5,
            Prim::Double(_) => 6,
            Prim::Int16(_) => 7,
            Prim::Int32(_) => 8,
            Prim::Int64(_) => 9,
            Prim::SByte(_) => 10,
            Prim::Single(_) => 11,
            Prim::TimeSpan(_) => 12,
            Prim::DateTime(_) => 13,
            Prim::UInt16(_) => 14,
            Prim::UInt32(_) => 15,
            Prim::UInt64(_) => 16,
            Prim::Null => 17,
            Prim::Str(_) => 18,
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        match self {
            Prim::Bool(b) | Prim::Byte(b) => out.put_u8(*b),
            Prim::Char(c) => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            Prim::Decimal(l) | Prim::Str(l) => l.write(out),
            Prim::Double(bits) | Prim::UInt64(bits) => out.put_u64_le(*bits),
            Prim::Int16(v) => out.extend_from_slice(&v.to_le_bytes()),
            Prim::Int32(v) => out.put_i32_le(*v),
            Prim::Int64(v) | Prim::TimeSpan(v) | Prim::DateTime(v) => out.put_i64_le(*v),
            Prim::SByte(v) => out.extend_from_slice(&v.to_le_bytes()),
            Prim::Single(bits) | Prim::UInt32(bits) => out.put_u32_le(*bits),
            Prim::UInt16(v) => out.put_u16_le(*v),
            Prim::Null => {}
        }
    }

    fn to_node(&self) -> Node {
        match self {
            Prim::Bool(b) => Node::Bool(*b != 0),
            Prim::Byte(v) => Node::U8(*v),
            Prim::Char(c) => Node::String(c.to_string()),
            Prim::Decimal(l) | Prim::Str(l) => Node::String(l.text.clone()),
            Prim::Double(bits) => Node::F64(f64::from_bits(*bits)),
            Prim::Int16(v) => Node::I16(*v),
            Prim::Int32(v) => Node::I32(*v),
            Prim::Int64(v) | Prim::TimeSpan(v) | Prim::DateTime(v) => Node::I64(*v),
            Prim::SByte(v) => Node::I8(*v),
            Prim::Single(bits) => Node::F32(f32::from_bits(*bits)),
            Prim::UInt16(v) => Node::U16(*v),
            Prim::UInt32(v) => Node::U32(*v),
            Prim::UInt64(v) => Node::U64(*v),
            Prim::Null => Node::Null,
        }
    }

    /// Stores an (already type-coerced) node.
    fn assign(&mut self, node: &Node) -> Result<(), String> {
        match (&mut *self, node) {
            (Prim::Bool(b), Node::Bool(v)) => {
                if (*b != 0) != *v {
                    *b = u8::from(*v);
                }
            }
            (Prim::Byte(x), Node::U8(v)) => *x = *v,
            (Prim::SByte(x), Node::I8(v)) => *x = *v,
            (Prim::Int16(x), Node::I16(v)) => *x = *v,
            (Prim::Int32(x), Node::I32(v)) => *x = *v,
            (Prim::Int64(x) | Prim::TimeSpan(x) | Prim::DateTime(x), Node::I64(v)) => *x = *v,
            (Prim::UInt16(x), Node::U16(v)) => *x = *v,
            (Prim::UInt32(x), Node::U32(v)) => *x = *v,
            (Prim::UInt64(x), Node::U64(v)) => *x = *v,
            (Prim::Single(bits), Node::F32(v)) => *bits = v.to_bits(),
            (Prim::Double(bits), Node::F64(v)) => *bits = v.to_bits(),
            (Prim::Char(c), Node::String(s)) => {
                let mut it = s.chars();
                match (it.next(), it.next()) {
                    (Some(ch), None) if (ch as u32) <= 0xffff => *c = ch,
                    _ => return Err("a .NET Char holds exactly one BMP character".into()),
                }
            }
            (Prim::Decimal(l), Node::String(s)) => {
                if !is_decimal_text(s) {
                    return Err(format!("`{s}` is not a decimal number (expected e.g. -123.45)"));
                }
                l.set(s)?;
            }
            (Prim::Str(l), Node::String(s)) => l.set(s)?,
            (Prim::Null, _) => return Err("a null primitive cannot be edited".into()),
            (p, n) => return Err(format!("cannot store {} in a {} field", n.kind().name(), prim_name(p.code()))),
        }
        Ok(())
    }
}

/// `BinaryTypeEnumeration` with its additional info.
#[derive(Debug, Clone, PartialEq)]
enum BinType {
    Primitive(u8),
    String,
    Object,
    SystemClass(Lps),
    Class(Lps, i32),
    ObjectArray,
    StringArray,
    PrimitiveArray(u8),
}

impl BinType {
    fn code(&self) -> u8 {
        match self {
            BinType::Primitive(_) => 0,
            BinType::String => 1,
            BinType::Object => 2,
            BinType::SystemClass(_) => 3,
            BinType::Class(..) => 4,
            BinType::ObjectArray => 5,
            BinType::StringArray => 6,
            BinType::PrimitiveArray(_) => 7,
        }
    }

    /// Reads the additional info for a type code.
    fn read_info(r: &mut ByteReader<'_>, code: u8) -> CodecResult<BinType> {
        let prim = |r: &mut ByteReader<'_>| -> CodecResult<u8> {
            let p = r.u8("primitive type")?;
            if !valid_prim_code(p) {
                return Err(r.malformed(format!("invalid PrimitiveTypeEnumeration {p}")));
            }
            Ok(p)
        };
        Ok(match code {
            0 => BinType::Primitive(prim(r)?),
            1 => BinType::String,
            2 => BinType::Object,
            3 => BinType::SystemClass(Lps::read(r)?),
            4 => {
                let name = Lps::read(r)?;
                let lib = r.i32_le("ClassTypeInfo library id")?;
                BinType::Class(name, lib)
            }
            5 => BinType::ObjectArray,
            6 => BinType::StringArray,
            7 => BinType::PrimitiveArray(prim(r)?),
            other => return Err(r.malformed(format!("invalid BinaryTypeEnumeration {other}"))),
        })
    }

    fn write_info(&self, out: &mut Vec<u8>) {
        match self {
            BinType::Primitive(p) | BinType::PrimitiveArray(p) => out.put_u8(*p),
            BinType::SystemClass(n) => n.write(out),
            BinType::Class(n, lib) => {
                n.write(out);
                out.put_i32_le(*lib);
            }
            BinType::String | BinType::Object | BinType::ObjectArray | BinType::StringArray => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ClassInfo {
    object_id: i32,
    name: Lps,
    member_count: i32,
    member_names: Vec<Lps>,
}

/// A member value or array element.
#[derive(Debug, Clone, PartialEq)]
enum Value {
    /// Inline primitive (`MemberPrimitiveUnTyped`).
    Prim(Prim),
    /// An inline record, optionally preceded by `BinaryLibrary` records.
    Rec { libs: Vec<usize>, rec: usize },
}

#[derive(Debug, Clone, PartialEq)]
enum Record {
    Header {
        root_id: i32,
        header_id: i32,
        major: i32,
        minor: i32,
    },
    ClassWithId {
        object_id: i32,
        metadata_id: i32,
        /// Arena index of the record holding the class metadata.
        meta: usize,
        values: Vec<Value>,
    },
    /// Record types 2, 3, 4 and 5.
    Class {
        rtype: u8,
        info: ClassInfo,
        types: Option<Vec<BinType>>,
        library_id: Option<i32>,
        values: Vec<Value>,
    },
    ObjectString {
        object_id: i32,
        value: Lps,
    },
    BinaryArray {
        object_id: i32,
        array_type: u8,
        rank: i32,
        lengths: Vec<i32>,
        lower_bounds: Option<Vec<i32>>,
        elem: BinType,
        values: Vec<Value>,
    },
    MemberPrimitiveTyped(Prim),
    MemberReference(i32),
    ObjectNull,
    MessageEnd,
    BinaryLibrary {
        library_id: i32,
        name: Lps,
    },
    NullMultiple256(u8),
    NullMultiple(i32),
    ArraySinglePrimitive {
        object_id: i32,
        length: i32,
        prim: u8,
        values: Vec<Prim>,
    },
    /// Record types 16 (`ArraySingleObject`) and 17 (`ArraySingleString`).
    ArraySingle {
        rtype: u8,
        object_id: i32,
        length: i32,
        values: Vec<Value>,
    },
}

impl Record {
    fn object_id(&self) -> Option<i32> {
        match self {
            Record::ClassWithId { object_id, .. }
            | Record::ObjectString { object_id, .. }
            | Record::BinaryArray { object_id, .. }
            | Record::ArraySinglePrimitive { object_id, .. }
            | Record::ArraySingle { object_id, .. } => Some(*object_id),
            Record::Class { info, .. } => Some(info.object_id),
            _ => None,
        }
    }

    fn null_slots(&self) -> Option<usize> {
        match self {
            Record::NullMultiple256(n) => Some(*n as usize),
            Record::NullMultiple(n) => Some((*n).max(0) as usize),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Class metadata needed to read member values.
#[derive(Debug, Clone)]
struct Meta {
    rec: usize,
    types: Option<Vec<BinType>>,
    count: usize,
}

struct Parser<'a> {
    r: ByteReader<'a>,
    records: Vec<Record>,
    ids: HashMap<i32, usize>,
    metas: HashMap<i32, Meta>,
    references: Vec<(usize, i32)>,
    depth: usize,
    null_slots: usize,
}

/// Where a record is being read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    TopLevel,
    Member,
    Element,
}

impl<'a> Parser<'a> {
    fn register_id(&mut self, id: i32, idx: usize, at: usize) -> CodecResult<()> {
        if self.ids.insert(id, idx).is_some() {
            return Err(CodecError::malformed(CODEC, at, format!("duplicate object id {id}")));
        }
        Ok(())
    }

    fn read_class_info(&mut self) -> CodecResult<ClassInfo> {
        let object_id = self.r.i32_le("ClassInfo object id")?;
        let name = Lps::read(&mut self.r)?;
        let member_count = self.r.i32_le("ClassInfo member count")?;
        if member_count < 0 {
            return Err(self.r.malformed(format!("negative member count {member_count}")));
        }
        let count = member_count as usize;
        self.r.check_count(count, 1, "member names")?;
        let mut member_names = Vec::with_capacity(count);
        for _ in 0..count {
            member_names.push(Lps::read(&mut self.r)?);
        }
        Ok(ClassInfo {
            object_id,
            name,
            member_count,
            member_names,
        })
    }

    fn read_member_types(&mut self, count: usize) -> CodecResult<Vec<BinType>> {
        self.r.check_count(count, 1, "member types")?;
        let codes = self.r.take(count, "member type codes")?.to_vec();
        codes.into_iter().map(|c| BinType::read_info(&mut self.r, c)).collect()
    }

    /// Reads the member values of a class.
    fn read_members(&mut self, types: Option<&[BinType]>, count: usize) -> CodecResult<Vec<Value>> {
        let mut values = Vec::with_capacity(count.min(self.r.remaining()));
        for i in 0..count {
            match types.and_then(|t| t.get(i)) {
                Some(BinType::Primitive(code)) => values.push(Value::Prim(Prim::read(&mut self.r, *code)?)),
                _ => values.push(self.read_value_record(Ctx::Member)?),
            }
        }
        Ok(values)
    }

    /// Reads one member/element record, collecting preceding libraries.
    fn read_value_record(&mut self, ctx: Ctx) -> CodecResult<Value> {
        let mut libs = Vec::new();
        loop {
            let at = self.r.pos();
            let idx = self.read_record(ctx)?;
            match &self.records[idx] {
                Record::BinaryLibrary { .. } => libs.push(idx),
                Record::Header { .. } | Record::MessageEnd => {
                    return Err(CodecError::malformed(CODEC, at, "header / message end record in a member position"));
                }
                Record::NullMultiple256(_) | Record::NullMultiple(_) if ctx != Ctx::Element => {
                    return Err(CodecError::malformed(CODEC, at, "ObjectNullMultiple outside of an array"));
                }
                _ => return Ok(Value::Rec { libs, rec: idx }),
            }
        }
    }

    /// Reads `total` non-primitive array elements (null runs count for
    /// several elements).
    fn read_elements(&mut self, total: usize) -> CodecResult<Vec<Value>> {
        let mut values = Vec::new();
        let mut filled = 0usize;
        while filled < total {
            let at = self.r.pos();
            let v = self.read_value_record(Ctx::Element)?;
            let n = match &v {
                Value::Rec { rec, .. } => match self.records[*rec].null_slots() {
                    Some(n) => {
                        self.null_slots = self.null_slots.saturating_add(n);
                        if self.null_slots > MAX_NULL_SLOTS {
                            return Err(CodecError::malformed(CODEC, at, "too many null array slots"));
                        }
                        n
                    }
                    None => 1,
                },
                Value::Prim(_) => 1,
            };
            filled = filled.saturating_add(n);
            if filled > total {
                return Err(CodecError::malformed(
                    CODEC,
                    at,
                    format!("null run overflows the array ({filled} elements for length {total})"),
                ));
            }
            values.push(v);
        }
        Ok(values)
    }

    fn read_length(&mut self, ctx: &str) -> CodecResult<i32> {
        let len = self.r.i32_le(ctx)?;
        if len < 0 {
            return Err(self.r.malformed(format!("{ctx}: negative length {len}")));
        }
        Ok(len)
    }

    fn read_record(&mut self, ctx: Ctx) -> CodecResult<usize> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.r.malformed("records nested too deeply"));
        }
        let res = self.read_record_inner(ctx);
        self.depth -= 1;
        res
    }

    fn read_record_inner(&mut self, ctx: Ctx) -> CodecResult<usize> {
        let at = self.r.pos();
        let rtype = self.r.u8("record type")?;
        let idx = self.records.len();
        self.records.push(Record::ObjectNull);
        let rec = match rtype {
            0 => return Err(CodecError::malformed(CODEC, at, "unexpected SerializedStreamHeader")),
            1 => {
                let object_id = self.r.i32_le("ClassWithId object id")?;
                let metadata_id = self.r.i32_le("ClassWithId metadata id")?;
                let meta = self.metas.get(&metadata_id).cloned().ok_or_else(|| {
                    CodecError::malformed(CODEC, at, format!("ClassWithId refers to unknown metadata id {metadata_id}"))
                })?;
                self.register_id(object_id, idx, at)?;
                let values = self.read_members(meta.types.as_deref(), meta.count)?;
                Record::ClassWithId {
                    object_id,
                    metadata_id,
                    meta: meta.rec,
                    values,
                }
            }
            2..=5 => {
                let info = self.read_class_info()?;
                let count = info.member_names.len();
                let types = if matches!(rtype, 4 | 5) { Some(self.read_member_types(count)?) } else { None };
                let library_id = if matches!(rtype, 3 | 5) { Some(self.r.i32_le("library id")?) } else { None };
                self.register_id(info.object_id, idx, at)?;
                self.metas.insert(
                    info.object_id,
                    Meta {
                        rec: idx,
                        types: types.clone(),
                        count,
                    },
                );
                let values = self.read_members(types.as_deref(), count)?;
                Record::Class {
                    rtype,
                    info,
                    types,
                    library_id,
                    values,
                }
            }
            6 => {
                let object_id = self.r.i32_le("BinaryObjectString object id")?;
                let value = Lps::read(&mut self.r)?;
                self.register_id(object_id, idx, at)?;
                Record::ObjectString { object_id, value }
            }
            7 => {
                let object_id = self.r.i32_le("BinaryArray object id")?;
                let array_type = self.r.u8("BinaryArray type")?;
                if array_type > 5 {
                    return Err(self.r.malformed(format!("invalid BinaryArrayTypeEnumeration {array_type}")));
                }
                let rank = self.r.i32_le("BinaryArray rank")?;
                if !(1..=MAX_RANK).contains(&rank) {
                    return Err(self.r.malformed(format!("invalid array rank {rank}")));
                }
                let mut lengths = Vec::with_capacity(rank as usize);
                for _ in 0..rank {
                    lengths.push(self.read_length("BinaryArray length")?);
                }
                let lower_bounds = if matches!(array_type, 3..=5) {
                    let mut lb = Vec::with_capacity(rank as usize);
                    for _ in 0..rank {
                        lb.push(self.r.i32_le("BinaryArray lower bound")?);
                    }
                    Some(lb)
                } else {
                    None
                };
                let code = self.r.u8("BinaryArray element type")?;
                let elem = BinType::read_info(&mut self.r, code)?;
                let total = lengths
                    .iter()
                    .try_fold(1usize, |acc, &l| acc.checked_mul(l as usize))
                    .ok_or_else(|| self.r.malformed("array element count overflows"))?;
                self.register_id(object_id, idx, at)?;
                let values = match &elem {
                    BinType::Primitive(p) => {
                        if *p == 17 {
                            return Err(self.r.malformed("array of Null primitives"));
                        }
                        self.r.check_count(total, prim_min_size(*p), "BinaryArray elements")?;
                        let mut v = Vec::with_capacity(total);
                        for _ in 0..total {
                            v.push(Value::Prim(Prim::read(&mut self.r, *p)?));
                        }
                        v
                    }
                    _ => self.read_elements(total)?,
                };
                Record::BinaryArray {
                    object_id,
                    array_type,
                    rank,
                    lengths,
                    lower_bounds,
                    elem,
                    values,
                }
            }
            8 => {
                let code = self.r.u8("MemberPrimitiveTyped type")?;
                if !valid_prim_code(code) {
                    return Err(self.r.malformed(format!("invalid PrimitiveTypeEnumeration {code}")));
                }
                Record::MemberPrimitiveTyped(Prim::read(&mut self.r, code)?)
            }
            9 => {
                let id = self.r.i32_le("MemberReference id")?;
                self.references.push((at, id));
                Record::MemberReference(id)
            }
            10 => Record::ObjectNull,
            11 => Record::MessageEnd,
            12 => {
                let library_id = self.r.i32_le("BinaryLibrary id")?;
                let name = Lps::read(&mut self.r)?;
                Record::BinaryLibrary { library_id, name }
            }
            13 => Record::NullMultiple256(self.r.u8("ObjectNullMultiple256 count")?),
            14 => {
                let n = self.r.i32_le("ObjectNullMultiple count")?;
                if n < 0 {
                    return Err(self.r.malformed(format!("negative null count {n}")));
                }
                Record::NullMultiple(n)
            }
            15 => {
                let object_id = self.r.i32_le("ArraySinglePrimitive object id")?;
                let length = self.read_length("ArraySinglePrimitive length")?;
                let prim = self.r.u8("ArraySinglePrimitive type")?;
                if !valid_prim_code(prim) || prim == 17 {
                    return Err(self.r.malformed(format!("invalid array primitive type {prim}")));
                }
                self.r.check_count(length as usize, prim_min_size(prim), "ArraySinglePrimitive elements")?;
                self.register_id(object_id, idx, at)?;
                let mut values = Vec::with_capacity(length as usize);
                for _ in 0..length {
                    values.push(Prim::read(&mut self.r, prim)?);
                }
                Record::ArraySinglePrimitive {
                    object_id,
                    length,
                    prim,
                    values,
                }
            }
            16 | 17 => {
                let object_id = self.r.i32_le("ArraySingle object id")?;
                let length = self.read_length("ArraySingle length")?;
                self.register_id(object_id, idx, at)?;
                let values = self.read_elements(length as usize)?;
                Record::ArraySingle {
                    rtype,
                    object_id,
                    length,
                    values,
                }
            }
            21 | 22 => {
                return Err(CodecError::unsupported(
                    CODEC,
                    format!(
                        "{} record (remoting call/return) at offset {at:#x} never appears in save files",
                        if rtype == 21 { "BinaryMethodCall" } else { "BinaryMethodReturn" }
                    ),
                ))
            }
            other => return Err(CodecError::malformed(CODEC, at, format!("unknown record type {other}"))),
        };
        if ctx == Ctx::TopLevel && rec.null_slots().is_some() {
            return Err(CodecError::malformed(CODEC, at, "ObjectNullMultiple outside of an array"));
        }
        self.records[idx] = rec;
        Ok(idx)
    }
}

// ---------------------------------------------------------------------------
// Document
// ---------------------------------------------------------------------------

/// A parsed NRBF stream.
#[derive(Debug, Clone)]
pub struct NrbfDocument {
    records: Vec<Record>,
    /// Top-level records in stream order (header first, `MessageEnd` last).
    top: Vec<usize>,
    ids: HashMap<i32, usize>,
    root_id: i32,
    trailing: Vec<u8>,
}

/// Where an AST leaf lives in the record arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Loc {
    /// Inline primitive member `member` of class record `rec`.
    Member { rec: usize, member: usize },
    /// Primitive element `index` of array record `rec`.
    Elem { rec: usize, index: usize },
    /// Value of the `BinaryObjectString` record `rec`.
    Str { rec: usize },
    /// Value of the `MemberPrimitiveTyped` record `rec`.
    Typed { rec: usize },
}

struct Projector<'a> {
    doc: &'a NrbfDocument,
    visited: HashSet<i32>,
    binds: HashMap<NodePath, Loc>,
}

fn unique_key(map: &NodeMap, name: &str) -> String {
    if !map.contains_key(name) {
        return name.to_string();
    }
    (2..)
        .map(|n| format!("{name}#{n}"))
        .find(|k| !map.contains_key(k))
        .unwrap_or_else(|| name.to_string())
}

impl Projector<'_> {
    fn reference(id: i32) -> Node {
        Node::String(format!("$ref:{id}"))
    }

    fn value(&mut self, v: &Value, path: &NodePath, loc: Loc, depth: usize) -> Node {
        match v {
            Value::Prim(p) => {
                self.binds.insert(path.clone(), loc);
                p.to_node()
            }
            Value::Rec { rec, .. } => self.record(*rec, path, depth),
        }
    }

    fn record(&mut self, idx: usize, path: &NodePath, depth: usize) -> Node {
        let doc = self.doc;
        match &doc.records[idx] {
            Record::ObjectString { value, .. } => {
                self.binds.insert(path.clone(), Loc::Str { rec: idx });
                Node::String(value.text.clone())
            }
            Record::MemberPrimitiveTyped(p) => {
                self.binds.insert(path.clone(), Loc::Typed { rec: idx });
                p.to_node()
            }
            Record::MemberReference(id) => match doc.ids.get(id) {
                Some(&target) => match &doc.records[target] {
                    Record::ObjectString { .. } => self.record(target, path, depth),
                    _ => self.object(target, path, depth),
                },
                None => Self::reference(*id),
            },
            Record::ClassWithId { .. }
            | Record::Class { .. }
            | Record::BinaryArray { .. }
            | Record::ArraySinglePrimitive { .. }
            | Record::ArraySingle { .. } => self.object(idx, path, depth),
            _ => Node::Null,
        }
    }

    fn elements(&mut self, idx: usize, values: &[Value], path: &NodePath, depth: usize) -> Node {
        let mut out = Vec::with_capacity(values.len());
        for (i, v) in values.iter().enumerate() {
            if let Value::Rec { rec, .. } = v {
                if let Some(n) = self.doc.records[*rec].null_slots() {
                    out.resize(out.len() + n, Node::Null);
                    continue;
                }
            }
            let p = path.index(out.len());
            out.push(self.value(v, &p, Loc::Elem { rec: idx, index: i }, depth + 1));
        }
        Node::Array(out)
    }

    fn object(&mut self, idx: usize, path: &NodePath, depth: usize) -> Node {
        let doc = self.doc;
        let rec = &doc.records[idx];
        let id = rec.object_id().unwrap_or_default();
        if depth > MAX_DEPTH || !self.visited.insert(id) {
            return Self::reference(id);
        }
        match rec {
            Record::Class { info, values, .. } => self.class(idx, info, values, path, depth),
            Record::ClassWithId { meta, values, .. } => match &doc.records[*meta] {
                Record::Class { info, .. } => self.class(idx, info, values, path, depth),
                _ => Node::Null,
            },
            Record::ArraySinglePrimitive { values, .. } => {
                let mut out = Vec::with_capacity(values.len());
                for (i, p) in values.iter().enumerate() {
                    self.binds.insert(path.index(i), Loc::Elem { rec: idx, index: i });
                    out.push(p.to_node());
                }
                Node::Array(out)
            }
            Record::BinaryArray { values, .. } | Record::ArraySingle { values, .. } => self.elements(idx, values, path, depth),
            _ => Node::Null,
        }
    }

    fn class(&mut self, idx: usize, info: &ClassInfo, values: &[Value], path: &NodePath, depth: usize) -> Node {
        let mut map = NodeMap::new();
        map.insert("$type".to_string(), Node::String(info.name.text.clone()));
        for (i, v) in values.iter().enumerate() {
            let name = info.member_names.get(i).map(|n| n.text.as_str()).unwrap_or("member");
            let key = unique_key(&map, name);
            let p = path.key(key.clone());
            let node = self.value(v, &p, Loc::Member { rec: idx, member: i }, depth + 1);
            map.insert(key, node);
        }
        Node::Map(map)
    }
}

impl NrbfDocument {
    /// Parses an NRBF stream.
    pub fn parse(bytes: &[u8]) -> CodecResult<NrbfDocument> {
        let mut p = Parser {
            r: ByteReader::new(bytes, CODEC),
            records: Vec::new(),
            ids: HashMap::new(),
            metas: HashMap::new(),
            references: Vec::new(),
            depth: 0,
            null_slots: 0,
        };
        let rtype = p.r.u8("record type")?;
        if rtype != 0 {
            return Err(CodecError::malformed(CODEC, 0, "stream does not start with a SerializedStreamHeader"));
        }
        let root_id = p.r.i32_le("RootId")?;
        let header_id = p.r.i32_le("HeaderId")?;
        let major = p.r.i32_le("MajorVersion")?;
        let minor = p.r.i32_le("MinorVersion")?;
        if major != 1 || minor != 0 {
            return Err(CodecError::malformed(CODEC, 9, format!("unsupported NRBF version {major}.{minor}")));
        }
        p.records.push(Record::Header {
            root_id,
            header_id,
            major,
            minor,
        });
        let mut top = vec![0usize];
        loop {
            let idx = p.read_record(Ctx::TopLevel)?;
            top.push(idx);
            if matches!(p.records[idx], Record::MessageEnd) {
                break;
            }
        }
        for (at, id) in &p.references {
            if !p.ids.contains_key(id) {
                return Err(CodecError::malformed(CODEC, *at, format!("MemberReference to unknown object id {id}")));
            }
        }
        if !p.ids.contains_key(&root_id) {
            return Err(CodecError::malformed(CODEC, 1, format!("root object id {root_id} is not defined")));
        }
        let trailing = p.r.rest().to_vec();
        Ok(NrbfDocument {
            records: p.records,
            top,
            ids: p.ids,
            root_id,
            trailing,
        })
    }

    fn project(&self) -> (Node, HashMap<NodePath, Loc>) {
        let mut pr = Projector {
            doc: self,
            visited: HashSet::new(),
            binds: HashMap::new(),
        };
        let node = match self.ids.get(&self.root_id) {
            Some(&idx) => pr.record(idx, &NodePath::root(), 0),
            None => Node::Null,
        };
        (node, pr.binds)
    }

    /// Serialises the record stream.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for &idx in &self.top {
            self.write_record(idx, &mut out);
        }
        out.extend_from_slice(&self.trailing);
        out
    }

    fn write_values(&self, values: &[Value], out: &mut Vec<u8>) {
        for v in values {
            match v {
                Value::Prim(p) => p.write(out),
                Value::Rec { libs, rec } => {
                    for &l in libs {
                        self.write_record(l, out);
                    }
                    self.write_record(*rec, out);
                }
            }
        }
    }

    fn write_record(&self, idx: usize, out: &mut Vec<u8>) {
        let Some(rec) = self.records.get(idx) else { return };
        match rec {
            Record::Header {
                root_id,
                header_id,
                major,
                minor,
            } => {
                out.put_u8(0);
                for v in [root_id, header_id, major, minor] {
                    out.put_i32_le(*v);
                }
            }
            Record::ClassWithId {
                object_id,
                metadata_id,
                values,
                ..
            } => {
                out.put_u8(1);
                out.put_i32_le(*object_id);
                out.put_i32_le(*metadata_id);
                self.write_values(values, out);
            }
            Record::Class {
                rtype,
                info,
                types,
                library_id,
                values,
            } => {
                out.put_u8(*rtype);
                out.put_i32_le(info.object_id);
                info.name.write(out);
                out.put_i32_le(info.member_count);
                for n in &info.member_names {
                    n.write(out);
                }
                if let Some(types) = types {
                    for t in types {
                        out.put_u8(t.code());
                    }
                    for t in types {
                        t.write_info(out);
                    }
                }
                if let Some(lib) = library_id {
                    out.put_i32_le(*lib);
                }
                self.write_values(values, out);
            }
            Record::ObjectString { object_id, value } => {
                out.put_u8(6);
                out.put_i32_le(*object_id);
                value.write(out);
            }
            Record::BinaryArray {
                object_id,
                array_type,
                rank,
                lengths,
                lower_bounds,
                elem,
                values,
            } => {
                out.put_u8(7);
                out.put_i32_le(*object_id);
                out.put_u8(*array_type);
                out.put_i32_le(*rank);
                for l in lengths {
                    out.put_i32_le(*l);
                }
                if let Some(lb) = lower_bounds {
                    for l in lb {
                        out.put_i32_le(*l);
                    }
                }
                out.put_u8(elem.code());
                elem.write_info(out);
                self.write_values(values, out);
            }
            Record::MemberPrimitiveTyped(p) => {
                out.put_u8(8);
                out.put_u8(p.code());
                p.write(out);
            }
            Record::MemberReference(id) => {
                out.put_u8(9);
                out.put_i32_le(*id);
            }
            Record::ObjectNull => out.put_u8(10),
            Record::MessageEnd => out.put_u8(11),
            Record::BinaryLibrary { library_id, name } => {
                out.put_u8(12);
                out.put_i32_le(*library_id);
                name.write(out);
            }
            Record::NullMultiple256(n) => {
                out.put_u8(13);
                out.put_u8(*n);
            }
            Record::NullMultiple(n) => {
                out.put_u8(14);
                out.put_i32_le(*n);
            }
            Record::ArraySinglePrimitive {
                object_id,
                length,
                prim,
                values,
            } => {
                out.put_u8(15);
                out.put_i32_le(*object_id);
                out.put_i32_le(*length);
                out.put_u8(*prim);
                for p in values {
                    p.write(out);
                }
            }
            Record::ArraySingle {
                rtype,
                object_id,
                length,
                values,
            } => {
                out.put_u8(*rtype);
                out.put_i32_le(*object_id);
                out.put_i32_le(*length);
                self.write_values(values, out);
            }
        }
    }

    fn root_class_name(&self) -> Option<&str> {
        let idx = *self.ids.get(&self.root_id)?;
        let info_rec = match &self.records[idx] {
            Record::ClassWithId { meta, .. } => &self.records[*meta],
            other => other,
        };
        match info_rec {
            Record::Class { info, .. } => Some(info.name.text.as_str()),
            _ => None,
        }
    }

    /// Mutable access to the primitive / string behind a binding.
    fn slot_mut(&mut self, loc: Loc) -> Option<Slot<'_>> {
        match loc {
            Loc::Member { rec, member } => match self.records.get_mut(rec)? {
                Record::Class { values, .. } | Record::ClassWithId { values, .. } => match values.get_mut(member)? {
                    Value::Prim(p) => Some(Slot::Prim(p)),
                    Value::Rec { .. } => None,
                },
                _ => None,
            },
            Loc::Elem { rec, index } => match self.records.get_mut(rec)? {
                Record::ArraySinglePrimitive { values, .. } => values.get_mut(index).map(Slot::Prim),
                Record::BinaryArray { values, .. } | Record::ArraySingle { values, .. } => match values.get_mut(index)? {
                    Value::Prim(p) => Some(Slot::Prim(p)),
                    Value::Rec { .. } => None,
                },
                _ => None,
            },
            Loc::Str { rec } => match self.records.get_mut(rec)? {
                Record::ObjectString { value, .. } => Some(Slot::Str(value)),
                _ => None,
            },
            Loc::Typed { rec } => match self.records.get_mut(rec)? {
                Record::MemberPrimitiveTyped(p) => Some(Slot::Prim(p)),
                _ => None,
            },
        }
    }
}

enum Slot<'a> {
    Prim(&'a mut Prim),
    Str(&'a mut Lps),
}

/// Applies diff changes through the binding table.
struct Editor<'a> {
    doc: NrbfDocument,
    binds: HashMap<NodePath, Loc>,
    current: &'a Node,
    /// String records written during this edit, to detect conflicting edits
    /// of a shared string through different paths.
    strings_set: HashMap<usize, String>,
}

impl PathEditable for Editor<'_> {
    fn codec(&self) -> &'static str {
        CODEC
    }

    fn set_value(&mut self, path: &NodePath, value: &Node) -> CodecResult<()> {
        let existing = self.current.get(path).ok_or_else(|| CodecError::path_not_found(CODEC, path))?;
        let Some(&loc) = self.binds.get(path) else {
            let reason = match existing {
                Node::String(s) if s.starts_with("$ref:") => "object references are read-only".to_string(),
                _ if path.last().and_then(|s| s.as_key()) == Some("$type") => "the class name ($type) is read-only".to_string(),
                Node::Null => "null values cannot be replaced (no record holds a value)".to_string(),
                n if n.is_container() => "classes and arrays cannot be reshaped".to_string(),
                _ => "this value is read-only".to_string(),
            };
            return Err(CodecError::structure(CODEC, path, reason));
        };
        let v = coerce_like(CODEC, path, existing, value)?;
        if let Loc::Str { rec } = loc {
            if let Node::String(s) = &v {
                if let Some(prev) = self.strings_set.get(&rec) {
                    if prev != s {
                        return Err(CodecError::unencodable(
                            CODEC,
                            path,
                            "this string object is shared by several members; conflicting values were given for its occurrences",
                        ));
                    }
                }
                self.strings_set.insert(rec, s.clone());
            }
        }
        let slot = self
            .doc
            .slot_mut(loc)
            .ok_or_else(|| CodecError::path_not_found(CODEC, path))?;
        let res = match slot {
            Slot::Prim(p) => p.assign(&v),
            Slot::Str(l) => match &v {
                Node::String(s) => l.set(s),
                other => Err(format!("cannot store {} in a string", other.kind().name())),
            },
        };
        res.map_err(|reason| CodecError::unencodable(CODEC, path, reason))
    }
}

impl Document for NrbfDocument {
    fn codec_id(&self) -> &'static str {
        CODEC
    }

    fn to_ast(&self) -> Node {
        self.project().0
    }

    fn apply_ast(&mut self, root: &Node) -> CodecResult<()> {
        let (current, binds) = self.project();
        let mut ed = Editor {
            doc: self.clone(),
            binds,
            current: &current,
            strings_set: HashMap::new(),
        };
        apply_via_diff(&mut ed, &current, root)?;
        *self = ed.doc;
        Ok(())
    }

    fn encode(&self) -> CodecResult<Vec<u8>> {
        Ok(self.serialize())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::VALUES_ONLY
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = vec![format!(
            "NRBF stream: {} records, root object {} ({})",
            self.records.len(),
            self.root_id,
            self.root_class_name().unwrap_or("not a class")
        )];
        let libs: Vec<&str> = self
            .records
            .iter()
            .filter_map(|r| match r {
                Record::BinaryLibrary { name, .. } => Some(name.text.as_str()),
                _ => None,
            })
            .collect();
        if !libs.is_empty() {
            notes.push(format!("libraries: {}", libs.join("; ")));
        }
        let mut shared: HashMap<i32, usize> = HashMap::new();
        for r in &self.records {
            if let Record::MemberReference(id) = r {
                if let Some(&t) = self.ids.get(id) {
                    if matches!(self.records[t], Record::ObjectString { .. }) {
                        *shared.entry(*id).or_default() += 1;
                    }
                }
            }
        }
        if !shared.is_empty() {
            notes.push(format!(
                "{} string object(s) are referenced from several places: editing one occurrence changes the string everywhere it is referenced",
                shared.len()
            ));
        }
        let non_minimal = self
            .records
            .iter()
            .filter(|r| matches!(r, Record::ObjectString { value, .. } if !value.is_minimal()))
            .count();
        if non_minimal > 0 {
            notes.push(format!("{non_minimal} string(s) use a non-minimal length prefix (kept unless edited)"));
        }
        if self.records.iter().any(|r| matches!(r, Record::BinaryArray { rank, .. } if *rank > 1)) {
            notes.push("multi-dimensional arrays are shown flattened in row-major order".into());
        }
        if !self.trailing.is_empty() {
            notes.push(format!("{} bytes after MessageEnd kept verbatim", self.trailing.len()));
        }
        notes
    }

    fn clone_box(&self) -> Box<dyn Document> {
        Box::new(self.clone())
    }
}

/// The NRBF codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct NrbfCodec;

impl StructuralCodec for NrbfCodec {
    fn id(&self) -> &'static str {
        CODEC
    }

    fn name(&self) -> &'static str {
        ".NET BinaryFormatter (NRBF)"
    }

    fn detect(&self, bytes: &[u8], _ctx: &DetectContext) -> Confidence {
        if bytes.len() < 18 || bytes[0] != 0 || bytes[9..17] != [1, 0, 0, 0, 0, 0, 0, 0] {
            return 0;
        }
        if NrbfDocument::parse(bytes).is_ok() {
            95
        } else {
            0
        }
    }

    fn decode(&self, bytes: &[u8], _ctx: &DetectContext) -> CodecResult<Box<dyn Document>> {
        Ok(Box::new(NrbfDocument::parse(bytes)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lps(s: &str) -> Vec<u8> {
        let mut v = encode_varint(s.len());
        v.extend_from_slice(s.as_bytes());
        v
    }

    fn header(root: i32) -> Vec<u8> {
        let mut v = vec![0u8];
        for x in [root, -1, 1, 0] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v
    }

    /// Root class `C` with a Char, a Decimal, a DateTime, a Double and an
    /// `Object` member holding a boxed Int16.
    fn sample() -> Vec<u8> {
        let mut b = header(1);
        b.push(4); // SystemClassWithMembersAndTypes
        b.extend_from_slice(&1i32.to_le_bytes());
        b.extend(lps("C"));
        b.extend_from_slice(&5i32.to_le_bytes());
        for n in ["c", "d", "t", "x", "o"] {
            b.extend(lps(n));
        }
        b.extend_from_slice(&[0, 0, 0, 0, 2]);
        b.extend_from_slice(&[3, 5, 13, 6]);
        b.extend("\u{e9}".as_bytes());
        b.extend(lps("-12.50"));
        b.extend_from_slice(&0x48d0_0000_0000_0001u64.to_le_bytes());
        b.extend_from_slice(&f64::NAN.to_bits().wrapping_add(7).to_le_bytes());
        b.push(8);
        b.push(7);
        b.extend_from_slice(&(-3i16).to_le_bytes());
        b.push(11);
        b
    }

    #[test]
    fn varint_roundtrip() {
        assert_eq!(encode_varint(0), vec![0]);
        assert_eq!(encode_varint(127), vec![0x7f]);
        assert_eq!(encode_varint(128), vec![0x80, 0x01]);
        assert_eq!(encode_varint(300), vec![0xac, 0x02]);
        let data = [0x83u8, 0x80, 0x00, b'a', b'b', b'c'];
        let mut r = ByteReader::new(&data, CODEC);
        let l = Lps::read(&mut r).unwrap();
        assert_eq!(l.text, "abc");
        assert!(!l.is_minimal());
        let mut out = Vec::new();
        l.write(&mut out);
        assert_eq!(out, data);
        let bad = [0x80u8, 0x80, 0x80, 0x80, 0x80, 0x01];
        assert!(Lps::read(&mut ByteReader::new(&bad, CODEC)).is_err());
    }

    #[test]
    fn primitives_roundtrip_and_project() {
        let bytes = sample();
        let doc = NrbfDocument::parse(&bytes).unwrap();
        assert_eq!(doc.serialize(), bytes);
        let ast = doc.to_ast();
        let get = |k: &str| ast.get(&NodePath::root().key(k)).cloned().unwrap();
        assert_eq!(get("$type"), Node::String("C".into()));
        assert_eq!(get("c"), Node::String("\u{e9}".into()));
        assert_eq!(get("d"), Node::String("-12.50".into()));
        assert_eq!(get("t"), Node::I64(0x48d0_0000_0000_0001));
        assert!(matches!(get("x"), Node::F64(x) if x.is_nan()));
        assert_eq!(get("o"), Node::I16(-3));
    }

    #[test]
    fn edits_char_decimal_boxed() {
        let mut doc = NrbfDocument::parse(&sample()).unwrap();
        let mut ast = doc.to_ast();
        ast.set(&NodePath::root().key("c"), Node::String("Z".into()), false).unwrap();
        ast.set(&NodePath::root().key("d"), Node::String("1000.125".into()), false).unwrap();
        ast.set(&NodePath::root().key("o"), Node::I64(1234), false).unwrap();
        doc.apply_ast(&ast).unwrap();
        let re = NrbfDocument::parse(&doc.serialize()).unwrap().to_ast();
        assert!(savelens_core::bit_equal(&re, &doc.to_ast()));
        assert_eq!(re.get(&NodePath::root().key("c")), Some(&Node::String("Z".into())));
        assert_eq!(re.get(&NodePath::root().key("d")), Some(&Node::String("1000.125".into())));
        assert_eq!(re.get(&NodePath::root().key("o")), Some(&Node::I16(1234)));

        for (k, v) in [
            ("c", Node::String("ab".into())),
            ("d", Node::String("1e5".into())),
            ("o", Node::I64(70000)),
            ("$type", Node::String("D".into())),
        ] {
            let mut ast = doc.to_ast();
            ast.set(&NodePath::root().key(k), v, false).unwrap();
            assert!(doc.apply_ast(&ast).is_err(), "{k}");
        }
        let mut ast = doc.to_ast();
        ast.set(&NodePath::root().key("new"), Node::I32(1), true).unwrap();
        assert!(matches!(doc.apply_ast(&ast), Err(CodecError::StructureUnsupported { .. })));
    }

    #[test]
    fn rejects_bad_streams() {
        assert!(NrbfDocument::parse(&[]).is_err());
        assert!(NrbfDocument::parse(&[1, 2, 3]).is_err());
        // Version 2.0.
        let mut b = vec![0u8];
        for x in [1i32, -1, 2, 0] {
            b.extend_from_slice(&x.to_le_bytes());
        }
        b.push(11);
        assert!(NrbfDocument::parse(&b).is_err());
        // Root not defined.
        let mut b = header(1);
        b.push(11);
        assert!(NrbfDocument::parse(&b).is_err());
        // Method call record.
        let mut b = header(1);
        b.push(21);
        b.extend_from_slice(&[0; 16]);
        assert!(matches!(NrbfDocument::parse(&b), Err(CodecError::Unsupported { .. })));
        // Huge array length is refused before allocating.
        let mut b = header(1);
        b.push(15);
        b.extend_from_slice(&1i32.to_le_bytes());
        b.extend_from_slice(&i32::MAX.to_le_bytes());
        b.push(8);
        assert!(NrbfDocument::parse(&b).is_err());
        // Null run larger than the array.
        let mut b = header(1);
        b.push(16);
        b.extend_from_slice(&1i32.to_le_bytes());
        b.extend_from_slice(&2i32.to_le_bytes());
        b.extend_from_slice(&[13, 3, 11]);
        assert!(NrbfDocument::parse(&b).is_err());
        // Every truncation fails cleanly.
        let s = sample();
        for n in 0..s.len() {
            assert!(NrbfDocument::parse(&s[..n]).is_err());
        }
    }

    #[test]
    fn cycles_project_as_refs_and_rectangular_arrays_flatten() {
        let mut b = header(1);
        // class Node { Object next; }  root -> ref 2 -> ref 1 (cycle)
        b.push(2); // SystemClassWithMembers (no types)
        b.extend_from_slice(&1i32.to_le_bytes());
        b.extend(lps("Node"));
        b.extend_from_slice(&2i32.to_le_bytes());
        b.extend(lps("next"));
        b.extend(lps("grid"));
        b.push(9);
        b.extend_from_slice(&2i32.to_le_bytes());
        b.push(9);
        b.extend_from_slice(&3i32.to_le_bytes());
        b.push(1); // ClassWithId
        b.extend_from_slice(&2i32.to_le_bytes());
        b.extend_from_slice(&1i32.to_le_bytes());
        b.push(9);
        b.extend_from_slice(&1i32.to_le_bytes());
        b.push(10);
        // 2x2 rectangular Int16 array with lower bounds.
        b.push(7);
        b.extend_from_slice(&3i32.to_le_bytes());
        b.push(5);
        b.extend_from_slice(&2i32.to_le_bytes());
        for x in [2i32, 2, 1, 1] {
            b.extend_from_slice(&x.to_le_bytes());
        }
        b.extend_from_slice(&[0, 7]);
        for x in [1i16, 2, 3, 4] {
            b.extend_from_slice(&x.to_le_bytes());
        }
        b.push(11);
        b.extend_from_slice(b"tail");
        let mut doc = NrbfDocument::parse(&b).unwrap();
        assert_eq!(doc.serialize(), b);
        let ast = doc.to_ast();
        assert_eq!(ast.get(&NodePath::parse("next.next").unwrap()), Some(&Node::String("$ref:1".into())));
        assert_eq!(ast.get(&NodePath::parse("next.grid").unwrap()), Some(&Node::Null));
        assert_eq!(
            ast.get(&NodePath::parse("grid").unwrap()),
            Some(&Node::Array(vec![Node::I16(1), Node::I16(2), Node::I16(3), Node::I16(4)]))
        );
        assert!(doc.notes().iter().any(|n| n.contains("flattened")));
        assert!(doc.notes().iter().any(|n| n.contains("after MessageEnd")));
        let mut edited = ast.clone();
        edited.set(&NodePath::parse("grid[3]").unwrap(), Node::I64(-9), false).unwrap();
        doc.apply_ast(&edited).unwrap();
        edited.set(&NodePath::parse("grid[3]").unwrap(), Node::I16(-9), false).unwrap();
        assert_eq!(doc.to_ast(), edited);
        let mut bad = edited.clone();
        bad.set(&NodePath::parse("next.next").unwrap(), Node::String("x".into()), false).unwrap();
        assert!(doc.apply_ast(&bad).is_err());
        let mut bad = edited;
        bad.set(&NodePath::parse("next.grid").unwrap(), Node::I32(1), false).unwrap();
        assert!(doc.apply_ast(&bad).is_err());
    }

    #[test]
    fn detect() {
        let c = NrbfCodec;
        let ctx = DetectContext::default();
        assert_eq!(c.detect(&sample(), &ctx), 95);
        assert_eq!(c.detect(b"{\"a\":1}", &ctx), 0);
        let s = sample();
        assert_eq!(c.detect(&s[..s.len() - 1], &ctx), 0);
    }
}
