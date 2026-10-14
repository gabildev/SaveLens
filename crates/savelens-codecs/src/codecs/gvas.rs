//! Unreal Engine 4/5 GVAS save games (`.sav`, `USaveGame` serialisation).
//!
//! The parser builds a codec-native document that keeps every layout detail
//! of the file: the header (engine version, custom versions, save-game
//! class), the exact encoding of every `FString` (empty / 8-bit / UTF-16),
//! property GUIDs, array indices, struct GUIDs, the inner tag of struct
//! arrays, removed-entry lists of sets and maps, raw bool bytes and any
//! trailing bytes after the root property list.
//!
//! Two property-tag layouts are supported:
//!
//! * the classic tag (UE4 and UE5 before 5.4), including the property
//!   extension byte introduced with `PackageFileUE5Version` 1011;
//! * the UE 5.4+ "complete type name" tag (`PackageFileUE5Version` >= 1012):
//!   a recursive `FPropertyTypeName` and a flags byte.
//!
//! **Losslessness**: every property value is parsed from exactly `Size`
//! bytes and re-encoded immediately; if the parse does not consume the whole
//! value, the type is unknown (`TextProperty`, delegates, field paths, …) or
//! the re-encoded bytes differ, the value is kept as raw bytes
//! ([`Node::Bytes`], same-length edits only). Sizes (including the inner
//! struct-array tag size) are recomputed on encode, so string edits that
//! change lengths produce a consistent file. Tag constructs that cannot be
//! parsed with certainty make [`GvasCodec::decode`] fail with
//! [`CodecError::Unsupported`], which lets the pipeline fall back to the raw
//! codec.

use std::collections::HashSet;

use savelens_core::{bit_equal, Endianness, Node, NodeMap, NodePath};

use crate::document::{coerce_like, Capabilities, Confidence, DetectContext, Document, StructuralCodec};
use crate::error::{CodecError, CodecResult};
use crate::io::{ByteReader, WriteExt};

const CODEC: &str = "gvas";
const MAGIC: &[u8; 4] = b"GVAS";
/// Maximum nesting of property lists (structs inside structs).
const MAX_DEPTH: usize = 64;
/// Maximum nesting of a UE 5.4 `FPropertyTypeName`.
const MAX_TYPE_DEPTH: usize = 16;
/// `EUnrealEngineObjectUE5Version::LARGE_WORLD_COORDINATES`.
const UE5_LARGE_WORLD_COORDINATES: i32 = 1004;
/// `EUnrealEngineObjectUE5Version::PROPERTY_TAG_EXTENSION_AND_OVERRIDABLE_SERIALIZATION`.
const UE5_PROPERTY_TAG_EXTENSION: i32 = 1011;
/// `EUnrealEngineObjectUE5Version::PROPERTY_TAG_COMPLETE_TYPE_NAME`.
const UE5_PROPERTY_TAG_COMPLETE_TYPE_NAME: i32 = 1012;

/// UE 5.4 `EPropertyTagFlags`.
const FLAG_HAS_ARRAY_INDEX: u8 = 0x01;
const FLAG_HAS_PROPERTY_GUID: u8 = 0x02;
const FLAG_HAS_PROPERTY_EXTENSIONS: u8 = 0x04;
const FLAG_BOOL_TRUE: u8 = 0x10;
/// Every flag bit known to this codec (0x08 binary/native serialize and 0x20
/// skipped serialize only affect the value, which is size-delimited).
const KNOWN_FLAGS: u8 = 0x3f;

// ---------------------------------------------------------------------------
// FString
// ---------------------------------------------------------------------------

/// How an `FString` is stored on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StrEncoding {
    /// Length 0, no terminator.
    Empty,
    /// Positive length, 8-bit (Latin-1) characters + NUL.
    Latin1,
    /// Negative length, UTF-16LE code units + NUL.
    Utf16,
}

/// An `FString` with its exact on-disk encoding.
#[derive(Debug, Clone, PartialEq)]
struct FStr {
    text: String,
    enc: StrEncoding,
}

impl FStr {
    /// A new string with the encoding Unreal would pick for it.
    fn new(text: &str) -> FStr {
        let enc = if text.is_empty() {
            StrEncoding::Empty
        } else if text.is_ascii() {
            StrEncoding::Latin1
        } else {
            StrEncoding::Utf16
        };
        FStr { text: text.to_string(), enc }
    }

    fn read(r: &mut ByteReader<'_>, ctx: &str) -> CodecResult<FStr> {
        let start = r.pos();
        let len = r.i32_le(ctx)?;
        if len == 0 {
            return Ok(FStr { text: String::new(), enc: StrEncoding::Empty });
        }
        if len > 0 {
            let bytes = r.take(len as usize, ctx)?;
            let (body, term) = bytes.split_at(bytes.len() - 1);
            if term != [0] {
                return Err(CodecError::malformed(CODEC, start, format!("{ctx}: 8-bit FString is not NUL-terminated")));
            }
            return Ok(FStr { text: body.iter().map(|&b| char::from(b)).collect(), enc: StrEncoding::Latin1 });
        }
        let units = len
            .checked_neg()
            .ok_or_else(|| CodecError::malformed(CODEC, start, format!("{ctx}: invalid FString length {len}")))?
            as usize;
        let bytes = r.take(units.saturating_mul(2), ctx)?;
        let code_units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let Some((&0, body)) = code_units.split_last() else {
            return Err(CodecError::malformed(CODEC, start, format!("{ctx}: UTF-16 FString is not NUL-terminated")));
        };
        let text = String::from_utf16(body).map_err(|_| {
            CodecError::malformed(CODEC, start, format!("{ctx}: UTF-16 FString contains unpaired surrogates"))
        })?;
        Ok(FStr { text, enc: StrEncoding::Utf16 })
    }

    fn write(&self, out: &mut Vec<u8>) -> CodecResult<()> {
        let too_long = || CodecError::unencodable(CODEC, self.text.as_str(), "string is too long for an FString");
        match self.enc {
            StrEncoding::Empty if self.text.is_empty() => out.put_i32_le(0),
            StrEncoding::Empty | StrEncoding::Latin1 => {
                let mut bytes = Vec::with_capacity(self.text.len() + 1);
                for c in self.text.chars() {
                    let b = u8::try_from(u32::from(c)).map_err(|_| {
                        CodecError::unencodable(
                            CODEC,
                            self.text.as_str(),
                            format!("character {c:?} does not fit an 8-bit FString"),
                        )
                    })?;
                    bytes.push(b);
                }
                bytes.push(0);
                out.put_i32_le(i32::try_from(bytes.len()).map_err(|_| too_long())?);
                out.extend_from_slice(&bytes);
            }
            StrEncoding::Utf16 => {
                let units: Vec<u16> = self.text.encode_utf16().chain(std::iter::once(0)).collect();
                let len = i32::try_from(units.len()).map_err(|_| too_long())?;
                out.put_i32_le(-len);
                for u in units {
                    out.put_u16_le(u);
                }
            }
        }
        Ok(())
    }

    /// Replaces the text, keeping the original encoding whenever possible.
    fn set_text(&mut self, text: &str) {
        let enc = match self.enc {
            StrEncoding::Empty => FStr::new(text).enc,
            StrEncoding::Latin1 if text.chars().all(|c| u32::from(c) <= 0xff) => StrEncoding::Latin1,
            StrEncoding::Latin1 | StrEncoding::Utf16 => StrEncoding::Utf16,
        };
        self.text = text.to_string();
        self.enc = enc;
    }

    /// True for the `None` name that terminates a property list.
    fn is_none(&self) -> bool {
        self.text.eq_ignore_ascii_case("None")
    }
}

// ---------------------------------------------------------------------------
// Scalars and binary struct layouts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScalarTy {
    Bool,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
}

impl ScalarTy {
    fn size(self) -> usize {
        match self {
            ScalarTy::Bool | ScalarTy::I8 | ScalarTy::U8 => 1,
            ScalarTy::I16 | ScalarTy::U16 => 2,
            ScalarTy::I32 | ScalarTy::U32 | ScalarTy::F32 => 4,
            ScalarTy::I64 | ScalarTy::U64 | ScalarTy::F64 => 8,
        }
    }

    /// Numeric property types.
    fn from_property(name: &str) -> Option<ScalarTy> {
        Some(match name {
            "IntProperty" => ScalarTy::I32,
            "Int8Property" => ScalarTy::I8,
            "Int16Property" => ScalarTy::I16,
            "Int64Property" => ScalarTy::I64,
            "UInt16Property" => ScalarTy::U16,
            "UInt32Property" => ScalarTy::U32,
            "UInt64Property" => ScalarTy::U64,
            "FloatProperty" => ScalarTy::F32,
            "DoubleProperty" => ScalarTy::F64,
            _ => return None,
        })
    }
}

/// A primitive value. Bools keep their raw byte so that non-canonical
/// values (`2`, `0xff`) survive the round trip.
#[derive(Debug, Clone, Copy)]
enum Scalar {
    Bool(u8),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    F32(f32),
    F64(f64),
}

impl Scalar {
    fn zero(ty: ScalarTy) -> Scalar {
        match ty {
            ScalarTy::Bool => Scalar::Bool(0),
            ScalarTy::I8 => Scalar::I8(0),
            ScalarTy::I16 => Scalar::I16(0),
            ScalarTy::I32 => Scalar::I32(0),
            ScalarTy::I64 => Scalar::I64(0),
            ScalarTy::U8 => Scalar::U8(0),
            ScalarTy::U16 => Scalar::U16(0),
            ScalarTy::U32 => Scalar::U32(0),
            ScalarTy::U64 => Scalar::U64(0),
            ScalarTy::F32 => Scalar::F32(0.0),
            ScalarTy::F64 => Scalar::F64(0.0),
        }
    }

