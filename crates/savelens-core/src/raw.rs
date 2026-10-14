//! Raw byte regions and primitive (de)serialisation at arbitrary offsets.
//!
//! A [`RawRegion`] is how the AST represents bytes whose structure is unknown.
//! It keeps the bytes verbatim plus an index of human-recognisable values
//! (ASCII/UTF-16 strings) and fixed-size block hashes used to diff huge blobs
//! quickly.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};
use crate::node::ScalarValue;

/// Byte order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Endianness {
    #[default]
    Little,
    Big,
}

impl Endianness {
    /// Short suffix used in location strings (`le` / `be`).
    pub fn suffix(self) -> &'static str {
        match self {
            Endianness::Little => "le",
            Endianness::Big => "be",
        }
    }
}

/// Primitive value encodings the scanner understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub enum ScanType {
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
    /// Decimal number stored as ASCII text inside a raw region (`"1500"`),
    /// or a numeric string leaf in the AST.
    AsciiDecimal,
}

impl ScanType {
    /// All binary numeric scan types.
    pub const BINARY: [ScanType; 10] = [
        ScanType::I8,
        ScanType::I16,
        ScanType::I32,
        ScanType::I64,
        ScanType::U8,
        ScanType::U16,
        ScanType::U32,
        ScanType::U64,
        ScanType::F32,
        ScanType::F64,
    ];

    /// The most common types for game values (default scan set).
    pub const COMMON: [ScanType; 5] = [ScanType::I32, ScanType::U32, ScanType::I64, ScanType::F32, ScanType::F64];

    /// Width in bytes for binary types; `None` for variable width text.
    pub fn width(self) -> Option<usize> {
        Some(match self {
            ScanType::I8 | ScanType::U8 => 1,
            ScanType::I16 | ScanType::U16 => 2,
            ScanType::I32 | ScanType::U32 | ScanType::F32 => 4,
            ScanType::I64 | ScanType::U64 | ScanType::F64 => 8,
            ScanType::AsciiDecimal => return None,
        })
    }

    /// True for `f32`/`f64`.
    pub fn is_float(self) -> bool {
        matches!(self, ScanType::F32 | ScanType::F64)
    }

    /// True when endianness matters for this type.
    pub fn is_multibyte(self) -> bool {
        self.width().is_some_and(|w| w > 1)
    }

    /// Short name (`i32`, `f64`, `ascii`).
    pub fn name(self) -> &'static str {
        match self {
            ScanType::I8 => "i8",
            ScanType::I16 => "i16",
            ScanType::I32 => "i32",
            ScanType::I64 => "i64",
            ScanType::U8 => "u8",
            ScanType::U16 => "u16",
            ScanType::U32 => "u32",
            ScanType::U64 => "u64",
            ScanType::F32 => "f32",
            ScanType::F64 => "f64",
            ScanType::AsciiDecimal => "ascii",
        }
    }

    /// Checks whether `value` is representable by this type.
    pub fn can_represent(self, value: ScalarValue) -> bool {
        match self {
            ScanType::F32 => {
                let f = value.as_f64();
                f.is_nan() || f.is_infinite() || f.abs() <= f32::MAX as f64
            }
            ScanType::F64 => true,
            ScanType::AsciiDecimal => value.as_i128().is_some(),
            int => {
                let Some(i) = value.as_i128() else { return false };
                let (min, max) = int_bounds(int);
                i >= min && i <= max
            }
        }
    }
}

impl fmt::Display for ScanType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for ScanType {
    type Err = CoreError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "i8" | "sbyte" => ScanType::I8,
            "i16" | "short" => ScanType::I16,
            "i32" | "int" => ScanType::I32,
            "i64" | "long" => ScanType::I64,
            "u8" | "byte" => ScanType::U8,
            "u16" | "ushort" => ScanType::U16,
            "u32" | "uint" => ScanType::U32,
            "u64" | "ulong" => ScanType::U64,
            "f32" | "float" => ScanType::F32,
            "f64" | "double" => ScanType::F64,
            "ascii" | "str" | "string" => ScanType::AsciiDecimal,
            other => {
                return Err(CoreError::ValueParse {
                    input: other.to_string(),
                    target: "scan type".into(),
                    reason: "expected one of i8,i16,i32,i64,u8,u16,u32,u64,f32,f64,ascii".into(),
                })
            }
        })
    }
}