    fn read(ty: ScalarTy, r: &mut ByteReader<'_>) -> CodecResult<Scalar> {
        Ok(match ty {
            ScalarTy::Bool => Scalar::Bool(r.u8("bool")?),
            ScalarTy::I8 => Scalar::I8(r.i8("int8")?),
            ScalarTy::I16 => Scalar::I16(r.i16_le("int16")?),
            ScalarTy::I32 => Scalar::I32(r.i32_le("int32")?),
            ScalarTy::I64 => Scalar::I64(r.i64_le("int64")?),
            ScalarTy::U8 => Scalar::U8(r.u8("uint8")?),
            ScalarTy::U16 => Scalar::U16(r.u16_le("uint16")?),
            ScalarTy::U32 => Scalar::U32(r.u32_le("uint32")?),
            ScalarTy::U64 => Scalar::U64(r.u64_le("uint64")?),
            ScalarTy::F32 => Scalar::F32(r.f32_le("float")?),
            ScalarTy::F64 => Scalar::F64(r.f64_le("double")?),
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        match *self {
            Scalar::Bool(v) | Scalar::U8(v) => out.put_u8(v),
            Scalar::I8(v) => out.extend_from_slice(&v.to_le_bytes()),
            Scalar::I16(v) => out.extend_from_slice(&v.to_le_bytes()),
            Scalar::I32(v) => out.put_i32_le(v),
            Scalar::I64(v) => out.put_i64_le(v),
            Scalar::U16(v) => out.put_u16_le(v),
            Scalar::U32(v) => out.put_u32_le(v),
            Scalar::U64(v) => out.put_u64_le(v),
            Scalar::F32(v) => out.extend_from_slice(&v.to_le_bytes()),
            Scalar::F64(v) => out.extend_from_slice(&v.to_le_bytes()),
        }
    }

    fn to_node(self) -> Node {
        match self {
            Scalar::Bool(v) => Node::Bool(v != 0),
            Scalar::I8(v) => Node::I8(v),
            Scalar::I16(v) => Node::I16(v),
            Scalar::I32(v) => Node::I32(v),
            Scalar::I64(v) => Node::I64(v),
            Scalar::U8(v) => Node::U8(v),
            Scalar::U16(v) => Node::U16(v),
            Scalar::U32(v) => Node::U32(v),
            Scalar::U64(v) => Node::U64(v),
            Scalar::F32(v) => Node::F32(v),
            Scalar::F64(v) => Node::F64(v),
        }
    }

    /// The scalar after an AST edit (same type, range checked).
    fn edited(self, node: &Node, path: &NodePath) -> CodecResult<Scalar> {
        let current = self.to_node();
        if bit_equal(&current, node) {
            return Ok(self);
        }
        let new = coerce_like(CODEC, path, &current, node)?;
        Ok(match (self, new) {
            (Scalar::Bool(raw), Node::Bool(b)) => {
                if (raw != 0) == b {
                    Scalar::Bool(raw)
                } else {
                    Scalar::Bool(u8::from(b))
                }
            }
            (_, Node::I8(v)) => Scalar::I8(v),
            (_, Node::I16(v)) => Scalar::I16(v),
            (_, Node::I32(v)) => Scalar::I32(v),
            (_, Node::I64(v)) => Scalar::I64(v),
            (_, Node::U8(v)) => Scalar::U8(v),
            (_, Node::U16(v)) => Scalar::U16(v),
            (_, Node::U32(v)) => Scalar::U32(v),
            (_, Node::U64(v)) => Scalar::U64(v),
            (_, Node::F32(v)) => Scalar::F32(v),
            (_, Node::F64(v)) => Scalar::F64(v),
            (_, other) => {
                return Err(CodecError::type_mismatch(CODEC, path, current.kind().name(), other.kind().name()));
            }
        })
    }
}

/// One field of a binary ("native serialised") struct.
#[derive(Debug)]
struct BinField {
    /// Sub-map the field is projected into (`Min` / `Max` of a `Box`).
    group: Option<&'static str>,
    name: &'static str,
    ty: ScalarTy,
}

/// Layout of a well-known binary struct.
#[derive(Debug)]
struct BinLayout {
    name: &'static str,
    fields: &'static [BinField],
    /// Single-field structs (`DateTime`) are projected as the bare value.
    direct: bool,
}

impl BinLayout {
    fn size(&self) -> usize {
        self.fields.iter().map(|f| f.ty.size()).sum()
    }

    fn is_double(&self) -> bool {
        self.fields.iter().any(|f| f.ty == ScalarTy::F64)
    }
}

const fn fld(name: &'static str, ty: ScalarTy) -> BinField {
    BinField { group: None, name, ty }
}

const fn grp(group: &'static str, name: &'static str, ty: ScalarTy) -> BinField {
    BinField { group: Some(group), name, ty }
}

const fn layout(name: &'static str, fields: &'static [BinField]) -> BinLayout {
    BinLayout { name, fields, direct: false }
}

use ScalarTy::{F32, F64, I32, I64, U8};

const XY_F: &[BinField] = &[fld("X", F32), fld("Y", F32)];
const XY_D: &[BinField] = &[fld("X", F64), fld("Y", F64)];
const XYZ_F: &[BinField] = &[fld("X", F32), fld("Y", F32), fld("Z", F32)];
const XYZ_D: &[BinField] = &[fld("X", F64), fld("Y", F64), fld("Z", F64)];
const XYZW_F: &[BinField] = &[fld("X", F32), fld("Y", F32), fld("Z", F32), fld("W", F32)];
const XYZW_D: &[BinField] = &[fld("X", F64), fld("Y", F64), fld("Z", F64), fld("W", F64)];
const PYR_F: &[BinField] = &[fld("Pitch", F32), fld("Yaw", F32), fld("Roll", F32)];
const PYR_D: &[BinField] = &[fld("Pitch", F64), fld("Yaw", F64), fld("Roll", F64)];
const XY_I: &[BinField] = &[fld("X", I32), fld("Y", I32)];
const XYZ_I: &[BinField] = &[fld("X", I32), fld("Y", I32), fld("Z", I32)];
const XYZW_I: &[BinField] = &[fld("X", I32), fld("Y", I32), fld("Z", I32), fld("W", I32)];
const RGBA_F: &[BinField] = &[fld("R", F32), fld("G", F32), fld("B", F32), fld("A", F32)];
const BGRA_U8: &[BinField] = &[fld("B", U8), fld("G", U8), fld("R", U8), fld("A", U8)];
const TICKS: &[BinField] = &[fld("Ticks", I64)];
const BOX_F: &[BinField] = &[
    grp("Min", "X", F32),
    grp("Min", "Y", F32),
    grp("Min", "Z", F32),
    grp("Max", "X", F32),
    grp("Max", "Y", F32),
    grp("Max", "Z", F32),
    fld("IsValid", U8),
];
const BOX_D: &[BinField] = &[
    grp("Min", "X", F64),
    grp("Min", "Y", F64),
    grp("Min", "Z", F64),
    grp("Max", "X", F64),
    grp("Max", "Y", F64),
    grp("Max", "Z", F64),
    fld("IsValid", U8),
];
const BOX2D_F: &[BinField] =
    &[grp("Min", "X", F32), grp("Min", "Y", F32), grp("Max", "X", F32), grp("Max", "Y", F32), fld("bIsValid", U8)];
const BOX2D_D: &[BinField] =
    &[grp("Min", "X", F64), grp("Min", "Y", F64), grp("Max", "X", F64), grp("Max", "Y", F64), fld("bIsValid", U8)];

/// Well-known binary structs. Float/double variants share a name; the value
/// size decides which one applies.
static LAYOUTS: &[BinLayout] = &[
    layout("Vector", XYZ_F),
    layout("Vector", XYZ_D),
    layout("Vector3f", XYZ_F),
    layout("Vector3d", XYZ_D),
    layout("Vector2D", XY_F),
    layout("Vector2D", XY_D),
    layout("Vector2f", XY_F),
    layout("Vector4", XYZW_F),
    layout("Vector4", XYZW_D),
    layout("Vector4f", XYZW_F),
    layout("Vector4d", XYZW_D),
    layout("Quat", XYZW_F),
    layout("Quat", XYZW_D),
    layout("Quat4f", XYZW_F),
    layout("Quat4d", XYZW_D),
    layout("Plane", XYZW_F),
    layout("Plane", XYZW_D),
    layout("Rotator", PYR_F),
    layout("Rotator", PYR_D),
    layout("Rotator3f", PYR_F),
    layout("Rotator3d", PYR_D),
    layout("LinearColor", RGBA_F),
    layout("Color", BGRA_U8),
    layout("IntPoint", XY_I),
    layout("IntVector2", XY_I),
    layout("IntVector", XYZ_I),
    layout("IntVector4", XYZW_I),
    layout("Box", BOX_F),
    layout("Box", BOX_D),
    layout("Box2D", BOX2D_F),
    layout("Box2D", BOX2D_D),
    BinLayout { name: "DateTime", fields: TICKS, direct: true },
    BinLayout { name: "Timespan", fields: TICKS, direct: true },
];

fn guid_to_string(g: &[u8; 16]) -> String {
    g.chunks_exact(4).map(|c| format!("{:08X}", u32::from_le_bytes([c[0], c[1], c[2], c[3]]))).collect()
}

fn parse_guid(s: &str, path: &NodePath) -> CodecResult<[u8; 16]> {
    let hex: String = s.chars().filter(|c| !matches!(c, '-' | '{' | '}' | ' ')).collect();
    let bad = || CodecError::unencodable(CODEC, path, format!("`{s}` is not a GUID (expected 32 hexadecimal digits)"));
    if hex.len() != 32 || !hex.is_ascii() {
        return Err(bad());
    }
    let mut out = [0u8; 16];
    for (i, chunk) in out.chunks_exact_mut(4).enumerate() {
        let part = hex.get(i * 8..i * 8 + 8).ok_or_else(bad)?;
        let v = u32::from_str_radix(part, 16).map_err(|_| bad())?;
        chunk.copy_from_slice(&v.to_le_bytes());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Document model
// ---------------------------------------------------------------------------

/// How a container element (or a struct value) is encoded.
#[derive(Debug, Clone, Copy)]
enum ElemKind {
    Scalar(ScalarTy),
    Str,
    /// `n` consecutive FStrings (soft object paths).
    StrSeq(usize),
    Bin(&'static BinLayout),
    Guid,
    /// A property list terminated by `None`.
    Props,
}

impl ElemKind {
    fn min_size(self) -> usize {
        match self {
            ElemKind::Scalar(t) => t.size(),
            ElemKind::Str | ElemKind::Props => 4,
            ElemKind::StrSeq(n) => 4 * n,
            ElemKind::Bin(l) => l.size(),
            ElemKind::Guid => 16,
        }
    }
}

#[derive(Debug, Clone)]
enum Value {
    Scalar(Scalar),
    Str(FStr),
    StrSeq(Vec<FStr>),
    Bin(&'static BinLayout, Vec<Scalar>),
    Guid([u8; 16]),
    Props(PropList),
    Array(Box<ArrayValue>),
    Set(Box<SetValue>),
    Map(Box<MapValue>),
    /// Unknown or unparseable value, kept verbatim.
    Raw(Vec<u8>),
}

/// The inner tag that precedes the elements of a struct array.
#[derive(Debug, Clone)]
struct InnerTag {
    name: FStr,
    type_name: FStr,
    array_index: i32,
    struct_name: FStr,
    struct_guid: [u8; 16],
    guid: PropGuid,
    ext: Option<u8>,
}

#[derive(Debug, Clone)]
struct ArrayValue {
    kind: ElemKind,
    inner_tag: Option<InnerTag>,
    elems: Vec<Value>,
}

#[derive(Debug, Clone)]
struct SetValue {
    kind: ElemKind,
    removed: Vec<Value>,
    elems: Vec<Value>,
}

#[derive(Debug, Clone)]
struct MapValue {
    removed: Vec<Value>,
    entries: Vec<(Value, Value)>,
}

/// Classic `HasPropertyGuid` byte + GUID.
#[derive(Debug, Clone)]
struct PropGuid {
    flag: u8,
    guid: Option<[u8; 16]>,
}

impl PropGuid {
    fn read(r: &mut ByteReader<'_>) -> CodecResult<PropGuid> {
        let flag = r.u8("HasPropertyGuid")?;
        let guid = if flag != 0 { Some(r.array::<16>("property guid")?) } else { None };
        Ok(PropGuid { flag, guid })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.put_u8(self.flag);
        if let Some(g) = &self.guid {
            out.extend_from_slice(g);
        }
    }
}

/// Type-specific header data of a classic tag.
#[derive(Debug, Clone)]
enum ClassicExtra {
    None,
    /// `BoolProperty`: the value byte lives in the property value.
    Bool,
    /// `ByteProperty` / `EnumProperty`: enum name.
    Enum(FStr),
    Struct {
        name: FStr,
        guid: [u8; 16],
    },
    /// `ArrayProperty` / `SetProperty`: inner type.
    Inner(FStr),
    Map {
        key: FStr,
        value: FStr,
    },
}

/// UE 5.4 `FPropertyTypeName` node.
#[derive(Debug, Clone)]
struct TypeNode {
    name: FStr,
    params: Vec<TypeNode>,
}

impl TypeNode {
    fn read(r: &mut ByteReader<'_>, depth: usize) -> CodecResult<TypeNode> {
        if depth > MAX_TYPE_DEPTH {
            return Err(r.malformed("property type name is nested too deeply"));
        }
        let name = FStr::read(r, "type name")?;
        let count = r.i32_le("type parameter count")?;
        let count =
            usize::try_from(count).map_err(|_| r.malformed(format!("negative type parameter count {count}")))?;
        r.check_count(count, 8, "type parameters")?;
        let mut params = Vec::with_capacity(count);
        for _ in 0..count {
            params.push(TypeNode::read(r, depth + 1)?);
        }
        Ok(TypeNode { name, params })
    }

    fn write(&self, out: &mut Vec<u8>) -> CodecResult<()> {
        self.name.write(out)?;
        out.put_i32_le(len_i32(self.params.len(), "type parameters")?);
        for p in &self.params {
            p.write(out)?;
        }
        Ok(())
    }

    fn display(&self) -> String {
        if self.params.is_empty() {
            self.name.text.clone()
        } else {
            let inner: Vec<String> = self.params.iter().map(TypeNode::display).collect();
            format!("{}({})", self.name.text, inner.join(","))
        }
    }
}

#[derive(Debug, Clone)]
enum Tag {
    Classic { type_name: FStr, extra: ClassicExtra, guid: PropGuid, ext: Option<u8> },
    Complete { type_name: TypeNode, flags: u8, guid: Option<[u8; 16]>, ext: Option<u8> },
}

#[derive(Debug, Clone)]
struct Property {
    name: FStr,
    array_index: i32,
    tag: Tag,
    value: Value,
}

#[derive(Debug, Clone)]
struct PropList {
    props: Vec<Property>,
    /// The `None` name (its encoding is preserved too).
    terminator: FStr,
}

#[derive(Debug, Clone)]
enum CustomVersions {
    /// `SaveGameFileVersion` < 2: no custom versions.
    Absent,
    /// Format 3 (`Optimized`): GUID + version.
    Optimized { format: i32, entries: Vec<([u8; 16], i32)> },
    /// Format 2 (`Guids`): GUID + version + friendly name.
    Guids { format: i32, entries: Vec<([u8; 16], i32, FStr)> },
}

#[derive(Debug, Clone)]
struct Header {
    save_game_version: i32,
    package_ue4: i32,
    package_ue5: Option<i32>,
    major: u16,
    minor: u16,
    patch: u16,
    changelist: u32,
    branch: FStr,
    custom: CustomVersions,
    class_name: FStr,
}

impl Header {
    fn read(r: &mut ByteReader<'_>) -> CodecResult<Header> {
        let magic = r.array::<4>("magic")?;
        if &magic != MAGIC {
            return Err(CodecError::malformed(CODEC, 0, "missing GVAS magic"));
        }
        let save_game_version = r.i32_le("SaveGameFileVersion")?;
        if !(1..=1000).contains(&save_game_version) {
            return Err(r.malformed(format!("implausible SaveGameFileVersion {save_game_version}")));
        }
        let package_ue4 = r.i32_le("PackageFileUE4Version")?;
        let package_ue5 = if save_game_version >= 3 { Some(r.i32_le("PackageFileUE5Version")?) } else { None };
        let major = r.u16_le("engine major")?;
        let minor = r.u16_le("engine minor")?;
        let patch = r.u16_le("engine patch")?;
        let changelist = r.u32_le("engine changelist")?;
        let branch = FStr::read(r, "engine branch")?;
        let custom = if save_game_version >= 2 {
            let format = r.i32_le("CustomVersionFormat")?;
            let count = r.i32_le("custom version count")?;
            let count =
                usize::try_from(count).map_err(|_| r.malformed(format!("negative custom version count {count}")))?;
            match format {
                3 => {
                    r.check_count(count, 20, "custom versions")?;
                    let mut entries = Vec::with_capacity(count);
                    for _ in 0..count {
                        entries.push((r.array::<16>("custom version guid")?, r.i32_le("custom version")?));
                    }
                    CustomVersions::Optimized { format, entries }
                }
                2 => {
                    r.check_count(count, 24, "custom versions")?;
                    let mut entries = Vec::with_capacity(count);
                    for _ in 0..count {
                        let g = r.array::<16>("custom version guid")?;
                        let v = r.i32_le("custom version")?;
                        entries.push((g, v, FStr::read(r, "custom version name")?));
                    }
                    CustomVersions::Guids { format, entries }
                }
                other => return Err(CodecError::unsupported(CODEC, format!("custom version format {other}"))),
            }
        } else {
            CustomVersions::Absent
        };
        let class_name = FStr::read(r, "SaveGameClassName")?;
        Ok(Header {
            save_game_version,
            package_ue4,
            package_ue5,
            major,
            minor,
            patch,
            changelist,
            branch,
            custom,
            class_name,
        })
    }

    fn write(&self, out: &mut Vec<u8>) -> CodecResult<()> {
        out.extend_from_slice(MAGIC);
        out.put_i32_le(self.save_game_version);
        out.put_i32_le(self.package_ue4);
        if let Some(v) = self.package_ue5 {
            out.put_i32_le(v);
        }
        out.put_u16_le(self.major);
        out.put_u16_le(self.minor);
        out.put_u16_le(self.patch);
        out.put_u32_le(self.changelist);
        self.branch.write(out)?;
        match &self.custom {
            CustomVersions::Absent => {}
            CustomVersions::Optimized { format, entries } => {
                out.put_i32_le(*format);
                out.put_i32_le(len_i32(entries.len(), "custom versions")?);
                for (g, v) in entries {
                    out.extend_from_slice(g);
                    out.put_i32_le(*v);
                }
            }
            CustomVersions::Guids { format, entries } => {
                out.put_i32_le(*format);
                out.put_i32_le(len_i32(entries.len(), "custom versions")?);
                for (g, v, name) in entries {
                    out.extend_from_slice(g);
                    out.put_i32_le(*v);
                    name.write(out)?;
                }
            }
        }
        self.class_name.write(out)
    }

    fn custom_count(&self) -> usize {
        match &self.custom {
            CustomVersions::Absent => 0,
            CustomVersions::Optimized { entries, .. } => entries.len(),
            CustomVersions::Guids { entries, .. } => entries.len(),
        }
    }
}

fn len_i32(len: usize, what: &str) -> CodecResult<i32> {
    i32::try_from(len).map_err(|_| CodecError::unencodable(CODEC, what, format!("{len} exceeds the 32-bit size limit")))
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Serialisation flavour derived from the header.
#[derive(Debug, Clone, Copy)]
struct Fmt {
    /// UE 5.4+ complete type name tags.
    complete_tags: bool,
    /// Classic tags carry a property-extension byte (UE 5.3).
    tag_extension: bool,
    /// Large world coordinates: ambiguous vectors default to doubles.
    prefer_double: bool,
}

impl Fmt {
    fn from_header(h: &Header) -> Fmt {
        let ue5 = h.package_ue5.unwrap_or(0);
        Fmt {
            complete_tags: ue5 >= UE5_PROPERTY_TAG_COMPLETE_TYPE_NAME,
            tag_extension: (UE5_PROPERTY_TAG_EXTENSION..UE5_PROPERTY_TAG_COMPLETE_TYPE_NAME).contains(&ue5),
            prefer_double: ue5 >= UE5_LARGE_WORLD_COORDINATES,
        }
    }
}

/// Semantic type of a property, independent of the tag layout.
#[derive(Debug, Clone)]
enum TypeDesc {
    Bool,
    Scalar(ScalarTy),
    /// `ByteProperty`; `Some(true)` when an enum is known to be attached.
    Byte(Option<bool>),
    Str,
    SoftObject,
    Struct(Option<String>),
    Array(Box<TypeDesc>),
    Set(Box<TypeDesc>),
    Map(Box<TypeDesc>, Box<TypeDesc>),
    Unknown,
}

/// Type of a container element known only by its property type name.
fn simple_desc(name: &str) -> TypeDesc {
    match name {
        "BoolProperty" => TypeDesc::Bool,
        "ByteProperty" => TypeDesc::Byte(None),
        "StrProperty" | "NameProperty" | "ObjectProperty" | "ClassProperty" | "WeakObjectProperty" | "EnumProperty" => {
            TypeDesc::Str
        }
        "SoftObjectProperty" | "SoftClassProperty" => TypeDesc::SoftObject,
        "StructProperty" => TypeDesc::Struct(None),
        other => ScalarTy::from_property(other).map(TypeDesc::Scalar).unwrap_or(TypeDesc::Unknown),
    }
}

fn classic_desc(type_name: &str, extra: &ClassicExtra) -> TypeDesc {
    match (type_name, extra) {
        ("ByteProperty", ClassicExtra::Enum(e)) => TypeDesc::Byte(Some(!e.is_none())),
        ("StructProperty", ClassicExtra::Struct { name, .. }) => TypeDesc::Struct(Some(name.text.clone())),
        ("ArrayProperty", ClassicExtra::Inner(i)) => TypeDesc::Array(Box::new(simple_desc(&i.text))),
        ("SetProperty", ClassicExtra::Inner(i)) => TypeDesc::Set(Box::new(simple_desc(&i.text))),
        ("MapProperty", ClassicExtra::Map { key, value }) => {
            TypeDesc::Map(Box::new(simple_desc(&key.text)), Box::new(simple_desc(&value.text)))
        }
        (name, _) => simple_desc(name),
    }
}

fn complete_desc(n: &TypeNode, nested: bool) -> TypeDesc {
    let p = &n.params;
    match n.name.text.as_str() {
        "StructProperty" => TypeDesc::Struct(p.first().map(|x| x.name.text.clone())),
        "ByteProperty" => TypeDesc::Byte(Some(!p.is_empty())),
        "ArrayProperty" | "SetProperty" | "MapProperty" if nested => TypeDesc::Unknown,
        "ArrayProperty" => match p.first() {
            Some(i) => TypeDesc::Array(Box::new(complete_desc(i, true))),
            None => TypeDesc::Unknown,
        },
        "SetProperty" => match p.first() {
            Some(i) => TypeDesc::Set(Box::new(complete_desc(i, true))),
            None => TypeDesc::Unknown,
        },
        "MapProperty" => match (p.first(), p.get(1)) {
            (Some(k), Some(v)) => TypeDesc::Map(Box::new(complete_desc(k, true)), Box::new(complete_desc(v, true))),
            _ => TypeDesc::Unknown,
        },
        other => simple_desc(other),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Key,
    Value,
}

fn struct_candidates(name: Option<&str>, elem_size: Option<usize>, role: Role, fmt: Fmt) -> Vec<ElemKind> {
    let mut v = Vec::new();
    match name {
        Some(n) => {
            let mut layouts: Vec<&'static BinLayout> =
                LAYOUTS.iter().filter(|l| l.name == n && elem_size.is_none_or(|s| s == l.size())).collect();
            // Stable sort: the preferred precision first.
            layouts.sort_by_key(|l| l.is_double() != fmt.prefer_double);
            v.extend(layouts.into_iter().map(ElemKind::Bin));
            if n == "Guid" && elem_size.is_none_or(|s| s == 16) {
                v.push(ElemKind::Guid);
            }
            if matches!(n, "SoftObjectPath" | "SoftClassPath") {
                v.push(ElemKind::StrSeq(2));
                v.push(ElemKind::StrSeq(3));
            }
            v.push(ElemKind::Props);
        }
        None => match role {
            Role::Key => v.extend([ElemKind::Guid, ElemKind::Props]),
            Role::Value => v.extend([ElemKind::Props, ElemKind::Guid]),
        },
    }
    v
}

fn elem_candidates(desc: &TypeDesc, elem_size: Option<usize>, role: Role, fmt: Fmt) -> Vec<ElemKind> {
    match desc {
        TypeDesc::Bool => vec![ElemKind::Scalar(ScalarTy::Bool)],
        TypeDesc::Scalar(t) => vec![ElemKind::Scalar(*t)],
        TypeDesc::Byte(Some(true)) => vec![ElemKind::Str, ElemKind::Scalar(ScalarTy::U8)],
        TypeDesc::Byte(_) => vec![ElemKind::Scalar(ScalarTy::U8), ElemKind::Str],
        TypeDesc::Str => vec![ElemKind::Str],
        TypeDesc::SoftObject => vec![ElemKind::StrSeq(2), ElemKind::StrSeq(3)],
        TypeDesc::Struct(n) => struct_candidates(n.as_deref(), elem_size, role, fmt),
        TypeDesc::Array(_) | TypeDesc::Set(_) | TypeDesc::Map(..) | TypeDesc::Unknown => Vec::new(),
    }
}

fn per_elem(total: usize, count: usize) -> Option<usize> {
    (count > 0 && total % count == 0).then(|| total / count)
}

fn read_count(r: &mut ByteReader<'_>, ctx: &str) -> CodecResult<usize> {
    let n = r.i32_le(ctx)?;
    usize::try_from(n).map_err(|_| r.malformed(format!("{ctx}: negative count {n}")))
}

/// Tries every candidate on a copy of the reader; the first one that
/// succeeds *and* consumes the reader to its end wins.
fn first_fit<'a, K: Copy, T>(
    r: &mut ByteReader<'a>,
    cands: &[K],
    mut f: impl FnMut(K, &mut ByteReader<'a>) -> CodecResult<T>,
) -> CodecResult<(K, T)> {
    for &k in cands {
        let mut r2 = r.clone();
        if let Ok(v) = f(k, &mut r2) {
            if r2.is_empty() {
                *r = r2;
                return Ok((k, v));
            }
        }
    }
    Err(r.malformed("no element interpretation consumes exactly the declared value size"))
}

fn read_elem(kind: ElemKind, r: &mut ByteReader<'_>, fmt: Fmt, depth: usize) -> CodecResult<Value> {
    Ok(match kind {
        ElemKind::Scalar(t) => Value::Scalar(Scalar::read(t, r)?),
        ElemKind::Str => Value::Str(FStr::read(r, "string value")?),
        ElemKind::StrSeq(n) => {
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(FStr::read(r, "soft object path")?);
            }
            Value::StrSeq(v)
        }
        ElemKind::Bin(l) => {
            let mut vals = Vec::with_capacity(l.fields.len());
            for f in l.fields {
                vals.push(Scalar::read(f.ty, r)?);
            }
            Value::Bin(l, vals)
        }
        ElemKind::Guid => Value::Guid(r.array::<16>("guid")?),
        ElemKind::Props => Value::Props(read_props(r, fmt, depth + 1)?),
    })
}

fn read_n(kind: ElemKind, n: usize, r: &mut ByteReader<'_>, fmt: Fmt, depth: usize) -> CodecResult<Vec<Value>> {
    r.check_count(n, kind.min_size(), "container elements")?;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(read_elem(kind, r, fmt, depth)?);
    }
    Ok(v)
}

impl InnerTag {
    fn read(r: &mut ByteReader<'_>, fmt: Fmt) -> CodecResult<(InnerTag, usize)> {
        let name = FStr::read(r, "inner tag name")?;
        let type_name = FStr::read(r, "inner tag type")?;
        if type_name.text != "StructProperty" {
            return Err(r.malformed(format!("struct array inner tag has type `{}`", type_name.text)));
        }
        let size = r.i32_le("inner tag size")?;
        let size = usize::try_from(size).map_err(|_| r.malformed("negative inner tag size"))?;
        let array_index = r.i32_le("inner tag array index")?;
        let struct_name = FStr::read(r, "inner tag struct name")?;
        let struct_guid = r.array::<16>("inner tag struct guid")?;
        let guid = PropGuid::read(r)?;
        let ext = if fmt.tag_extension {
            let e = r.u8("inner tag extensions")?;
            if e != 0 {
                return Err(r.malformed(format!("inner tag property extensions {e:#x}")));
            }
            Some(e)
        } else {
            None
        };
        if size != r.remaining() {
            return Err(
                r.malformed(format!("inner tag size {size} does not match the {} remaining bytes", r.remaining()))
            );
        }
        Ok((InnerTag { name, type_name, array_index, struct_name, struct_guid, guid, ext }, size))
    }

    fn write(&self, size: usize, out: &mut Vec<u8>) -> CodecResult<()> {
        self.name.write(out)?;
        self.type_name.write(out)?;
        out.put_i32_le(len_i32(size, "struct array")?);
        out.put_i32_le(self.array_index);
        self.struct_name.write(out)?;
        out.extend_from_slice(&self.struct_guid);
        self.guid.write(out);
        if let Some(e) = self.ext {
            out.put_u8(e);
        }
        Ok(())
    }
}

fn read_array(inner: &TypeDesc, r: &mut ByteReader<'_>, fmt: Fmt, depth: usize) -> CodecResult<ArrayValue> {
    let count = read_count(r, "array count")?;
    if let TypeDesc::Struct(name) = inner {
        if fmt.complete_tags {
            // UE 5.4 knows the struct from the type name; try without an inner tag first.
            let kinds = struct_candidates(name.as_deref(), per_elem(r.remaining(), count), Role::Value, fmt);
            let mut r2 = r.clone();
            if let Ok((kind, elems)) = first_fit(&mut r2, &kinds, |k, rr| read_n(k, count, rr, fmt, depth)) {
                *r = r2;
                return Ok(ArrayValue { kind, inner_tag: None, elems });
            }
        }
        let (tag, size) = InnerTag::read(r, fmt)?;
        let kinds = struct_candidates(Some(&tag.struct_name.text), per_elem(size, count), Role::Value, fmt);
        let (kind, elems) = first_fit(r, &kinds, |k, rr| read_n(k, count, rr, fmt, depth))?;
        return Ok(ArrayValue { kind, inner_tag: Some(tag), elems });
    }
    let kinds = elem_candidates(inner, per_elem(r.remaining(), count), Role::Value, fmt);
    let (kind, elems) = first_fit(r, &kinds, |k, rr| read_n(k, count, rr, fmt, depth))?;
    Ok(ArrayValue { kind, inner_tag: None, elems })
}

fn read_set(inner: &TypeDesc, r: &mut ByteReader<'_>, fmt: Fmt, depth: usize) -> CodecResult<SetValue> {
    let kinds = elem_candidates(inner, None, Role::Key, fmt);
    let (kind, (removed, elems)) = first_fit(r, &kinds, |k, rr| {
        let rc = read_count(rr, "set removed count")?;
        let removed = read_n(k, rc, rr, fmt, depth)?;
        let c = read_count(rr, "set count")?;
        let elems = read_n(k, c, rr, fmt, depth)?;
        Ok((removed, elems))
    })?;
    Ok(SetValue { kind, removed, elems })
}

fn read_map(key: &TypeDesc, value: &TypeDesc, r: &mut ByteReader<'_>, fmt: Fmt, depth: usize) -> CodecResult<MapValue> {
    let kk = elem_candidates(key, None, Role::Key, fmt);
    let vk = elem_candidates(value, None, Role::Value, fmt);
    let pairs: Vec<(ElemKind, ElemKind)> = kk.iter().flat_map(|&k| vk.iter().map(move |&v| (k, v))).collect();
    let (_, (removed, entries)) = first_fit(r, &pairs, |(k, v), rr| {
        let rc = read_count(rr, "map removed count")?;
        let removed = read_n(k, rc, rr, fmt, depth)?;
        let c = read_count(rr, "map count")?;
        rr.check_count(c, k.min_size() + v.min_size(), "map entries")?;
        let mut entries = Vec::with_capacity(c);
        for _ in 0..c {
            let key = read_elem(k, rr, fmt, depth)?;
            let val = read_elem(v, rr, fmt, depth)?;
            entries.push((key, val));
        }
        Ok((removed, entries))
    })?;
    Ok(MapValue { removed, entries })
}

/// Parses a property value of exactly `data.len()` bytes; anything that is
/// not understood with certainty is kept as raw bytes.
fn parse_value(desc: &TypeDesc, data: &[u8], fmt: Fmt, depth: usize) -> Value {
    let verified = |v: Value| -> Option<Value> {
        let mut out = Vec::with_capacity(data.len());
        (encode_value(&v, &mut out).is_ok() && out == data).then_some(v)
    };
    let container = |f: &dyn Fn(&mut ByteReader<'_>) -> CodecResult<Value>| -> Option<Value> {
        let mut r = ByteReader::new(data, CODEC);
        let v = f(&mut r).ok()?;
        if r.is_empty() {
            verified(v)
        } else {
            None
        }
    };
    let parsed = match desc {
        TypeDesc::Array(inner) => container(&|r| Ok(Value::Array(Box::new(read_array(inner, r, fmt, depth)?)))),
        TypeDesc::Set(inner) => container(&|r| Ok(Value::Set(Box::new(read_set(inner, r, fmt, depth)?)))),
        TypeDesc::Map(k, v) => container(&|r| Ok(Value::Map(Box::new(read_map(k, v, r, fmt, depth)?)))),
        _ => {
            let mut kinds = elem_candidates(desc, Some(data.len()), Role::Value, fmt);
            if matches!(desc, TypeDesc::SoftObject) {
                kinds.push(ElemKind::StrSeq(1));
            }
            kinds.into_iter().find_map(|k| {
                let mut r = ByteReader::new(data, CODEC);
                let v = read_elem(k, &mut r, fmt, depth).ok()?;
                if r.is_empty() {
                    verified(v)
                } else {
                    None
                }
            })
        }
    };
    parsed.unwrap_or_else(|| Value::Raw(data.to_vec()))
}

fn read_ext(r: &mut ByteReader<'_>) -> CodecResult<u8> {
    let e = r.u8("property extensions")?;
    if e != 0 {
        return Err(CodecError::unsupported(
            CODEC,
            format!("property tag extensions {e:#x} (overridable serialization) at offset {:#x}", r.pos() - 1),
        ));
    }
    Ok(e)
}

fn read_prop(r: &mut ByteReader<'_>, name: FStr, fmt: Fmt, depth: usize) -> CodecResult<Property> {
    let (tag, desc, size, array_index, bool_val) = if fmt.complete_tags {
        let type_name = TypeNode::read(r, 0)?;
        let size = r.i32_le("property size")?;
        let flags = r.u8("property flags")?;
        if flags & !KNOWN_FLAGS != 0 {
            return Err(CodecError::unsupported(CODEC, format!("unknown property tag flags {flags:#04x}")));
        }
        let array_index = if flags & FLAG_HAS_ARRAY_INDEX != 0 { r.i32_le("array index")? } else { 0 };
        let guid = if flags & FLAG_HAS_PROPERTY_GUID != 0 { Some(r.array::<16>("property guid")?) } else { None };
        let ext = if flags & FLAG_HAS_PROPERTY_EXTENSIONS != 0 { Some(read_ext(r)?) } else { None };
        let desc = complete_desc(&type_name, false);
        let bool_val = u8::from(flags & FLAG_BOOL_TRUE != 0);
        (Tag::Complete { type_name, flags, guid, ext }, desc, size, array_index, bool_val)
    } else {
        let type_name = FStr::read(r, "property type")?;
        let size = r.i32_le("property size")?;
        let array_index = r.i32_le("array index")?;
        let mut bool_val = 0;
        let extra = match type_name.text.as_str() {
            "BoolProperty" => {
                bool_val = r.u8("bool value")?;
                ClassicExtra::Bool
            }
            "ByteProperty" | "EnumProperty" => ClassicExtra::Enum(FStr::read(r, "enum name")?),
            "StructProperty" => {
                ClassicExtra::Struct { name: FStr::read(r, "struct name")?, guid: r.array::<16>("struct guid")? }
            }
            "ArrayProperty" | "SetProperty" => ClassicExtra::Inner(FStr::read(r, "inner type")?),
            "MapProperty" => {
                ClassicExtra::Map { key: FStr::read(r, "map key type")?, value: FStr::read(r, "map value type")? }
            }
            _ => ClassicExtra::None,
        };
        let guid = PropGuid::read(r)?;
        let ext = if fmt.tag_extension { Some(read_ext(r)?) } else { None };
        let desc = classic_desc(&type_name.text, &extra);
        (Tag::Classic { type_name, extra, guid, ext }, desc, size, array_index, bool_val)
    };
    let size =
        usize::try_from(size).map_err(|_| r.malformed(format!("property `{}` has negative size {size}", name.text)))?;
    let data = r.take(size, "property value")?;
    let value = if matches!(desc, TypeDesc::Bool) {
        if size != 0 {
            return Err(CodecError::unsupported(
                CODEC,
                format!("BoolProperty `{}` with a {size}-byte value", name.text),
            ));
        }
        Value::Scalar(Scalar::Bool(bool_val))
    } else {
        parse_value(&desc, data, fmt, depth)
    };
    Ok(Property { name, array_index, tag, value })
}

fn read_props(r: &mut ByteReader<'_>, fmt: Fmt, depth: usize) -> CodecResult<PropList> {
    if depth > MAX_DEPTH {
        return Err(r.malformed("property lists are nested too deeply"));
    }
    let mut props = Vec::new();
    loop {
        let name = FStr::read(r, "property name")?;
        if name.is_none() {
            return Ok(PropList { props, terminator: name });
        }
        props.push(read_prop(r, name, fmt, depth)?);
    }
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

fn write_elems(elems: &[Value], out: &mut Vec<u8>) -> CodecResult<()> {
    out.put_i32_le(len_i32(elems.len(), "container")?);
    for e in elems {
        encode_value(e, out)?;
    }
    Ok(())
}

fn encode_value(v: &Value, out: &mut Vec<u8>) -> CodecResult<()> {
    match v {
        Value::Scalar(s) => s.write(out),
        Value::Str(s) => s.write(out)?,
        Value::StrSeq(list) => {
            for s in list {
                s.write(out)?;
            }
        }
        Value::Bin(_, vals) => {
            for s in vals {
                s.write(out);
            }
        }
        Value::Guid(g) => out.extend_from_slice(g),
        Value::Props(pl) => write_props(pl, out)?,
        Value::Array(a) => match &a.inner_tag {
            Some(tag) => {
                out.put_i32_le(len_i32(a.elems.len(), "array")?);
                let mut body = Vec::new();
                for e in &a.elems {
                    encode_value(e, &mut body)?;
                }
                tag.write(body.len(), out)?;
                out.extend_from_slice(&body);
            }
            None => write_elems(&a.elems, out)?,
        },
        Value::Set(s) => {
            write_elems(&s.removed, out)?;
            write_elems(&s.elems, out)?;
        }
        Value::Map(m) => {
            write_elems(&m.removed, out)?;
            out.put_i32_le(len_i32(m.entries.len(), "map")?);
            for (k, v) in &m.entries {
                encode_value(k, out)?;
                encode_value(v, out)?;
            }
        }
        Value::Raw(b) => out.extend_from_slice(b),
    }
    Ok(())
}

fn write_prop(p: &Property, out: &mut Vec<u8>) -> CodecResult<()> {
    p.name.write(out)?;
    let bool_val = match p.value {
        Value::Scalar(Scalar::Bool(b)) => Some(b),
        _ => None,
    };
    let mut body = Vec::new();
    if bool_val.is_none() {
        encode_value(&p.value, &mut body)?;
    }
    let size = len_i32(body.len(), p.name.text.as_str())?;
    match &p.tag {
        Tag::Classic { type_name, extra, guid, ext } => {
            type_name.write(out)?;
            out.put_i32_le(size);
            out.put_i32_le(p.array_index);
            match extra {
                ClassicExtra::None => {}
                ClassicExtra::Bool => {
                    let b = bool_val.ok_or_else(|| {
                        CodecError::unencodable(CODEC, p.name.text.as_str(), "BoolProperty without a bool value")
                    })?;
                    out.put_u8(b);
                }
                ClassicExtra::Enum(e) => e.write(out)?,
                ClassicExtra::Struct { name, guid } => {
                    name.write(out)?;
                    out.extend_from_slice(guid);
                }
                ClassicExtra::Inner(i) => i.write(out)?,
                ClassicExtra::Map { key, value } => {
                    key.write(out)?;
                    value.write(out)?;
                }
            }
            guid.write(out);
            if let Some(e) = ext {
                out.put_u8(*e);
            }
        }
        Tag::Complete { type_name, flags, guid, ext } => {
            type_name.write(out)?;
            out.put_i32_le(size);
            let mut f = *flags;
            if let Some(b) = bool_val {
                if b != 0 {
                    f |= FLAG_BOOL_TRUE;
                } else {
                    f &= !FLAG_BOOL_TRUE;
                }
            }
            out.put_u8(f);
            if f & FLAG_HAS_ARRAY_INDEX != 0 {
                out.put_i32_le(p.array_index);
            }
            if let Some(g) = guid {
                out.extend_from_slice(g);
            }
            if let Some(e) = ext {
                out.put_u8(*e);
            }
        }
    }
    out.extend_from_slice(&body);
    Ok(())
}

fn write_props(pl: &PropList, out: &mut Vec<u8>) -> CodecResult<()> {
    for p in &pl.props {
        write_prop(p, out)?;
    }
    pl.terminator.write(out)
}

// ---------------------------------------------------------------------------
// Projection
// ---------------------------------------------------------------------------

/// AST keys of a property list: `Name`, `Name#<ArrayIndex>`, `Name#2` for
/// repeated names.
fn prop_keys(props: &[Property]) -> Vec<String> {
    let mut used: HashSet<String> = HashSet::with_capacity(props.len());
    let mut keys = Vec::with_capacity(props.len());
    for p in props {
        let base = if p.array_index > 0 { format!("{}#{}", p.name.text, p.array_index) } else { p.name.text.clone() };
        let key = if used.contains(&base) {
            let mut n = 2usize;
            loop {
                let k = format!("{}#{n}", p.name.text);
                if !used.contains(&k) {
                    break k;
                }
                n += 1;
            }
        } else {
            base
        };
        used.insert(key.clone());
        keys.push(key);
    }
    keys
}

fn strseq_names(n: usize) -> &'static [&'static str] {
    match n {
        1 => &["AssetPath"],
        2 => &["AssetPath", "SubPath"],
        _ => &["PackageName", "AssetName", "SubPath"],
    }
}

fn project_props(pl: &PropList) -> Node {
    let keys = prop_keys(&pl.props);
    Node::Map(keys.into_iter().zip(&pl.props).map(|(k, p)| (k, project_value(&p.value))).collect())
}

fn project_value(v: &Value) -> Node {
    match v {
        Value::Scalar(s) => s.to_node(),
        Value::Str(s) => Node::String(s.text.clone()),
        Value::StrSeq(list) => Node::Map(
            strseq_names(list.len())
                .iter()
                .zip(list)
                .map(|(n, s)| ((*n).to_string(), Node::String(s.text.clone())))
                .collect(),
        ),
        Value::Bin(l, vals) => {
            if l.direct {
                if let Some(s) = vals.first() {
                    return s.to_node();
                }
            }
            let mut m = NodeMap::new();
            for (f, s) in l.fields.iter().zip(vals) {
                match f.group {
                    None => {
                        m.insert(f.name.to_string(), s.to_node());
                    }
                    Some(g) => {
                        let entry = m.entry(g.to_string()).or_insert_with(Node::map);
                        if let Node::Map(sub) = entry {
                            sub.insert(f.name.to_string(), s.to_node());
                        }
                    }
                }
            }
            Node::Map(m)
        }
        Value::Guid(g) => Node::String(guid_to_string(g)),
        Value::Props(pl) => project_props(pl),
        Value::Array(a) => Node::Array(a.elems.iter().map(project_value).collect()),
        Value::Set(s) => Node::Array(s.elems.iter().map(project_value).collect()),
        Value::Map(m) => Node::Array(
            m.entries
                .iter()
                .map(|(k, v)| Node::map_from([("key", project_value(k)), ("value", project_value(v))]))
                .collect(),
        ),
        Value::Raw(b) => Node::Bytes(b.clone()),
    }
}

fn count_raw(v: &Value) -> usize {
    match v {
        Value::Raw(_) => 1,
        Value::Props(pl) => pl.props.iter().map(|p| count_raw(&p.value)).sum(),
        Value::Array(a) => a.elems.iter().map(count_raw).sum(),
        Value::Set(s) => s.elems.iter().map(count_raw).sum(),
        Value::Map(m) => m.entries.iter().map(|(k, v)| count_raw(k) + count_raw(v)).sum(),
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Edits
// ---------------------------------------------------------------------------

fn expect_str<'n>(node: &'n Node, path: &NodePath) -> CodecResult<&'n str> {
    match node {
        Node::String(s) => Ok(s),
        other => Err(CodecError::type_mismatch(CODEC, path, "string", other.kind().name())),
    }
}

/// True when both nodes have the same map/array skeleton (keys in order).
fn same_shape(a: &Node, b: &Node) -> bool {
    match (a, b) {
        (Node::Map(x), Node::Map(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|((ka, va), (kb, vb))| ka == kb && same_shape(va, vb))
        }
        (Node::Map(_), _) | (_, Node::Map(_)) => false,
        _ => true,
    }
}

fn check_shape(current: &Node, edited: &Node, path: &NodePath, what: &str) -> CodecResult<()> {
    if same_shape(current, edited) {
        Ok(())
    } else {
        Err(CodecError::structure(CODEC, path, format!("the fields of {what} cannot be added, removed or renamed")))
    }
}

fn reconcile_props(pl: &mut PropList, node: &Node, path: &NodePath) -> CodecResult<()> {
    let Node::Map(m) = node else {
        return Err(CodecError::type_mismatch(CODEC, path, "map", node.kind().name()));
    };
    let keys = prop_keys(&pl.props);
    if m.len() != keys.len() || !keys.iter().zip(m.keys()).all(|(a, b)| a == b) {
        return Err(CodecError::structure(
            CODEC,
            path,
            "GVAS properties cannot be added, removed, renamed or reordered",
        ));
    }
    for (p, (k, child)) in pl.props.iter_mut().zip(m) {
        reconcile_value(&mut p.value, child, &path.key(k.clone()))?;
    }
    Ok(())
}

fn new_elem(kind: ElemKind, path: &NodePath) -> CodecResult<Value> {
    Ok(match kind {
        ElemKind::Scalar(t) => Value::Scalar(Scalar::zero(t)),
        ElemKind::Str => Value::Str(FStr::new("")),
        ElemKind::StrSeq(n) => Value::StrSeq(vec![FStr::new(""); n]),
        ElemKind::Bin(l) => Value::Bin(l, l.fields.iter().map(|f| Scalar::zero(f.ty)).collect()),
        ElemKind::Guid => Value::Guid([0; 16]),
        ElemKind::Props => {
            return Err(CodecError::structure(
                CODEC,
                path,
                "elements that are property lists cannot be added (their property layout is unknown)",
            ));
        }
    })
}

fn reconcile_seq(elems: &mut Vec<Value>, kind: ElemKind, node: &Node, path: &NodePath) -> CodecResult<()> {
    let Node::Array(items) = node else {
        return Err(CodecError::type_mismatch(CODEC, path, "array", node.kind().name()));
    };
    if items.len() < elems.len() {
        elems.truncate(items.len());
    }
    while elems.len() < items.len() {
        elems.push(new_elem(kind, &path.index(elems.len()))?);
    }
    for (i, (e, item)) in elems.iter_mut().zip(items).enumerate() {
        reconcile_value(e, item, &path.index(i))?;
    }
    Ok(())
}

fn reconcile_value(v: &mut Value, node: &Node, path: &NodePath) -> CodecResult<()> {
    let current = project_value(v);
    if bit_equal(&current, node) {
        return Ok(());
    }
    match v {
        Value::Scalar(s) => *s = s.edited(node, path)?,
        Value::Str(s) => s.set_text(expect_str(node, path)?),
        Value::StrSeq(list) => {
            check_shape(&current, node, path, "a soft object path")?;
            let names = strseq_names(list.len());
            for (s, name) in list.iter_mut().zip(names) {
                let child = node
                    .get(&NodePath::root().key(*name))
                    .ok_or_else(|| CodecError::path_not_found(CODEC, path.key(*name)))?;
                s.set_text(expect_str(child, &path.key(*name))?);
            }
        }
        Value::Bin(l, vals) => {
            if l.direct {
                if let Some(s) = vals.first_mut() {
                    *s = s.edited(node, path)?;
                }
                return Ok(());
            }
            check_shape(&current, node, path, l.name)?;
            for (f, s) in l.fields.iter().zip(vals.iter_mut()) {
                let mut rel = NodePath::root();
                if let Some(g) = f.group {
                    rel = rel.key(g);
                }
                rel = rel.key(f.name);
                let child_path = path.join(&rel);
                let child = node.get(&rel).ok_or_else(|| CodecError::path_not_found(CODEC, &child_path))?;
                *s = s.edited(child, &child_path)?;
            }
        }
        Value::Guid(g) => *g = parse_guid(expect_str(node, path)?, path)?,
        Value::Props(pl) => reconcile_props(pl, node, path)?,
        Value::Array(a) => {
            let kind = a.kind;
            reconcile_seq(&mut a.elems, kind, node, path)?;
        }
        Value::Set(s) => {
            let kind = s.kind;
            reconcile_seq(&mut s.elems, kind, node, path)?;
        }
        Value::Map(m) => {
            let Node::Array(items) = node else {
                return Err(CodecError::type_mismatch(CODEC, path, "array", node.kind().name()));
            };
            if items.len() != m.entries.len() {
                return Err(CodecError::structure(CODEC, path, "GVAS map entries cannot be added or removed"));
            }
            for (i, ((k, val), item)) in m.entries.iter_mut().zip(items).enumerate() {
                let ep = path.index(i);
                let Node::Map(em) = item else {
                    return Err(CodecError::type_mismatch(CODEC, &ep, "map", item.kind().name()));
                };
                if em.len() != 2 {
                    return Err(CodecError::structure(CODEC, &ep, "map entries must have exactly `key` and `value`"));
                }
                let kn = em.get("key").ok_or_else(|| CodecError::path_not_found(CODEC, ep.key("key")))?;
                let vn = em.get("value").ok_or_else(|| CodecError::path_not_found(CODEC, ep.key("value")))?;
                reconcile_value(k, kn, &ep.key("key"))?;
                reconcile_value(val, vn, &ep.key("value"))?;
            }
        }
        Value::Raw(b) => {
            let new = match node {
                Node::Bytes(n) => n,
                Node::RawRegion(r) => &r.bytes,
                other => return Err(CodecError::type_mismatch(CODEC, path, "bytes", other.kind().name())),
            };
            if new.len() != b.len() {
                return Err(CodecError::unencodable(
                    CODEC,
                    path,
                    format!(
                        "raw (unparsed) property values only accept same-length edits ({} bytes, edit has {})",
                        b.len(),
                        new.len()
                    ),
                ));
            }
            b.copy_from_slice(new);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Document / codec
// ---------------------------------------------------------------------------

/// A parsed GVAS save.
#[derive(Debug, Clone)]
pub struct GvasDocument {
    header: Header,
    fmt: Fmt,
    root: PropList,
    trailing: Vec<u8>,
}

impl GvasDocument {
    /// Parses a GVAS payload.
    pub fn parse(bytes: &[u8]) -> CodecResult<GvasDocument> {
        let mut r = ByteReader::new(bytes, CODEC);
        let header = Header::read(&mut r)?;
        let fmt = Fmt::from_header(&header);
        let root = read_props(&mut r, fmt, 0)?;
        let trailing = r.rest().to_vec();
        Ok(GvasDocument { header, fmt, root, trailing })
    }

    /// Serialises the document.
    pub fn serialize(&self) -> CodecResult<Vec<u8>> {
        let mut out = Vec::new();
        self.header.write(&mut out)?;
        write_props(&self.root, &mut out)?;
        out.extend_from_slice(&self.trailing);
        Ok(out)
    }

    /// Save-game class name stored in the header.
    pub fn class_name(&self) -> &str {
        &self.header.class_name.text
    }

    fn type_summary(&self) -> String {
        let types: Vec<String> = self
            .root
            .props
            .iter()
            .take(1)
            .map(|p| match &p.tag {
                Tag::Classic { type_name, .. } => type_name.text.clone(),
                Tag::Complete { type_name, .. } => type_name.display(),
            })
            .collect();
        types.join(",")
    }
}

impl Document for GvasDocument {
    fn codec_id(&self) -> &'static str {
        CODEC
    }

    fn to_ast(&self) -> Node {
        project_props(&self.root)
    }

    fn apply_ast(&mut self, root: &Node) -> CodecResult<()> {
        let mut work = self.root.clone();
        reconcile_props(&mut work, root, &NodePath::root())?;
        self.root = work;
        Ok(())
    }

    fn encode(&self) -> CodecResult<Vec<u8>> {
        self.serialize()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::VALUES_ONLY
    }

    fn endianness(&self) -> Endianness {
        Endianness::Little
    }

    fn notes(&self) -> Vec<String> {
        let h = &self.header;
        let mut n = vec![
            format!("UE {}.{}.{} SaveGameVersion {}", h.major, h.minor, h.patch, h.save_game_version),
            format!(
                "package version UE4 {}{}; engine changelist {} branch `{}`",
                h.package_ue4,
                h.package_ue5.map(|v| format!(", UE5 {v}")).unwrap_or_default(),
                h.changelist,
                h.branch.text
            ),
            format!("save game class `{}`, {} custom version(s)", h.class_name.text, h.custom_count()),
        ];
        if self.fmt.complete_tags {
            let first = self.type_summary();
            n.push(if first.is_empty() {
                "UE 5.4+ complete type name property tags".to_string()
            } else {
                format!("UE 5.4+ complete type name property tags (first: {first})")
            });
        } else if self.fmt.tag_extension {
            n.push("classic property tags with property-extension byte (UE 5.3)".into());
        }
        let raw: usize = self.root.props.iter().map(|p| count_raw(&p.value)).sum();
        if raw > 0 {
            n.push(format!(
                "{raw} property value(s) kept as raw bytes (unknown or unparseable type; same-length edits only)"
            ));
        }
        if !self.trailing.is_empty() {
            n.push(format!("{} trailing byte(s) after the property list kept verbatim", self.trailing.len()));
        }
        n
    }

    fn clone_box(&self) -> Box<dyn Document> {
        Box::new(self.clone())
    }
}

/// Unreal Engine GVAS codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct GvasCodec;

impl StructuralCodec for GvasCodec {
    fn id(&self) -> &'static str {
        CODEC
    }

    fn name(&self) -> &'static str {
        "Unreal Engine GVAS (.sav)"
    }

    fn detect(&self, bytes: &[u8], _ctx: &DetectContext) -> Confidence {
        if !bytes.starts_with(MAGIC) {
            return 0;
        }
        let mut r = ByteReader::new(bytes, CODEC);
        if Header::read(&mut r).is_ok() {
            95
        } else {
            0
        }
    }

    fn decode(&self, bytes: &[u8], _ctx: &DetectContext) -> CodecResult<Box<dyn Document>> {
        let doc = GvasDocument::parse(bytes)?;
        // Never hand out a document that cannot reproduce its input: the
        // pipeline then falls back to the lossless raw codec.
        let out = doc.serialize()?;
        if out != bytes {
            let at = out.iter().zip(bytes).position(|(a, b)| a != b).unwrap_or(out.len().min(bytes.len()));
            return Err(CodecError::unsupported(
                CODEC,
                format!("re-serialisation is not bit-exact (first difference at offset {at:#x})"),
            ));
        }
        Ok(Box::new(doc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal independent byte builders for unit tests.
    fn s(out: &mut Vec<u8>, text: &str) {
        if text.is_empty() {
            out.extend_from_slice(&0i32.to_le_bytes());
            return;
        }
        out.extend_from_slice(&((text.len() + 1) as i32).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
        out.push(0);
    }

    fn s16(out: &mut Vec<u8>, text: &str) {
        let units: Vec<u16> = text.encode_utf16().collect();
        out.extend_from_slice(&(-((units.len() + 1) as i32)).to_le_bytes());
        for u in units {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out.extend_from_slice(&[0, 0]);
    }

    fn header(ue5: Option<i32>) -> Vec<u8> {
        let mut o = b"GVAS".to_vec();
        o.extend_from_slice(&(if ue5.is_some() { 3i32 } else { 2i32 }).to_le_bytes());
        o.extend_from_slice(&522i32.to_le_bytes());
        if let Some(v) = ue5 {
            o.extend_from_slice(&v.to_le_bytes());
        }
        for v in [5u16, 4, 1] {
            o.extend_from_slice(&v.to_le_bytes());
        }
        o.extend_from_slice(&12345u32.to_le_bytes());
        s(&mut o, "++UE5+Release-5.4");
        o.extend_from_slice(&3i32.to_le_bytes());
        o.extend_from_slice(&1i32.to_le_bytes());
        o.extend_from_slice(&[7u8; 16]);
        o.extend_from_slice(&4i32.to_le_bytes());
        s(&mut o, "/Script/Game.TestSave");
        o
    }

    fn classic_int(o: &mut Vec<u8>, name: &str, v: i32, ext: bool) {
        s(o, name);
        s(o, "IntProperty");
        o.extend_from_slice(&4i32.to_le_bytes());
        o.extend_from_slice(&0i32.to_le_bytes());
        o.push(0);
        if ext {
            o.push(0);
        }
        o.extend_from_slice(&v.to_le_bytes());
    }

    /// Writes a UE 5.4 type name: (name, param count) pairs in pre-order.
    fn type_name(o: &mut Vec<u8>, nodes: &[(&str, i32)]) {
        for (n, c) in nodes {
            s(o, n);
            o.extend_from_slice(&c.to_le_bytes());
        }
    }

    fn decode(bytes: &[u8]) -> GvasDocument {
        let doc = GvasDocument::parse(bytes).unwrap();
        assert_eq!(doc.serialize().unwrap(), bytes, "round trip");
        doc
    }

    #[test]
    fn fstring_encodings_round_trip() {
        for (text, utf16) in [("", false), ("abc", false), ("Ünïcödé", true), ("\u{1F600} emoji", true)] {
            let mut b = Vec::new();
            if utf16 {
                s16(&mut b, text);
            } else {
                s(&mut b, text);
            }
            let mut r = ByteReader::new(&b, CODEC);
            let f = FStr::read(&mut r, "t").unwrap();
            assert_eq!(f.text, text);
            let mut out = Vec::new();
            f.write(&mut out).unwrap();
            assert_eq!(out, b);
        }
        // A single NUL (length 1) is distinct from length 0.
        let b = [1u8, 0, 0, 0, 0];
        let f = FStr::read(&mut ByteReader::new(&b, CODEC), "t").unwrap();
        assert_eq!(f.enc, StrEncoding::Latin1);
        let mut out = Vec::new();
        f.write(&mut out).unwrap();
        assert_eq!(out, b);
        // Latin-1 bytes above 0x7f survive.
        let b = [3u8, 0, 0, 0, 0xe9, 0xff, 0];
        let f = FStr::read(&mut ByteReader::new(&b, CODEC), "t").unwrap();
        assert_eq!(f.text, "\u{e9}\u{ff}");
        let mut out = Vec::new();
        f.write(&mut out).unwrap();
        assert_eq!(out, b);
    }

    #[test]
    fn fstring_rejects_bad_input() {
        for b in [
            vec![3u8, 0, 0, 0, b'a', b'b', b'c'],
            vec![0xff, 0xff, 0xff, 0xff, b'a', 0],
            vec![0, 0, 0, 0x80],
            vec![0xfe, 0xff, 0xff, 0xff, 0x00, 0xd8, 0, 0],
            vec![0xff, 0xff, 0xff, 0x7f],
        ] {
            assert!(FStr::read(&mut ByteReader::new(&b, CODEC), "t").is_err(), "{b:?}");
        }
    }

    #[test]
    fn set_text_keeps_or_upgrades_encoding() {
        let mut f = FStr::new("abc");
        f.set_text("d\u{e9}f");
        assert_eq!(f.enc, StrEncoding::Latin1);
        f.set_text("\u{4e16}");
        assert_eq!(f.enc, StrEncoding::Utf16);
        f.set_text("plain");
        assert_eq!(f.enc, StrEncoding::Utf16, "UTF-16 strings stay UTF-16");
        let mut e = FStr::new("");
        e.set_text("x");
        assert_eq!(e.enc, StrEncoding::Latin1);
    }

    #[test]
    fn guid_text_round_trip() {
        let g: [u8; 16] = std::array::from_fn(|i| i as u8 * 17);
        let text = guid_to_string(&g);
        assert_eq!(text.len(), 32);
        assert_eq!(parse_guid(&text, &NodePath::root()).unwrap(), g);
        let dashed = format!("{}-{}-{}-{}", &text[..8], &text[8..16], &text[16..24], &text[24..]);
        assert_eq!(parse_guid(&dashed, &NodePath::root()).unwrap(), g);
        assert!(parse_guid("xyz", &NodePath::root()).is_err());
        assert!(parse_guid(&"G".repeat(32), &NodePath::root()).is_err());
    }

    #[test]
    fn unknown_type_becomes_raw_and_accepts_same_length_edits() {
        let mut b = header(None);
        s(&mut b, "Weird");
        s(&mut b, "DelegateProperty");
        b.extend_from_slice(&5i32.to_le_bytes());
        b.extend_from_slice(&0i32.to_le_bytes());
        b.push(0);
        b.extend_from_slice(&[1, 2, 3, 4, 5]);
        s(&mut b, "None");
        let mut doc = decode(&b);
        let ast = doc.to_ast();
        assert_eq!(ast.get(&NodePath::parse("Weird").unwrap()), Some(&Node::Bytes(vec![1, 2, 3, 4, 5])));
        assert!(doc.notes().iter().any(|n| n.contains("raw bytes")));
        let mut edited = ast.clone();
        edited.set(&NodePath::parse("Weird").unwrap(), Node::Bytes(vec![9, 9, 9, 9, 9]), false).unwrap();
        doc.apply_ast(&edited).unwrap();
        assert!(doc.serialize().unwrap().ends_with(&[9, 9, 9, 9, 9, 5, 0, 0, 0, b'N', b'o', b'n', b'e', 0]));
        edited.set(&NodePath::parse("Weird").unwrap(), Node::Bytes(vec![1]), false).unwrap();
        assert!(doc.apply_ast(&edited).is_err());
    }

    #[test]
    fn int_property_with_wrong_size_falls_back_to_raw() {
        let mut b = header(None);
        s(&mut b, "Odd");
        s(&mut b, "IntProperty");
        b.extend_from_slice(&6i32.to_le_bytes());
        b.extend_from_slice(&0i32.to_le_bytes());
        b.push(0);
        b.extend_from_slice(&[1, 0, 0, 0, 0xaa, 0xbb]);
        s(&mut b, "None");
        let doc = decode(&b);
        assert!(matches!(doc.to_ast().get(&NodePath::parse("Odd").unwrap()), Some(Node::Bytes(v)) if v.len() == 6));
    }

    #[test]
    fn array_index_and_duplicate_keys() {
        let mut b = header(None);
        for (idx, v) in [(0i32, 10i32), (1, 11), (2, 12)] {
            s(&mut b, "Stat");
            s(&mut b, "IntProperty");
            b.extend_from_slice(&4i32.to_le_bytes());
            b.extend_from_slice(&idx.to_le_bytes());
            b.push(0);
            b.extend_from_slice(&v.to_le_bytes());
        }
        classic_int(&mut b, "Stat", 99, false);
        s(&mut b, "None");
        let doc = decode(&b);
        let Node::Map(m) = doc.to_ast() else { panic!() };
        let keys: Vec<&str> = m.keys().map(String::as_str).collect();
        assert_eq!(keys, ["Stat", "Stat#1", "Stat#2", "Stat#3"]);
        assert_eq!(m["Stat#3"], Node::I32(99));
    }

    #[test]
    fn ue53_extension_byte_round_trip_and_edit() {
        let mut b = header(Some(UE5_PROPERTY_TAG_EXTENSION));
        classic_int(&mut b, "Gold", 100, true);
        s(&mut b, "None");
        b.extend_from_slice(&[0; 4]);
        let mut doc = decode(&b);
        assert!(doc.fmt.tag_extension);
        let mut ast = doc.to_ast();
        ast.set(&NodePath::parse("Gold").unwrap(), Node::I32(5000), false).unwrap();
        doc.apply_ast(&ast).unwrap();
        let out = doc.serialize().unwrap();
        let re = decode(&out);
        assert_eq!(re.to_ast().get(&NodePath::parse("Gold").unwrap()), Some(&Node::I32(5000)));
        // Non-zero extensions are not guessed.
        let mut bad = header(Some(UE5_PROPERTY_TAG_EXTENSION));
        s(&mut bad, "Gold");
        s(&mut bad, "IntProperty");
        bad.extend_from_slice(&4i32.to_le_bytes());
        bad.extend_from_slice(&0i32.to_le_bytes());
        bad.push(0);
        bad.push(2);
        bad.extend_from_slice(&1i32.to_le_bytes());
        s(&mut bad, "None");
        assert!(matches!(GvasCodec.decode(&bad, &DetectContext::default()), Err(CodecError::Unsupported { .. })));
    }

    /// UE 5.4 complete type name tags.
    fn ue54_file() -> Vec<u8> {
        let mut b = header(Some(UE5_PROPERTY_TAG_COMPLETE_TYPE_NAME));
        // Int with array index 2 and a property guid.
        s(&mut b, "Score");
        type_name(&mut b, &[("IntProperty", 0)]);
        b.extend_from_slice(&4i32.to_le_bytes());
        b.push(FLAG_HAS_ARRAY_INDEX | FLAG_HAS_PROPERTY_GUID);
        b.extend_from_slice(&2i32.to_le_bytes());
        b.extend_from_slice(&[0xab; 16]);
        b.extend_from_slice(&77i32.to_le_bytes());
        // Bool true via flags.
        s(&mut b, "bAlive");
        type_name(&mut b, &[("BoolProperty", 0)]);
        b.extend_from_slice(&0i32.to_le_bytes());
        b.push(FLAG_BOOL_TRUE);
        // Vector (double, LWC).
        s(&mut b, "Pos");
        type_name(&mut b, &[("StructProperty", 1), ("Vector", 1), ("/Script/CoreUObject", 0)]);
        b.extend_from_slice(&24i32.to_le_bytes());
        b.push(0x08);
        for v in [1.5f64, -2.0, 3.25] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        // Byte with enum -> FString.
        s(&mut b, "Difficulty");
        type_name(&mut b, &[("ByteProperty", 1), ("EDifficulty", 1), ("/Script/Game", 0)]);
        let mut val = Vec::new();
        s(&mut val, "EDifficulty::Hard");
        b.extend_from_slice(&(val.len() as i32).to_le_bytes());
        b.push(0);
        b.extend_from_slice(&val);
        // Array of structs without inner tag.
        s(&mut b, "Points");
        type_name(&mut b, &[("ArrayProperty", 1), ("StructProperty", 1), ("IntPoint", 1), ("/Script/CoreUObject", 0)]);
        b.extend_from_slice(&20i32.to_le_bytes());
        b.push(0);
        b.extend_from_slice(&2i32.to_le_bytes());
        for v in [1i32, 2, 3, 4] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        // Map<Str, Int>.
        s(&mut b, "Counts");
        type_name(&mut b, &[("MapProperty", 2), ("StrProperty", 0), ("IntProperty", 0)]);
        let mut val = Vec::new();
        val.extend_from_slice(&0i32.to_le_bytes());
        val.extend_from_slice(&1i32.to_le_bytes());
        s(&mut val, "wood");
        val.extend_from_slice(&12i32.to_le_bytes());
        b.extend_from_slice(&(val.len() as i32).to_le_bytes());
        b.push(0);
        b.extend_from_slice(&val);
        s(&mut b, "None");
        b.extend_from_slice(&[0; 4]);
        b
    }

    #[test]
    fn ue54_complete_type_names() {
        let b = ue54_file();
        let mut doc = decode(&b);
        assert!(doc.fmt.complete_tags);
        let ast = doc.to_ast();
        let get = |p: &str| ast.get(&NodePath::parse(p).unwrap()).cloned();
        assert_eq!(get("Score#2"), Some(Node::I32(77)));
        assert_eq!(get("bAlive"), Some(Node::Bool(true)));
        assert_eq!(get("Pos.X"), Some(Node::F64(1.5)));
        assert_eq!(get("Pos.Z"), Some(Node::F64(3.25)));
        assert_eq!(get("Difficulty"), Some(Node::String("EDifficulty::Hard".into())));
        assert_eq!(get("Points[1].Y"), Some(Node::I32(4)));
        assert_eq!(get("Counts[0].key"), Some(Node::String("wood".into())));
        assert_eq!(get("Counts[0].value"), Some(Node::I32(12)));
        assert!(doc.notes().iter().any(|n| n.contains("5.4")));

        let mut edited = ast.clone();
        edited.set(&NodePath::parse("bAlive").unwrap(), Node::Bool(false), false).unwrap();
        edited.set(&NodePath::parse("Pos.Y").unwrap(), Node::F64(100.0), false).unwrap();
        edited
            .set(&NodePath::parse("Difficulty").unwrap(), Node::String("EDifficulty::Nightmare".into()), false)
            .unwrap();
        doc.apply_ast(&edited).unwrap();
        let out = doc.serialize().unwrap();
        let re = decode(&out);
        assert!(bit_equal(&re.to_ast(), &edited));
    }

    #[test]
    fn ue54_unknown_flags_are_unsupported() {
        let mut b = header(Some(UE5_PROPERTY_TAG_COMPLETE_TYPE_NAME));
        s(&mut b, "X");
        type_name(&mut b, &[("IntProperty", 0)]);
        b.extend_from_slice(&4i32.to_le_bytes());
        b.push(0x80);
        b.extend_from_slice(&1i32.to_le_bytes());
        s(&mut b, "None");
        assert!(matches!(GvasCodec.decode(&b, &DetectContext::default()), Err(CodecError::Unsupported { .. })));
    }

    #[test]
    fn detect_requires_parseable_header() {
        let ctx = DetectContext::for_file("Slot.sav");
        assert_eq!(GvasCodec.detect(&ue54_file(), &ctx), 95);
        assert_eq!(GvasCodec.detect(b"GVAS\x02\x00", &ctx), 0);
        assert_eq!(GvasCodec.detect(b"{\"a\":1}", &ctx), 0);
    }

    #[test]
    fn every_truncation_is_handled_without_panic() {
        let b = ue54_file();
        for n in 0..b.len() {
            let cut = &b[..n];
            if let Ok(doc) = GvasCodec.decode(cut, &DetectContext::default()) {
                assert_eq!(doc.encode().unwrap(), cut);
            }
        }
        // Corrupt every byte in turn.
        for i in 0..b.len() {
            let mut c = b.clone();
            c[i] ^= 0x5a;
            if let Ok(doc) = GvasCodec.decode(&c, &DetectContext::default()) {
                assert_eq!(doc.encode().unwrap(), c);
            }
        }
    }

    #[test]
    fn deep_nesting_is_bounded() {
        // A chain of nested structs deeper than MAX_DEPTH must not overflow the stack.
        fn nested(level: usize) -> Vec<u8> {
            let mut inner = Vec::new();
            if level > 0 {
                let v = nested(level - 1);
                s(&mut inner, "Child");
                s(&mut inner, "StructProperty");
                inner.extend_from_slice(&(v.len() as i32).to_le_bytes());
                inner.extend_from_slice(&0i32.to_le_bytes());
                s(&mut inner, "Node");
                inner.extend_from_slice(&[0; 16]);
                inner.push(0);
                inner.extend_from_slice(&v);
            }
            s(&mut inner, "None");
            inner
        }
        let mut b = header(None);
        b.extend_from_slice(&nested(MAX_DEPTH + 10));
        let doc = decode(&b);
        assert!(count_raw(&Value::Props(doc.root.clone())) == 1, "the too-deep tail is kept raw");
    }

    #[test]
    fn structural_edits_are_rejected() {
        let mut b = header(None);
        classic_int(&mut b, "A", 1, false);
        s(&mut b, "None");
        let mut doc = decode(&b);
        let mut ast = doc.to_ast();
        ast.set(&NodePath::parse("B").unwrap(), Node::I32(2), true).unwrap();
        assert!(matches!(doc.apply_ast(&ast), Err(CodecError::StructureUnsupported { .. })));
        let mut ast = doc.to_ast();
        ast.set(&NodePath::parse("A").unwrap(), Node::String("x".into()), false).unwrap();
        assert!(matches!(doc.apply_ast(&ast), Err(CodecError::TypeMismatch { .. })));
        let mut ast = doc.to_ast();
        ast.set(&NodePath::parse("A").unwrap(), Node::I64(1 << 40), false).unwrap();
        assert!(doc.apply_ast(&ast).is_err());
        assert_eq!(doc.serialize().unwrap(), b, "failed edits leave the document untouched");
    }
}