fn int_bounds(t: ScanType) -> (i128, i128) {
    match t {
        ScanType::I8 => (i8::MIN as i128, i8::MAX as i128),
        ScanType::I16 => (i16::MIN as i128, i16::MAX as i128),
        ScanType::I32 => (i32::MIN as i128, i32::MAX as i128),
        ScanType::I64 => (i64::MIN as i128, i64::MAX as i128),
        ScanType::U8 => (0, u8::MAX as i128),
        ScanType::U16 => (0, u16::MAX as i128),
        ScanType::U32 => (0, u32::MAX as i128),
        ScanType::U64 => (0, u64::MAX as i128),
        ScanType::AsciiDecimal => (i64::MIN as i128, i64::MAX as i128),
        ScanType::F32 | ScanType::F64 => (i128::MIN, i128::MAX),
    }
}

/// Reads a primitive at `offset`. Returns `None` when it does not fit.
///
/// For [`ScanType::AsciiDecimal`] the offset must be the first digit (or `-`)
/// of a run that is not preceded by another digit.
pub fn read_primitive(bytes: &[u8], offset: usize, ty: ScanType, endian: Endianness) -> Option<ScalarValue> {
    if ty == ScanType::AsciiDecimal {
        return read_ascii_decimal(bytes, offset).map(|(v, _)| ScalarValue::Int(v));
    }
    let w = ty.width()?;
    let end = offset.checked_add(w)?;
    let s = bytes.get(offset..end)?;
    macro_rules! rd {
        ($t:ty) => {{
            let arr: [u8; std::mem::size_of::<$t>()] = s.try_into().ok()?;
            match endian {
                Endianness::Little => <$t>::from_le_bytes(arr),
                Endianness::Big => <$t>::from_be_bytes(arr),
            }
        }};
    }
    Some(match ty {
        ScanType::I8 => ScalarValue::Int(s[0] as i8 as i128),
        ScanType::U8 => ScalarValue::Int(s[0] as i128),
        ScanType::I16 => ScalarValue::Int(rd!(i16) as i128),
        ScanType::U16 => ScalarValue::Int(rd!(u16) as i128),
        ScanType::I32 => ScalarValue::Int(rd!(i32) as i128),
        ScanType::U32 => ScalarValue::Int(rd!(u32) as i128),
        ScanType::I64 => ScalarValue::Int(rd!(i64) as i128),
        ScanType::U64 => ScalarValue::Int(rd!(u64) as i128),
        ScanType::F32 => ScalarValue::Float(rd!(f32) as f64),
        ScanType::F64 => ScalarValue::Float(rd!(f64)),
        ScanType::AsciiDecimal => return None,
    })
}

/// Reads an ASCII decimal integer starting at `offset`; returns `(value, byte_len)`.
pub fn read_ascii_decimal(bytes: &[u8], offset: usize) -> Option<(i128, usize)> {
    let first = *bytes.get(offset)?;
    if offset > 0 && bytes[offset - 1].is_ascii_digit() {
        return None;
    }
    let neg = first == b'-';
    let start = if neg { offset + 1 } else { offset };
    let mut end = start;
    while end < bytes.len() && bytes[end].is_ascii_digit() && end - start < 19 {
        end += 1;
    }
    if end == start {
        return None;
    }
    if end < bytes.len() && bytes[end].is_ascii_digit() {
        // Longer than 19 digits: not a plausible value.
        return None;
    }
    let text = std::str::from_utf8(&bytes[start..end]).ok()?;
    let v: i128 = text.parse().ok()?;
    Some((if neg { -v } else { v }, end - offset))
}

/// Encodes a value as the given primitive (range checked).
pub fn encode_primitive(value: ScalarValue, ty: ScanType, endian: Endianness) -> CoreResult<Vec<u8>> {
    if !ty.can_represent(value) {
        return Err(CoreError::ValueOutOfRange {
            path: String::new(),
            value: value.to_string(),
            target: ty.name().to_string(),
        });
    }
    let int = || -> CoreResult<i128> {
        value.as_i128().ok_or_else(|| CoreError::TypeMismatch {
            path: String::new(),
            expected: ty.name().to_string(),
            found: "non-integral float".into(),
        })
    };
    macro_rules! wr {
        ($v:expr) => {{
            let v = $v;
            match endian {
                Endianness::Little => v.to_le_bytes().to_vec(),
                Endianness::Big => v.to_be_bytes().to_vec(),
            }
        }};
    }
    Ok(match ty {
        ScanType::I8 => vec![int()? as i8 as u8],
        ScanType::U8 => vec![int()? as u8],
        ScanType::I16 => wr!(int()? as i16),
        ScanType::U16 => wr!(int()? as u16),
        ScanType::I32 => wr!(int()? as i32),
        ScanType::U32 => wr!(int()? as u32),
        ScanType::I64 => wr!(int()? as i64),
        ScanType::U64 => wr!(int()? as u64),
        ScanType::F32 => wr!(value.as_f64() as f32),
        ScanType::F64 => wr!(value.as_f64()),
        ScanType::AsciiDecimal => int()?.to_string().into_bytes(),
    })
}

/// Writes a primitive into `bytes` at `offset`.
///
/// ASCII decimals must keep the same number of characters (zero padding is
/// used when the new value is shorter and the original had leading zeros is
/// not assumed): a value with a different textual width is rejected because
/// it would shift every following byte.
pub fn write_primitive(
    bytes: &mut [u8],
    offset: usize,
    ty: ScanType,
    endian: Endianness,
    value: ScalarValue,
    region_name: &str,
) -> CoreResult<()> {
    let encoded = encode_primitive(value, ty, endian).map_err(|e| with_path(e, region_name))?;
    let width = if ty == ScanType::AsciiDecimal {
        let (_, old_len) = read_ascii_decimal(bytes, offset).ok_or_else(|| CoreError::RawOffsetOutOfRange {
            path: region_name.to_string(),
            offset: offset as u64,
            width: 1,
            len: bytes.len(),
        })?;
        if old_len != encoded.len() {
            return Err(CoreError::ValueOutOfRange {
                path: region_name.to_string(),
                value: value.to_string(),
                target: format!("ascii number of exactly {old_len} characters"),
            });
        }
        old_len
    } else {
        encoded.len()
    };
    let end = offset.checked_add(width).filter(|e| *e <= bytes.len()).ok_or_else(|| {
        CoreError::RawOffsetOutOfRange {
            path: region_name.to_string(),
            offset: offset as u64,
            width,
            len: bytes.len(),
        }
    })?;
    bytes[offset..end].copy_from_slice(&encoded);
    Ok(())
}

fn with_path(e: CoreError, path: &str) -> CoreError {
    match e {
        CoreError::ValueOutOfRange { value, target, .. } => CoreError::ValueOutOfRange {
            path: path.to_string(),
            value,
            target,
        },
        CoreError::TypeMismatch { expected, found, .. } => CoreError::TypeMismatch {
            path: path.to_string(),
            expected,
            found,
        },
        other => other,
    }
}

/// Text encoding of an indexed string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StringEncoding {
    Ascii,
    Utf16Le,
}

/// A printable string discovered inside a raw region.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawString {
    pub offset: u64,
    pub byte_len: u32,
    pub encoding: StringEncoding,
    pub text: String,
}

/// Unknown binary data with an index of recognisable values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawRegion {
    /// The bytes, verbatim.
    #[serde(with = "serde_hex_bytes")]
    pub bytes: Vec<u8>,
    /// Block size used for block hashing / diffing.
    pub block_size: u32,
    /// Index of printable strings (ASCII and UTF-16LE).
    pub strings: Vec<RawString>,
    /// Endianness the region most likely uses (heuristic).
    pub endian_hint: Endianness,
}

/// Default block size for raw diffing.
pub const DEFAULT_BLOCK_SIZE: u32 = 4096;
/// Minimum characters for an indexed string.
pub const MIN_STRING_LEN: usize = 4;

impl RawRegion {
    /// Builds a region and its index.
    pub fn new(bytes: Vec<u8>) -> Self {
        let strings = index_strings(&bytes, MIN_STRING_LEN);
        let endian_hint = guess_endianness(&bytes);
        RawRegion {
            bytes,
            block_size: DEFAULT_BLOCK_SIZE,
            strings,
            endian_hint,
        }
    }

    /// Re-indexes after the bytes changed.
    pub fn reindex(&mut self) {
        self.strings = index_strings(&self.bytes, MIN_STRING_LEN);
        self.endian_hint = guess_endianness(&self.bytes);
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// True if empty.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Iterates over `(offset, value)` for every position matching `ty`
    /// with the given alignment.
    pub fn primitives(
        &self,
        ty: ScanType,
        endian: Endianness,
        alignment: usize,
    ) -> impl Iterator<Item = (usize, ScalarValue)> + '_ {
        let step = alignment.max(1);
        (0..self.bytes.len())
            .step_by(step)
            .filter_map(move |off| read_primitive(&self.bytes, off, ty, endian).map(|v| (off, v)))
    }
}

/// Finds ASCII and UTF-16LE strings of at least `min_len` printable characters.
pub fn index_strings(bytes: &[u8], min_len: usize) -> Vec<RawString> {
    let mut out = Vec::new();
    let printable = |b: u8| (0x20..0x7f).contains(&b) || b == b'\t';
    // ASCII runs.
    let mut i = 0;
    while i < bytes.len() {
        if printable(bytes[i]) {
            let start = i;
            while i < bytes.len() && printable(bytes[i]) {
                i += 1;
            }
            if i - start >= min_len {
                out.push(RawString {
                    offset: start as u64,
                    byte_len: (i - start) as u32,
                    encoding: StringEncoding::Ascii,
                    text: String::from_utf8_lossy(&bytes[start..i]).into_owned(),
                });
            }
        } else {
            i += 1;
        }
    }
    // UTF-16LE runs (printable ASCII code units followed by 0x00).
    let mut j = 0;
    while j + 1 < bytes.len() {
        if printable(bytes[j]) && bytes[j + 1] == 0 {
            let start = j;
            let mut text = String::new();
            while j + 1 < bytes.len() && printable(bytes[j]) && bytes[j + 1] == 0 {
                text.push(bytes[j] as char);
                j += 2;
            }
            if text.len() >= min_len {
                out.push(RawString {
                    offset: start as u64,
                    byte_len: (j - start) as u32,
                    encoding: StringEncoding::Utf16Le,
                    text,
                });
            }
        } else {
            j += 1;
        }
    }
    out.sort_by_key(|s| s.offset);
    out
}

/// Heuristic: small integers stored little-endian have zero high bytes at the
/// end of each aligned word; big-endian at the start.
pub fn guess_endianness(bytes: &[u8]) -> Endianness {
    let mut le = 0usize;
    let mut be = 0usize;
    for w in bytes.chunks_exact(4) {
        let lo_zero = w[0] == 0 && w[1] == 0;
        let hi_zero = w[2] == 0 && w[3] == 0;
        if hi_zero && !lo_zero {
            le += 1;
        } else if lo_zero && !hi_zero {
            be += 1;
        }
    }
    if be > le * 2 && be > 4 {
        Endianness::Big
    } else {
        Endianness::Little
    }
}

/// Serde helper storing bytes as a hex string (compact in JSON).
pub mod serde_hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        hex::decode(s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitive_roundtrip_all_types() {
        let values = [
            (ScanType::I8, ScalarValue::Int(-5)),
            (ScanType::U8, ScalarValue::Int(250)),
            (ScanType::I16, ScalarValue::Int(-30000)),
            (ScanType::U16, ScalarValue::Int(60000)),
            (ScanType::I32, ScalarValue::Int(-2_000_000_000)),
            (ScanType::U32, ScalarValue::Int(4_000_000_000)),
            (ScanType::I64, ScalarValue::Int(-9_000_000_000_000)),
            (ScanType::U64, ScalarValue::Int(18_000_000_000_000_000_000)),
            (ScanType::F32, ScalarValue::Float(1500.5)),
            (ScanType::F64, ScalarValue::Float(-0.125)),
        ];
        for endian in [Endianness::Little, Endianness::Big] {
            for (ty, v) in values {
                let mut buf = vec![0xAAu8; 3 + ty.width().unwrap() + 2];
                write_primitive(&mut buf, 3, ty, endian, v, "r").unwrap();
                assert_eq!(read_primitive(&buf, 3, ty, endian), Some(v), "{ty} {endian:?}");
                assert_eq!(buf[0], 0xAA);
                assert_eq!(*buf.last().unwrap(), 0xAA);
            }
        }
    }

    #[test]
    fn out_of_range_rejected() {
        let mut buf = vec![0u8; 8];
        assert!(write_primitive(&mut buf, 0, ScanType::U8, Endianness::Little, ScalarValue::Int(300), "r").is_err());
        assert!(write_primitive(&mut buf, 6, ScanType::I32, Endianness::Little, ScalarValue::Int(1), "r").is_err());
        assert!(write_primitive(&mut buf, 0, ScanType::I32, Endianness::Little, ScalarValue::Float(0.5), "r").is_err());
        assert_eq!(read_primitive(&buf, 6, ScanType::I32, Endianness::Little), None);
    }

    #[test]
    fn ascii_decimal() {
        let mut data = b"gold=1500;hp=-20;x".to_vec();
        assert_eq!(read_ascii_decimal(&data, 5), Some((1500, 4)));
        assert_eq!(read_ascii_decimal(&data, 6), None, "inside a run");
        assert_eq!(read_ascii_decimal(&data, 13), Some((-20, 3)));
        write_primitive(&mut data, 5, ScanType::AsciiDecimal, Endianness::Little, ScalarValue::Int(9999), "r").unwrap();
        assert_eq!(&data, b"gold=9999;hp=-20;x");
        assert!(write_primitive(&mut data, 5, ScanType::AsciiDecimal, Endianness::Little, ScalarValue::Int(10000), "r").is_err());
    }

    #[test]
    fn string_index_finds_ascii_and_utf16() {
        let mut data = vec![0u8, 1, 2];
        data.extend_from_slice(b"Hello World");
        data.extend_from_slice(&[0, 0xff]);
        for c in "Sword".encode_utf16() {
            data.extend_from_slice(&c.to_le_bytes());
        }
        let idx = index_strings(&data, 4);
        assert!(idx.iter().any(|s| s.text == "Hello World" && s.encoding == StringEncoding::Ascii && s.offset == 3));
        assert!(idx.iter().any(|s| s.text == "Sword" && s.encoding == StringEncoding::Utf16Le));
    }

    #[test]
    fn endianness_guess() {
        let mut le = Vec::new();
        let mut be = Vec::new();
        for i in 1..100u32 {
            le.extend_from_slice(&i.to_le_bytes());
            be.extend_from_slice(&i.to_be_bytes());
        }
        assert_eq!(guess_endianness(&le), Endianness::Little);
        assert_eq!(guess_endianness(&be), Endianness::Big);
    }

    #[test]
    fn region_primitives_iter() {
        let mut bytes = vec![0u8; 16];
        bytes[4..8].copy_from_slice(&1500i32.to_le_bytes());
        let r = RawRegion::new(bytes);
        let hits: Vec<_> = r
            .primitives(ScanType::I32, Endianness::Little, 4)
            .filter(|(_, v)| *v == ScalarValue::Int(1500))
            .collect();
        assert_eq!(hits, vec![(4, ScalarValue::Int(1500))]);
    }

    #[test]
    fn scan_type_parse() {
        assert_eq!("float".parse::<ScanType>().unwrap(), ScanType::F32);
        assert_eq!("U64".parse::<ScanType>().unwrap(), ScanType::U64);
        assert!("x".parse::<ScanType>().is_err());
    }
}
