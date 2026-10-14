//! Python pickle codec (protocols 0–5), as used by Ren'Py save logs.
//!
//! **Never executes code.** The parser reads the opcode stream verbatim and
//! runs a *symbolic* pickle VM: `GLOBAL` / `STACK_GLOBAL` push class
//! descriptors (module + name) instead of importing anything, `REDUCE`,
//! `NEWOBJ(_EX)`, `INST` and `OBJ` build virtual instances that remember
//! their callable, arguments and `BUILD` state, and `SETITEM(S)`,
//! `APPEND(S)` and `ADDITEMS` only record items.
//!
//! **Losslessness.** Every opcode keeps its exact span of the input; the
//! writer re-emits each span and only replaces the opcodes whose argument was
//! edited. `FRAME` lengths (protocol 4+) are always recomputed from the
//! emitted opcode sizes, which reproduces the original lengths when nothing
//! changed. Bytes after `STOP` are kept verbatim.
//!
//! **Projection.** dicts become maps (non-string keys rendered as their
//! Python repr, duplicate keys suffixed `#2`, `#3`, …), lists / tuples / sets
//! arrays, ints `I64` (larger ints decimal strings), floats `F64`, `str`
//! `String`, `bytes` `Bytes`, `None` `Null`, bools `Bool`. Class instances
//! become maps with a read-only `"$class"` (`module.Name`), their
//! constructor arguments (`"$args"`, `"$kwargs"`), dict items, list items
//! (`"$items"`) and their `BUILD` state (dict state merged as keys, other
//! state under `"$state"`). A memo reference to a container that was already
//! projected becomes the read-only string `"$ref:<memo id>"`.
//!
//! **Edits.** Every int, float, bool, str and bytes leaf is bound to the
//! opcode that created it. An edit rewrites only that opcode: ints stay in
//! their family (`BININT1` → `BININT2` → `BININT` → `LONG1` → `LONG4`, or the
//! protocol-0 `INT` / `LONG` text forms), upgrading to the smallest opcode
//! that fits; floats keep `BINFLOAT` / `FLOAT`; strings pick
//! `SHORT_BINUNICODE` / `BINUNICODE` / `BINUNICODE8` by length (text forms
//! are re-rendered as text). Memoized scalars referenced from several places
//! referenced again through `GET` / `DUP` are projected by value at every
//! place; editing one occurrence rewrites only that occurrence (an edited
//! `GET` becomes a literal opcode, and when the defining opcode is edited the
//! other references are turned into literals of the old value), so other
//! variables and dict keys that shared the object keep their value. After an
//! edit the rewritten stream is parsed again, so the document always mirrors
//! the bytes it will write. Structural edits and read-only nodes are rejected.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use savelens_core::{Node, NodeMap, NodePath};

use crate::document::{
    apply_via_diff, Capabilities, Confidence, DetectContext, Document, PathEditable, StructuralCodec,
};
use crate::error::{CodecError, CodecResult};
use crate::io::ByteReader;

const CODEC: &str = "pickle";
/// Projection depth limit (Python's own pickler recursion limit is ~1000).
const MAX_DEPTH: usize = 400;
/// LONG1/LONG4 payloads longer than this are shown as read-only labels.
const MAX_BIGINT_BYTES: usize = 4096;

/// Pickle opcodes (protocols 0–5), named as in CPython's `pickle.py`.
pub mod op {
    pub const MARK: u8 = b'(';
    pub const STOP: u8 = b'.';
    pub const POP: u8 = b'0';
    pub const POP_MARK: u8 = b'1';
    pub const DUP: u8 = b'2';
    pub const FLOAT: u8 = b'F';
    pub const INT: u8 = b'I';
    pub const BININT: u8 = b'J';
    pub const BININT1: u8 = b'K';
    pub const LONG: u8 = b'L';
    pub const BININT2: u8 = b'M';
    pub const NONE: u8 = b'N';
    pub const PERSID: u8 = b'P';
    pub const BINPERSID: u8 = b'Q';
    pub const REDUCE: u8 = b'R';
    pub const STRING: u8 = b'S';
    pub const BINSTRING: u8 = b'T';
    pub const SHORT_BINSTRING: u8 = b'U';
    pub const UNICODE: u8 = b'V';
    pub const BINUNICODE: u8 = b'X';
    pub const APPEND: u8 = b'a';
    pub const BUILD: u8 = b'b';
    pub const GLOBAL: u8 = b'c';
    pub const DICT: u8 = b'd';
    pub const EMPTY_DICT: u8 = b'}';
    pub const APPENDS: u8 = b'e';
    pub const GET: u8 = b'g';
    pub const BINGET: u8 = b'h';
    pub const INST: u8 = b'i';
    pub const LONG_BINGET: u8 = b'j';
    pub const LIST: u8 = b'l';
    pub const EMPTY_LIST: u8 = b']';
    pub const OBJ: u8 = b'o';
    pub const PUT: u8 = b'p';
    pub const BINPUT: u8 = b'q';
    pub const LONG_BINPUT: u8 = b'r';
    pub const SETITEM: u8 = b's';
    pub const TUPLE: u8 = b't';
    pub const EMPTY_TUPLE: u8 = b')';
    pub const SETITEMS: u8 = b'u';
    pub const BINFLOAT: u8 = b'G';
    // Protocol 2.
    pub const PROTO: u8 = 0x80;
    pub const NEWOBJ: u8 = 0x81;
    pub const EXT1: u8 = 0x82;
    pub const EXT2: u8 = 0x83;
    pub const EXT4: u8 = 0x84;
    pub const TUPLE1: u8 = 0x85;
    pub const TUPLE2: u8 = 0x86;
    pub const TUPLE3: u8 = 0x87;
    pub const NEWTRUE: u8 = 0x88;
    pub const NEWFALSE: u8 = 0x89;
    pub const LONG1: u8 = 0x8a;
    pub const LONG4: u8 = 0x8b;
    // Protocol 3.
    pub const BINBYTES: u8 = b'B';
    pub const SHORT_BINBYTES: u8 = b'C';
    // Protocol 4.
    pub const SHORT_BINUNICODE: u8 = 0x8c;
    pub const BINUNICODE8: u8 = 0x8d;
    pub const BINBYTES8: u8 = 0x8e;
    pub const EMPTY_SET: u8 = 0x8f;
    pub const ADDITEMS: u8 = 0x90;
    pub const FROZENSET: u8 = 0x91;
    pub const NEWOBJ_EX: u8 = 0x92;
    pub const STACK_GLOBAL: u8 = 0x93;
    pub const MEMOIZE: u8 = 0x94;
    pub const FRAME: u8 = 0x95;
    // Protocol 5.
    pub const BYTEARRAY8: u8 = 0x96;
    pub const NEXT_BUFFER: u8 = 0x97;
    pub const READONLY_BUFFER: u8 = 0x98;
}

// ---------------------------------------------------------------------------
// Integers
// ---------------------------------------------------------------------------

/// A Python int.
#[derive(Debug, Clone, PartialEq)]
enum IntVal {
    /// Fits an `i64`.
    Small(i64),
    /// Normalised decimal text of a larger int.
    Big(String),
    /// A `LONG1`/`LONG4` payload too long to convert (byte length).
    Huge(usize),
}

impl IntVal {
    fn to_node(&self) -> Node {
        match self {
            IntVal::Small(v) => Node::I64(*v),
            IntVal::Big(s) => Node::String(s.clone()),
            IntVal::Huge(n) => Node::String(format!("$int:{n}-byte integer")),
        }
    }

    fn decimal(&self) -> String {
        match self {
            IntVal::Small(v) => v.to_string(),
            IntVal::Big(s) => s.clone(),
            IntVal::Huge(n) => format!("$int:{n}-byte integer"),
        }
    }

    /// Minimal two's complement little-endian bytes (Python `encode_long`).
    fn twos_le(&self) -> Option<Vec<u8>> {
        match self {
            IntVal::Small(v) => Some(trim_twos(v.to_le_bytes().to_vec())),
            IntVal::Big(s) => decimal_to_twos_le(s),
            IntVal::Huge(_) => None,
        }
    }

    /// Parses decimal text (optional sign, digits, surrounding whitespace).
    fn parse_decimal(text: &str) -> Option<IntVal> {
        let t = text.trim();
        let (neg, digits) = match t.as_bytes().first() {
            Some(b'-') => (true, &t[1..]),
            Some(b'+') => (false, &t[1..]),
            _ => (false, t),
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let stripped = digits.trim_start_matches('0');
        let stripped = if stripped.is_empty() { "0" } else { stripped };
        let norm = if neg && stripped != "0" { format!("-{stripped}") } else { stripped.to_string() };
        Some(match norm.parse::<i64>() {
            Ok(v) => IntVal::Small(v),
            Err(_) => IntVal::Big(norm),
        })
    }

    fn from_twos_le(bytes: &[u8]) -> IntVal {
        if bytes.len() > MAX_BIGINT_BYTES {
            return IntVal::Huge(bytes.len());
        }
        if bytes.len() <= 8 {
            let fill = if bytes.last().is_some_and(|b| b & 0x80 != 0) { 0xff } else { 0 };
            let mut arr = [fill; 8];
            arr[..bytes.len()].copy_from_slice(bytes);
            return IntVal::Small(i64::from_le_bytes(arr));
        }
        let dec = twos_le_to_decimal(bytes);
        match dec.parse::<i64>() {
            Ok(v) => IntVal::Small(v),
            Err(_) => IntVal::Big(dec),
        }
    }

    fn as_i64(&self) -> Option<i64> {
        match self {
            IntVal::Small(v) => Some(*v),
            _ => None,
        }
    }
}

/// Removes redundant sign-extension bytes; zero becomes empty.
fn trim_twos(mut out: Vec<u8>) -> Vec<u8> {
    while out.len() > 1 {
        let last = out[out.len() - 1];
        let prev = out[out.len() - 2];
        if (last == 0 && prev & 0x80 == 0) || (last == 0xff && prev & 0x80 != 0) {
            out.pop();
        } else {
            break;
        }
    }
    if out == [0] {
        out.clear();
    }
    out
}

/// Two's complement little-endian bytes to decimal text.
fn twos_le_to_decimal(bytes: &[u8]) -> String {
    let Some(&last) = bytes.last() else { return "0".into() };
    let neg = last & 0x80 != 0;
    let mut mag = bytes.to_vec();
    if neg {
        for b in mag.iter_mut() {
            *b = !*b;
        }
        for b in mag.iter_mut() {
            let (v, overflow) = b.overflowing_add(1);
            *b = v;
            if !overflow {
                break;
            }
        }
    }
    let mut be: Vec<u8> = mag.into_iter().rev().skip_while(|&b| b == 0).collect();
    let mut chunks: Vec<u32> = Vec::new();
    while !be.is_empty() {
        let mut rem: u64 = 0;
        let mut quotient = Vec::with_capacity(be.len());
        for &b in &be {
            let cur = (rem << 8) | u64::from(b);
            let q = cur / 1_000_000_000;
            rem = cur % 1_000_000_000;
            if !(quotient.is_empty() && q == 0) {
                // q < 256 because rem < 1e9 before the shift.
                quotient.push(q as u8);
            }
        }
        chunks.push(rem as u32);
        be = quotient;
    }
    let mut s = String::new();
    if neg {
        s.push('-');
    }
    match chunks.split_last() {
        None => return "0".into(),
        Some((top, rest)) => {
            s.push_str(&top.to_string());
            for c in rest.iter().rev() {
                s.push_str(&format!("{c:09}"));
            }
        }
    }
    s
}

/// Decimal text to minimal two's complement little-endian bytes.
fn decimal_to_twos_le(text: &str) -> Option<Vec<u8>> {
    let (neg, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut mag: Vec<u8> = Vec::new();
    for d in digits.bytes() {
        let mut carry = u32::from(d - b'0');
        for b in mag.iter_mut() {
            let v = u32::from(*b) * 10 + carry;
            *b = v as u8;
            carry = v >> 8;
        }
        while carry > 0 {
            mag.push(carry as u8);
            carry >>= 8;
        }
    }
    while mag.last() == Some(&0) {
        mag.pop();
    }
    if mag.is_empty() {
        return Some(Vec::new());
    }
    mag.push(0);
    if neg {
        for b in mag.iter_mut() {
            *b = !*b;
        }
        for b in mag.iter_mut() {
            let (v, overflow) = b.overflowing_add(1);
            *b = v;
            if !overflow {
                break;
            }
        }
    }
    Some(trim_twos(mag))
}

// ---------------------------------------------------------------------------
// Text forms
// ---------------------------------------------------------------------------

/// Python `repr(float)`.
fn py_float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let sci = format!("{x:e}");
    let (mant, exp) = match sci.split_once('e') {
        Some((m, e)) => (m, e.parse::<i32>().unwrap_or(0)),
        None => (sci.as_str(), 0),
    };
    let (sign, mant) = match mant.strip_prefix('-') {
        Some(m) => ("-", m),
        None => ("", mant),
    };
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    if (-4..16).contains(&exp) {
        let body = if exp >= 0 {
            let point = exp as usize + 1;
            let (int_part, frac) = if digits.len() > point {
                (digits[..point].to_string(), digits[point..].to_string())
            } else {
                (format!("{digits}{}", "0".repeat(point - digits.len())), "0".to_string())
            };
            format!("{int_part}.{frac}")
        } else {
            format!("0.{}{digits}", "0".repeat((-exp - 1) as usize))
        };
        format!("{sign}{body}")
    } else {
        let m = if digits.len() > 1 { format!("{}.{}", &digits[..1], &digits[1..]) } else { digits.clone() };
        let esign = if exp < 0 { '-' } else { '+' };
        format!("{sign}{m}e{esign}{:02}", exp.abs())
    }
}

/// Python 2 `repr(str)` used by the `STRING` opcode.
fn py2_str_repr(b: &[u8]) -> Vec<u8> {
    let mut out = vec![b'\''];
    for &c in b {
        match c {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\'' => out.extend_from_slice(b"\\'"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x20..=0x7e => out.push(c),
            _ => out.extend_from_slice(format!("\\x{c:02x}").as_bytes()),
        }
    }
    out.push(b'\'');
    out
}

/// `codecs.escape_decode` (the `STRING` opcode argument without quotes).
fn escape_decode(b: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        i += 1;
        if c != b'\\' {
            out.push(c);
            continue;
        }
        let Some(&e) = b.get(i) else { return Err("trailing backslash in STRING argument".into()) };
        i += 1;
        match e {
            b'\n' => {}
            b'\\' => out.push(b'\\'),
            b'\'' => out.push(b'\''),
            b'"' => out.push(b'"'),
            b'a' => out.push(7),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(11),
            b'0'..=b'7' => {
                let mut v = u32::from(e - b'0');
                for _ in 0..2 {
                    match b.get(i) {
                        Some(&d @ b'0'..=b'7') => {
                            v = v * 8 + u32::from(d - b'0');
                            i += 1;
                        }
                        _ => break,
                    }
                }
                out.push((v & 0xff) as u8);
            }
            b'x' => {
                let hex = b.get(i..i + 2).ok_or("truncated \\x escape")?;
                let s = std::str::from_utf8(hex).map_err(|_| "invalid \\x escape")?;
                out.push(u8::from_str_radix(s, 16).map_err(|_| "invalid \\x escape")?);
                i += 2;
            }
            other => {
                out.push(b'\\');
                out.push(other);
            }
        }
    }
    Ok(out)
}

/// `raw-unicode-escape` decoding (the `UNICODE` opcode argument).
fn raw_unicode_unescape(b: &[u8]) -> Result<String, String> {
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            out.push(char::from(b[i]));
            i += 1;
            continue;
        }
        let mut j = i;
        while j < b.len() && b[j] == b'\\' {
            j += 1;
        }
        let run = j - i;
        let escape = run % 2 == 1 && matches!(b.get(j), Some(b'u') | Some(b'U'));
        if !escape {
            out.extend(std::iter::repeat_n('\\', run));
            i = j;
            continue;
        }
        out.extend(std::iter::repeat_n('\\', run - 1));
        let n = if b[j] == b'u' { 4 } else { 8 };
        let hex = b.get(j + 1..j + 1 + n).ok_or("truncated \\uXXXX escape")?;
        let s = std::str::from_utf8(hex).map_err(|_| "invalid \\u escape")?;
        let v = u32::from_str_radix(s, 16).map_err(|_| "invalid \\u escape")?;
        out.push(char::from_u32(v).ok_or_else(|| format!("code point {v:#x} is not a Unicode scalar value"))?);
        i = j + 1 + n;
    }
    Ok(out)
}

/// `raw-unicode-escape` encoding as done by the protocol-0 pickler.
fn raw_unicode_escape(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for ch in s.chars() {
        let c = ch as u32;
        match ch {
            '\\' | '\0' | '\n' | '\r' | '\u{1a}' => out.extend_from_slice(format!("\\u{c:04x}").as_bytes()),
            _ if c < 0x100 => out.push(c as u8),
            _ if c < 0x10000 => out.extend_from_slice(format!("\\u{c:04x}").as_bytes()),
            _ => out.extend_from_slice(format!("\\U{c:08x}").as_bytes()),
        }
    }
    out
}

fn python_repr_bytes(b: &[u8]) -> String {
    let mut s = String::from("b");
    s.push_str(&String::from_utf8_lossy(&py2_str_repr(b)));
    s
}

// ---------------------------------------------------------------------------
// Opcode reader
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Arg {
    None,
    Proto(u8),
    Frame(u64),
    Int(IntVal),
    Bool(bool),
    Float(f64),
    Str(String),
    PyStr(Vec<u8>),
    Bytes(Vec<u8>),
    ByteArray(Vec<u8>),
    Memo(u64),
    Global(String, String),
    PersId(String),
    Ext(u32),
}

fn read_line<'a>(r: &mut ByteReader<'a>, ctx: &str) -> CodecResult<&'a [u8]> {
    let rest = r.rest();
    let n = rest
        .iter()
        .position(|&b| b == b'\n')
        .ok_or_else(|| r.malformed(format!("{ctx}: missing newline terminator")))?;
    let line = r.take(n + 1, ctx)?;
    Ok(&line[..n])
}

fn line_str<'a>(r: &ByteReader<'_>, line: &'a [u8], ctx: &str) -> CodecResult<&'a str> {
    std::str::from_utf8(line).map_err(|_| r.malformed(format!("{ctx}: argument is not UTF-8")))
}

fn len_prefixed<'a>(r: &mut ByteReader<'a>, width: usize, ctx: &str) -> CodecResult<&'a [u8]> {
    let n = match width {
        1 => usize::from(r.u8(ctx)?),
        4 => r.u32_le(ctx)? as usize,
        _ => usize::try_from(r.u64_le(ctx)?).map_err(|_| r.malformed(format!("{ctx}: length overflows")))?,
    };
    r.take(n, ctx)
}

fn utf8(r: &ByteReader<'_>, b: &[u8], ctx: &str) -> CodecResult<String> {
    String::from_utf8(b.to_vec()).map_err(|_| r.malformed(format!("{ctx}: string is not valid UTF-8")))
}

/// Reads one opcode and its argument.
fn read_op(r: &mut ByteReader<'_>) -> CodecResult<(u8, Arg)> {
    use op::*;
    let code = r.u8("opcode")?;
    let arg = match code {
        MARK | STOP | POP | POP_MARK | DUP | NONE | BINPERSID | REDUCE | APPEND | BUILD | DICT | EMPTY_DICT
        | APPENDS | LIST | EMPTY_LIST | OBJ | SETITEM | TUPLE | EMPTY_TUPLE | SETITEMS | NEWOBJ | TUPLE1 | TUPLE2
        | TUPLE3 | EMPTY_SET | ADDITEMS | FROZENSET | NEWOBJ_EX | STACK_GLOBAL | MEMOIZE | NEXT_BUFFER
        | READONLY_BUFFER => Arg::None,
        NEWTRUE => Arg::Bool(true),
        NEWFALSE => Arg::Bool(false),
        PROTO => Arg::Proto(r.u8("PROTO")?),
        FRAME => Arg::Frame(r.u64_le("FRAME")?),
        FLOAT => {
            let line = read_line(r, "FLOAT")?;
            let s = line_str(r, line, "FLOAT")?;
            Arg::Float(s.trim().parse::<f64>().map_err(|_| r.malformed(format!("FLOAT: invalid value `{s}`")))?)
        }
        INT => {
            let line = read_line(r, "INT")?;
            match line {
                b"00" => Arg::Bool(false),
                b"01" => Arg::Bool(true),
                _ => {
                    let s = line_str(r, line, "INT")?;
                    Arg::Int(IntVal::parse_decimal(s).ok_or_else(|| r.malformed(format!("INT: invalid value `{s}`")))?)
                }
            }
        }
        LONG => {
            let line = read_line(r, "LONG")?;
            let s = line_str(r, line, "LONG")?;
            let digits = s.strip_suffix('L').unwrap_or(s);
            Arg::Int(IntVal::parse_decimal(digits).ok_or_else(|| r.malformed(format!("LONG: invalid value `{s}`")))?)
        }
        BININT => Arg::Int(IntVal::Small(i64::from(r.i32_le("BININT")?))),
        BININT1 => Arg::Int(IntVal::Small(i64::from(r.u8("BININT1")?))),
        BININT2 => Arg::Int(IntVal::Small(i64::from(r.u16_le("BININT2")?))),
        LONG1 => Arg::Int(IntVal::from_twos_le(len_prefixed(r, 1, "LONG1")?)),
        LONG4 => {
            let n = r.i32_le("LONG4")?;
            let n = usize::try_from(n).map_err(|_| r.malformed("LONG4: negative byte count"))?;
            Arg::Int(IntVal::from_twos_le(r.take(n, "LONG4")?))
        }
        BINFLOAT => Arg::Float(r.f64_be("BINFLOAT")?),
        STRING => {
            let line = read_line(r, "STRING")?;
            let quoted = line.len() >= 2 && line[0] == line[line.len() - 1] && (line[0] == b'\'' || line[0] == b'"');
            if !quoted {
                return Err(r.malformed("STRING: argument must be quoted"));
            }
            Arg::PyStr(escape_decode(&line[1..line.len() - 1]).map_err(|e| r.malformed(format!("STRING: {e}")))?)
        }
        BINSTRING => {
            let n = r.i32_le("BINSTRING")?;
            let n = usize::try_from(n).map_err(|_| r.malformed("BINSTRING: negative length"))?;
            Arg::PyStr(r.take(n, "BINSTRING")?.to_vec())
        }
        SHORT_BINSTRING => Arg::PyStr(len_prefixed(r, 1, "SHORT_BINSTRING")?.to_vec()),
        UNICODE => {
            let line = read_line(r, "UNICODE")?;
            Arg::Str(raw_unicode_unescape(line).map_err(|e| r.malformed(format!("UNICODE: {e}")))?)
        }
        BINUNICODE => {
            let b = len_prefixed(r, 4, "BINUNICODE")?;
            Arg::Str(utf8(r, b, "BINUNICODE")?)
        }
        SHORT_BINUNICODE => {
            let b = len_prefixed(r, 1, "SHORT_BINUNICODE")?;
            Arg::Str(utf8(r, b, "SHORT_BINUNICODE")?)
        }
        BINUNICODE8 => {
            let b = len_prefixed(r, 8, "BINUNICODE8")?;
            Arg::Str(utf8(r, b, "BINUNICODE8")?)
        }
        BINBYTES => Arg::Bytes(len_prefixed(r, 4, "BINBYTES")?.to_vec()),
        SHORT_BINBYTES => Arg::Bytes(len_prefixed(r, 1, "SHORT_BINBYTES")?.to_vec()),
        BINBYTES8 => Arg::Bytes(len_prefixed(r, 8, "BINBYTES8")?.to_vec()),
        BYTEARRAY8 => Arg::ByteArray(len_prefixed(r, 8, "BYTEARRAY8")?.to_vec()),
        GLOBAL | INST => {
            let ctx = if code == GLOBAL { "GLOBAL" } else { "INST" };
            let module = read_line(r, ctx)?;
            let module = line_str(r, module, ctx)?.to_string();
            let name = read_line(r, ctx)?;
            let name = line_str(r, name, ctx)?.to_string();
            Arg::Global(module, name)
        }
        PERSID => {
            let line = read_line(r, "PERSID")?;
            Arg::PersId(line_str(r, line, "PERSID")?.to_string())
        }
        GET | PUT => {
            let ctx = if code == GET { "GET" } else { "PUT" };
            let line = read_line(r, ctx)?;
            let s = line_str(r, line, ctx)?;
            Arg::Memo(s.trim().parse::<u64>().map_err(|_| r.malformed(format!("{ctx}: invalid memo id `{s}`")))?)
        }
        BINGET | BINPUT => Arg::Memo(u64::from(r.u8("BINGET/BINPUT")?)),
        LONG_BINGET | LONG_BINPUT => Arg::Memo(u64::from(r.u32_le("LONG_BINGET/LONG_BINPUT")?)),
        EXT1 => Arg::Ext(u32::from(r.u8("EXT1")?)),
        EXT2 => Arg::Ext(u32::from(r.u16_le("EXT2")?)),
        EXT4 => Arg::Ext(r.u32_le("EXT4")?),
        other => {
            return Err(CodecError::malformed(CODEC, r.pos() - 1, format!("unknown opcode {other:#04x}")));
        }
    };
    Ok((code, arg))
}

/// Opcode-level scan used by detection: returns `(protocol, end of STOP)`.
fn scan(data: &[u8]) -> CodecResult<(u8, usize)> {
    let mut r = ByteReader::new(data, CODEC);
    let mut protocol = 0u8;
    let mut frame_end: Option<usize> = None;
    loop {
        let start = r.pos();
        if let Some(end) = frame_end {
            if start == end {
                frame_end = None;
            }
        }
        let (code, arg) = read_op(&mut r)?;
        if let Some(end) = frame_end {
            if r.pos() > end {
                return Err(CodecError::malformed(CODEC, start, "opcode crosses a FRAME boundary"));
            }
        }
        match arg {
            Arg::Proto(p) => protocol = p,
            Arg::Frame(len) => {
                if frame_end.is_some() {
                    return Err(CodecError::malformed(CODEC, start, "new FRAME before the end of the current one"));
                }
                frame_end = Some(frame_limit(&r, len)?);
            }
            _ => {}
        }
        if code == op::STOP {
            return Ok((protocol, r.pos()));
        }
    }
}

fn frame_limit(r: &ByteReader<'_>, len: u64) -> CodecResult<usize> {
    usize::try_from(len)
        .ok()
        .and_then(|l| r.pos().checked_add(l))
        .filter(|&e| e <= r.data().len())
        .ok_or_else(|| r.malformed(format!("FRAME length {len} exceeds the input")))
}

// ---------------------------------------------------------------------------
// Symbolic VM
// ---------------------------------------------------------------------------

type ObjId = usize;

#[derive(Debug, Clone, PartialEq)]
enum Scalar {
    None,
    Bool(bool),
    Int(IntVal),
    Float(f64),
    Str(String),
    PyStr(Vec<u8>),
    Bytes(Vec<u8>),
    ByteArray(Vec<u8>),
}

#[derive(Debug, Clone)]
enum Kind {
    Scalar(Scalar),
    Dict,
    List,
    Tuple,
    Set,
    FrozenSet,
    Global { module: String, name: String },
    Instance { callable: ObjId, args: Option<ObjId>, kwargs: Option<ObjId> },
    PersText(String),
    PersObj(ObjId),
    Ext(u32),
    Buffer(usize),
}

#[derive(Debug, Clone)]
struct Obj {
    kind: Kind,
    /// Index of the opcode that created the object.
    op: usize,
    items: Vec<ObjId>,
    pairs: Vec<(ObjId, ObjId)>,
    state: Vec<ObjId>,
    memo: Option<u64>,
    /// For a scalar pushed again by `GET` / `DUP`: the object that defined it.
    alias_of: Option<ObjId>,
}

#[derive(Debug, Clone, Copy)]
struct OpSpan {
    start: usize,
    code: u8,
}

#[derive(Debug, Clone, Copy)]
struct Frame {
    /// Index of the FRAME opcode.
    op: usize,
    /// One past the last opcode inside the frame.
    end: usize,
}

#[derive(Debug, Clone, Copy)]
enum Slot {
    Mark,
    Obj(ObjId),
}

struct Vm {
    stack: Vec<Slot>,
    memo: HashMap<u64, ObjId>,
    objs: Vec<Obj>,
    aliases: HashMap<ObjId, Vec<ObjId>>,
    buffers: usize,
    at: usize,
}

impl Vm {
    fn err(&self, reason: impl Into<String>) -> CodecError {
        CodecError::malformed(CODEC, self.at, reason)
    }

    fn new_obj(&mut self, kind: Kind, op: usize) -> ObjId {
        self.objs.push(Obj {
            kind,
            op,
            items: Vec::new(),
            pairs: Vec::new(),
            state: Vec::new(),
            memo: None,
            alias_of: None,
        });
        self.objs.len() - 1
    }

    fn push_new(&mut self, kind: Kind, op: usize) {
        let id = self.new_obj(kind, op);
        self.stack.push(Slot::Obj(id));
    }

    fn pop(&mut self) -> CodecResult<ObjId> {
        match self.stack.pop() {
            Some(Slot::Obj(id)) => Ok(id),
            Some(Slot::Mark) => Err(self.err("unexpected MARK on the stack")),
            None => Err(self.err("stack underflow")),
        }
    }

    fn top(&self) -> CodecResult<ObjId> {
        match self.stack.last() {
            Some(Slot::Obj(id)) => Ok(*id),
            Some(Slot::Mark) => Err(self.err("unexpected MARK on the stack")),
            None => Err(self.err("stack underflow")),
        }
    }

    fn pop_mark(&mut self) -> CodecResult<Vec<ObjId>> {
        let pos =
            self.stack.iter().rposition(|s| matches!(s, Slot::Mark)).ok_or_else(|| self.err("no MARK on the stack"))?;
        let items = self.stack.split_off(pos + 1);
        self.stack.pop();
        Ok(items.into_iter().filter_map(|s| if let Slot::Obj(id) = s { Some(id) } else { None }).collect())
    }

    fn pairs(&mut self, items: Vec<ObjId>) -> CodecResult<Vec<(ObjId, ObjId)>> {
        if !items.len().is_multiple_of(2) {
            return Err(self.err("odd number of items for a dict"));
        }
        Ok(items.chunks(2).map(|c| (c[0], c[1])).collect())
    }

    fn kind(&self, id: ObjId) -> &Kind {
        &self.objs[id].kind
    }

    fn add_pairs(&mut self, target: ObjId, pairs: Vec<(ObjId, ObjId)>) -> CodecResult<()> {
        match self.kind(target) {
            Kind::Dict | Kind::Instance { .. } => {
                self.objs[target].pairs.extend(pairs);
                Ok(())
            }
            _ => Err(self.err("SETITEM(S) target is not a dict")),
        }
    }

    fn add_items(&mut self, target: ObjId, items: Vec<ObjId>, set: bool) -> CodecResult<()> {
        let ok = match self.kind(target) {
            Kind::List => !set,
            Kind::Set => set,
            Kind::Instance { .. } => true,
            _ => false,
        };
        if !ok {
            return Err(self.err(if set { "ADDITEMS target is not a set" } else { "APPEND(S) target is not a list" }));
        }
        self.objs[target].items.extend(items);
        Ok(())
    }

    fn memoize(&mut self, id: u64) -> CodecResult<()> {
        let top = self.top()?;
        self.memo.insert(id, top);
        if self.objs[top].memo.is_none() {
            self.objs[top].memo = Some(id);
        }
        Ok(())
    }

    /// Pushes a reference produced by opcode `idx` (`GET` / `DUP`). Scalars
    /// get an alias object of their own so each occurrence is editable alone;
    /// containers are shared.
    fn reference(&mut self, id: ObjId, idx: usize) {
        let pushed = match &self.objs[id].kind {
            Kind::Scalar(s) => {
                let origin = self.objs[id].alias_of.unwrap_or(id);
                let alias = self.new_obj(Kind::Scalar(s.clone()), idx);
                self.objs[alias].alias_of = Some(origin);
                self.aliases.entry(origin).or_default().push(alias);
                alias
            }
            _ => id,
        };
        self.stack.push(Slot::Obj(pushed));
    }

    fn global_text(&self, id: ObjId) -> CodecResult<String> {
        match self.kind(id) {
            Kind::Scalar(Scalar::Str(s)) => Ok(s.clone()),
            Kind::Scalar(Scalar::PyStr(b)) => Ok(String::from_utf8_lossy(b).into_owned()),
            _ => Err(self.err("STACK_GLOBAL operands must be strings")),
        }
    }

    /// Executes one opcode symbolically.
    fn step(&mut self, idx: usize, code: u8, arg: Arg) -> CodecResult<()> {
        let scalar = match arg {
            Arg::Int(v) => Some(Kind::Scalar(Scalar::Int(v))),
            Arg::Bool(b) => Some(Kind::Scalar(Scalar::Bool(b))),
            Arg::Float(f) => Some(Kind::Scalar(Scalar::Float(f))),
            Arg::Str(s) => Some(Kind::Scalar(Scalar::Str(s))),
            Arg::PyStr(b) => Some(Kind::Scalar(Scalar::PyStr(b))),
            Arg::Bytes(b) => Some(Kind::Scalar(Scalar::Bytes(b))),
            Arg::ByteArray(b) => Some(Kind::Scalar(Scalar::ByteArray(b))),
            Arg::Ext(c) => Some(Kind::Ext(c)),
            Arg::PersId(s) => Some(Kind::PersText(s)),
            other => {
                self.step_other(idx, code, other)?;
                None
            }
        };
        if let Some(kind) = scalar {
            self.push_new(kind, idx);
        }
        Ok(())
    }

    fn step_other(&mut self, idx: usize, code: u8, arg: Arg) -> CodecResult<()> {
        use op::*;
        match arg {
            Arg::Global(module, name) => {
                let g = self.new_obj(Kind::Global { module, name }, idx);
                if code == INST {
                    let items = self.pop_mark()?;
                    let args = self.new_obj(Kind::Tuple, idx);
                    self.objs[args].items = items;
                    self.push_new(Kind::Instance { callable: g, args: Some(args), kwargs: None }, idx);
                } else {
                    self.stack.push(Slot::Obj(g));
                }
                return Ok(());
            }
            Arg::Memo(m) => {
                return match code {
                    GET | BINGET | LONG_BINGET => {
                        let id = *self.memo.get(&m).ok_or_else(|| self.err(format!("memo id {m} is not defined")))?;
                        self.reference(id, idx);
                        Ok(())
                    }
                    _ => self.memoize(m),
                }
            }
            _ => {}
        }
        match code {
            MARK => self.stack.push(Slot::Mark),
            STOP | PROTO | FRAME => {}
            READONLY_BUFFER => {
                self.top()?;
            }
            POP => match self.stack.pop() {
                Some(_) => {}
                None => return Err(self.err("POP on an empty stack")),
            },
            POP_MARK => {
                self.pop_mark()?;
            }
            DUP => {
                let t = self.top()?;
                self.reference(t, idx);
            }
            NONE => self.push_new(Kind::Scalar(Scalar::None), idx),
            BINPERSID => {
                let pid = self.pop()?;
                self.push_new(Kind::PersObj(pid), idx);
            }
            REDUCE => {
                let args = self.pop()?;
                let callable = self.pop()?;
                self.push_new(Kind::Instance { callable, args: Some(args), kwargs: None }, idx);
            }
            NEWOBJ => {
                let args = self.pop()?;
                let cls = self.pop()?;
                self.push_new(Kind::Instance { callable: cls, args: Some(args), kwargs: None }, idx);
            }
            NEWOBJ_EX => {
                let kwargs = self.pop()?;
                let args = self.pop()?;
                let cls = self.pop()?;
                self.push_new(Kind::Instance { callable: cls, args: Some(args), kwargs: Some(kwargs) }, idx);
            }
            OBJ => {
                let items = self.pop_mark()?;
                let (&cls, rest) = items.split_first().ok_or_else(|| self.err("OBJ without a class"))?;
                let args = self.new_obj(Kind::Tuple, idx);
                self.objs[args].items = rest.to_vec();
                self.push_new(Kind::Instance { callable: cls, args: Some(args), kwargs: None }, idx);
            }
            STACK_GLOBAL => {
                let name = self.pop()?;
                let module = self.pop()?;
                let (module, name) = (self.global_text(module)?, self.global_text(name)?);
                self.push_new(Kind::Global { module, name }, idx);
            }
            APPEND => {
                let v = self.pop()?;
                let t = self.top()?;
                self.add_items(t, vec![v], false)?;
            }
            APPENDS => {
                let items = self.pop_mark()?;
                let t = self.top()?;
                self.add_items(t, items, false)?;
            }
            ADDITEMS => {
                let items = self.pop_mark()?;
                let t = self.top()?;
                self.add_items(t, items, true)?;
            }
            SETITEM => {
                let v = self.pop()?;
                let k = self.pop()?;
                let t = self.top()?;
                let pairs = self.pairs(vec![k, v])?;
                self.add_pairs(t, pairs)?;
            }
            SETITEMS => {
                let items = self.pop_mark()?;
                let t = self.top()?;
                let pairs = self.pairs(items)?;
                self.add_pairs(t, pairs)?;
            }
            BUILD => {
                let state = self.pop()?;
                let t = self.top()?;
                if matches!(self.kind(t), Kind::Scalar(_)) {
                    return Err(self.err("BUILD on a scalar"));
                }
                self.objs[t].state.push(state);
            }
            DICT => {
                let items = self.pop_mark()?;
                let pairs = self.pairs(items)?;
                let d = self.new_obj(Kind::Dict, idx);
                self.objs[d].pairs = pairs;
                self.stack.push(Slot::Obj(d));
            }
            EMPTY_DICT => self.push_new(Kind::Dict, idx),
            EMPTY_LIST => self.push_new(Kind::List, idx),
            EMPTY_TUPLE => self.push_new(Kind::Tuple, idx),
            EMPTY_SET => self.push_new(Kind::Set, idx),
            LIST | TUPLE | FROZENSET => {
                let items = self.pop_mark()?;
                let kind = match code {
                    LIST => Kind::List,
                    TUPLE => Kind::Tuple,
                    _ => Kind::FrozenSet,
                };
                let o = self.new_obj(kind, idx);
                self.objs[o].items = items;
                self.stack.push(Slot::Obj(o));
            }
            TUPLE1 | TUPLE2 | TUPLE3 => {
                let n = usize::from(code - TUPLE1 + 1);
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(self.pop()?);
                }
                items.reverse();
                let o = self.new_obj(Kind::Tuple, idx);
                self.objs[o].items = items;
                self.stack.push(Slot::Obj(o));
            }
            NEXT_BUFFER => {
                self.buffers += 1;
                let n = self.buffers;
                self.push_new(Kind::Buffer(n), idx);
            }
            MEMOIZE => {
                let id = self.memo.len() as u64;
                self.memoize(id)?;
            }
            other => return Err(self.err(format!("opcode {other:#04x} has an unexpected argument"))),
        }
        Ok(())
    }
}

struct Parsed {
    ops: Vec<OpSpan>,
    frames: Vec<Frame>,
    objs: Vec<Obj>,
    root: ObjId,
    stop_end: usize,
    protocol: u8,
    aliases: HashMap<ObjId, Vec<ObjId>>,
    memo_len: usize,
}

fn parse(data: &[u8]) -> CodecResult<Parsed> {
    let mut r = ByteReader::new(data, CODEC);
    let mut vm =
        Vm { stack: Vec::new(), memo: HashMap::new(), objs: Vec::new(), aliases: HashMap::new(), buffers: 0, at: 0 };
    let mut ops: Vec<OpSpan> = Vec::new();
    let mut frames: Vec<Frame> = Vec::new();
    let mut frame_end: Option<usize> = None;
    let mut protocol = 0u8;
    loop {
        let start = r.pos();
        if let Some(end) = frame_end {
            if start == end {
                if let Some(f) = frames.last_mut() {
                    f.end = ops.len();
                }
                frame_end = None;
            }
        }
        let (code, arg) = read_op(&mut r)?;
        if let Some(end) = frame_end {
            if r.pos() > end {
                return Err(CodecError::malformed(CODEC, start, "opcode crosses a FRAME boundary"));
            }
        }
        let idx = ops.len();
        ops.push(OpSpan { start, code });
        vm.at = start;
        match &arg {
            Arg::Proto(p) => protocol = *p,
            Arg::Frame(len) => {
                if frame_end.is_some() {
                    return Err(CodecError::malformed(CODEC, start, "new FRAME before the end of the current one"));
                }
                frame_end = Some(frame_limit(&r, *len)?);
                frames.push(Frame { op: idx, end: idx + 1 });
            }
            _ => {}
        }
        vm.step(idx, code, arg)?;
        if code == op::STOP {
            break;
        }
    }
    let stop_end = r.pos();
    if let Some(end) = frame_end {
        if end != stop_end {
            return Err(CodecError::malformed(CODEC, stop_end, "the last FRAME extends past STOP"));
        }
        if let Some(f) = frames.last_mut() {
            f.end = ops.len();
        }
    }
    // STOP pops the result.
    vm.at = stop_end.saturating_sub(1);
    let root = vm.pop()?;
    Ok(Parsed { ops, frames, root, stop_end, protocol, memo_len: vm.memo.len(), objs: vm.objs, aliases: vm.aliases })
}

// ---------------------------------------------------------------------------
// Document
// ---------------------------------------------------------------------------

/// A parsed pickle stream.
#[derive(Debug, Clone)]
pub struct PickleDocument {
    data: Arc<Vec<u8>>,
    ops: Arc<Vec<OpSpan>>,
    frames: Arc<Vec<Frame>>,
    objs: Arc<Vec<Obj>>,
    aliases: Arc<HashMap<ObjId, Vec<ObjId>>>,
    root: ObjId,
    stop_end: usize,
    protocol: u8,
    memo_len: usize,
}

fn unique_key(map: &NodeMap, name: &str) -> String {
    if !map.contains_key(name) {
        return name.to_string();
    }
    (2..).map(|n| format!("{name}#{n}")).find(|k| !map.contains_key(k)).unwrap_or_else(|| name.to_string())
}

struct Projector<'a> {
    doc: &'a PickleDocument,
    shown: HashSet<ObjId>,
    binds: HashMap<NodePath, ObjId>,
}

impl Projector<'_> {
    fn reference(&self, id: ObjId) -> Node {
        match self.doc.objs[id].memo {
            Some(m) => Node::String(format!("$ref:{m}")),
            None => Node::String(format!("$ref:#{id}")),
        }
    }

    fn class_name(&self, callable: ObjId, args: Option<ObjId>) -> (String, bool) {
        let objs = &self.doc.objs;
        match &objs[callable].kind {
            Kind::Global { module, name } => {
                // copy_reg._reconstructor(cls, base, state) is how Python 2
                // pickles plain new-style instances: show the real class.
                if name == "_reconstructor" && (module == "copy_reg" || module == "copyreg") {
                    if let Some(a) = args {
                        if let Some(&cls) = objs[a].items.first() {
                            if let Kind::Global { module, name } = &objs[cls].kind {
                                return (format!("{module}.{name}"), true);
                            }
                        }
                    }
                }
                (format!("{module}.{name}"), false)
            }
            _ => ("$dynamic".into(), false),
        }
    }

    fn key_repr(&self, id: ObjId, depth: usize) -> String {
        let objs = &self.doc.objs;
        if depth > 8 {
            return "...".into();
        }
        if let Some(s) = self.doc.current(id) {
            return match s {
                Scalar::Str(s) => s.clone(),
                Scalar::PyStr(b) => match std::str::from_utf8(b) {
                    Ok(s) => s.to_string(),
                    Err(_) => python_repr_bytes(b),
                },
                Scalar::Int(v) => v.decimal(),
                Scalar::Float(f) => py_float_repr(*f),
                Scalar::Bool(b) => (if *b { "True" } else { "False" }).to_string(),
                Scalar::None => "None".into(),
                Scalar::Bytes(b) | Scalar::ByteArray(b) => python_repr_bytes(b),
            };
        }
        match &objs[id].kind {
            Kind::Tuple | Kind::FrozenSet => {
                let parts: Vec<String> = objs[id]
                    .items
                    .iter()
                    .map(|&i| match self.doc.current(i) {
                        Some(Scalar::Str(s)) => format!("'{s}'"),
                        Some(Scalar::PyStr(b)) => String::from_utf8_lossy(&py2_str_repr(b)).into_owned(),
                        _ => self.key_repr(i, depth + 1),
                    })
                    .collect();
                if matches!(objs[id].kind, Kind::FrozenSet) {
                    format!("frozenset({{{}}})", parts.join(", "))
                } else if parts.len() == 1 {
                    format!("({},)", parts[0])
                } else {
                    format!("({})", parts.join(", "))
                }
            }
            Kind::Global { module, name } => format!("{module}.{name}"),
            Kind::Instance { callable, args, .. } => format!("<{}>", self.class_name(*callable, *args).0),
            _ => format!("<object #{id}>"),
        }
    }

    fn merge_pairs(&mut self, map: &mut NodeMap, pairs: &[(ObjId, ObjId)], path: &NodePath, depth: usize) {
        for &(k, v) in pairs {
            let key = unique_key(map, &self.key_repr(k, 0));
            let child = self.node(v, &path.key(key.clone()), depth + 1);
            map.insert(key, child);
        }
    }

    fn array(&mut self, items: &[ObjId], path: &NodePath, depth: usize) -> Node {
        Node::Array(items.iter().enumerate().map(|(i, &c)| self.node(c, &path.index(i), depth + 1)).collect())
    }

    fn node(&mut self, id: ObjId, path: &NodePath, depth: usize) -> Node {
        let doc = self.doc;
        if let Some(s) = doc.current(id) {
            let n = match s {
                Scalar::None => return Node::Null,
                Scalar::Bool(b) => Node::Bool(*b),
                Scalar::Int(v @ IntVal::Huge(_)) => return v.to_node(),
                Scalar::Int(v) => v.to_node(),
                Scalar::Float(f) => Node::F64(*f),
                Scalar::Str(s) => Node::String(s.clone()),
                Scalar::PyStr(b) => match std::str::from_utf8(b) {
                    Ok(s) => Node::String(s.to_string()),
                    Err(_) => Node::Bytes(b.clone()),
                },
                Scalar::Bytes(b) | Scalar::ByteArray(b) => Node::Bytes(b.clone()),
            };
            self.binds.insert(path.clone(), id);
            return n;
        }
        let obj = &doc.objs[id];
        match &obj.kind {
            Kind::Global { module, name } => return Node::String(format!("$global:{module}.{name}")),
            Kind::Ext(c) => return Node::String(format!("$ext:{c}")),
            Kind::Buffer(n) => return Node::String(format!("$buffer:{n}")),
            Kind::PersText(s) => return Node::map_from([("$persid", Node::String(s.clone()))]),
            _ => {}
        }
        if depth > MAX_DEPTH {
            return Node::String("$depth-limit".into());
        }
        if !self.shown.insert(id) {
            return self.reference(id);
        }
        match &obj.kind {
            Kind::Dict => {
                let mut map = NodeMap::new();
                self.merge_pairs(&mut map, &obj.pairs, path, depth);
                Node::Map(map)
            }
            Kind::List | Kind::Tuple | Kind::Set | Kind::FrozenSet => {
                let mut n = self.array(&obj.items, path, depth);
                if !obj.pairs.is_empty() || !obj.state.is_empty() {
                    // Only possible for odd streams (BUILD on a list): keep it visible.
                    let mut map = NodeMap::new();
                    map.insert("$items".into(), std::mem::replace(&mut n, Node::Null));
                    self.instance_tail(&mut map, obj, path, depth);
                    return Node::Map(map);
                }
                n
            }
            Kind::PersObj(pid) => {
                let child = self.node(*pid, &path.key("$persid"), depth + 1);
                Node::map_from([("$persid", child)])
            }
            Kind::Instance { callable, args, kwargs } => {
                let mut map = NodeMap::new();
                let (class, reconstructed) = self.class_name(*callable, *args);
                map.insert("$class".into(), Node::String(class));
                if let Some(a) = args {
                    let non_empty = !matches!(doc.objs[*a].kind, Kind::Tuple) || !doc.objs[*a].items.is_empty();
                    if non_empty && !reconstructed {
                        let child = self.node(*a, &path.key("$args"), depth + 1);
                        map.insert("$args".into(), child);
                    }
                }
                if let Some(k) = kwargs {
                    let non_empty = !matches!(doc.objs[*k].kind, Kind::Dict) || !doc.objs[*k].pairs.is_empty();
                    if non_empty {
                        let child = self.node(*k, &path.key("$kwargs"), depth + 1);
                        map.insert("$kwargs".into(), child);
                    }
                }
                self.instance_tail(&mut map, obj, path, depth);
                Node::Map(map)
            }
            _ => Node::Null,
        }
    }

    /// Dict items, list items and BUILD state of an instance-like object.
    fn instance_tail(&mut self, map: &mut NodeMap, obj: &Obj, path: &NodePath, depth: usize) {
        let doc = self.doc;
        self.merge_pairs(map, &obj.pairs, path, depth);
        if !obj.items.is_empty() && !map.contains_key("$items") {
            let n = self.array(&obj.items, &path.key("$items"), depth);
            map.insert("$items".into(), n);
        }
        for &st in &obj.state {
            let s = &doc.objs[st];
            let mergeable = |id: ObjId| {
                matches!(doc.objs[id].kind, Kind::Dict | Kind::Scalar(Scalar::None)) && doc.objs[id].state.is_empty()
            };
            if matches!(s.kind, Kind::Dict) && s.state.is_empty() && !self.shown.contains(&st) {
                self.shown.insert(st);
                self.merge_pairs(map, &s.pairs, path, depth);
                continue;
            }
            if matches!(s.kind, Kind::Tuple)
                && s.items.len() == 2
                && s.items.iter().all(|&i| mergeable(i) && !self.shown.contains(&i))
                && !self.shown.contains(&st)
            {
                self.shown.insert(st);
                for &part in &s.items {
                    self.shown.insert(part);
                    let pairs = doc.objs[part].pairs.clone();
                    self.merge_pairs(map, &pairs, path, depth);
                }
                continue;
            }
            let key = unique_key(map, "$state");
            let child = self.node(st, &path.key(key.clone()), depth + 1);
            map.insert(key, child);
        }
    }
}

/// Smallest opcode of `family` (ordered small → large) that fits, keeping
/// `orig` when it still fits.
fn pick(orig: u8, family: &[(u8, bool)]) -> Option<u8> {
    if family.iter().any(|&(c, fits)| c == orig && fits) {
        return Some(orig);
    }
    family.iter().find(|&&(_, fits)| fits).map(|&(c, _)| c)
}

fn with_len(code: u8, width: usize, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 9);
    out.push(code);
    match width {
        1 => out.push(payload.len() as u8),
        4 => out.extend_from_slice(&(payload.len() as u32).to_le_bytes()),
        _ => out.extend_from_slice(&(payload.len() as u64).to_le_bytes()),
    }
    out.extend_from_slice(payload);
    out
}

impl PickleDocument {
    /// Parses a pickle stream (no code is executed).
    pub fn parse(bytes: &[u8]) -> CodecResult<PickleDocument> {
        let p = parse(bytes)?;
        Ok(PickleDocument {
            data: Arc::new(bytes.to_vec()),
            ops: Arc::new(p.ops),
            frames: Arc::new(p.frames),
            objs: Arc::new(p.objs),
            aliases: Arc::new(p.aliases),
            root: p.root,
            stop_end: p.stop_end,
            protocol: p.protocol,
            memo_len: p.memo_len,
        })
    }

    /// Pickle protocol (0 when the stream has no PROTO opcode).
    pub fn protocol(&self) -> u8 {
        self.protocol
    }

    fn project(&self) -> (Node, HashMap<NodePath, ObjId>) {
        let mut p = Projector { doc: self, shown: HashSet::new(), binds: HashMap::new() };
        let root = p.node(self.root, &NodePath::root(), 0);
        (root, p.binds)
    }

    fn op_end(&self, idx: usize) -> usize {
        self.ops.get(idx + 1).map(|o| o.start).unwrap_or(self.stop_end)
    }

    /// Original bytes of opcode `idx` (opcode byte included).
    fn original_op(&self, idx: usize) -> &[u8] {
        &self.data[self.ops[idx].start..self.op_end(idx)]
    }

    /// Serialises the opcode stream, recomputing FRAME lengths.
    pub fn serialize(&self) -> Vec<u8> {
        self.serialize_with(&HashMap::new())
    }

    /// Serialises with some opcodes replaced (`op index → new bytes`).
    fn serialize_with(&self, replaced: &HashMap<usize, Vec<u8>>) -> Vec<u8> {
        let op_bytes = |i: usize| -> &[u8] {
            match replaced.get(&i) {
                Some(b) => b,
                None => self.original_op(i),
            }
        };
        let mut frame_at: HashMap<usize, u64> = HashMap::new();
        for f in self.frames.iter() {
            let len: usize = (f.op + 1..f.end).map(|i| op_bytes(i).len()).sum();
            frame_at.insert(f.op, len as u64);
        }
        let mut out = Vec::with_capacity(self.data.len() + 64);
        for i in 0..self.ops.len() {
            match frame_at.get(&i) {
                Some(len) => {
                    out.push(op::FRAME);
                    out.extend_from_slice(&len.to_le_bytes());
                }
                None => out.extend_from_slice(op_bytes(i)),
            }
        }
        out.extend_from_slice(&self.data[self.stop_end..]);
        out
    }

    fn current(&self, id: ObjId) -> Option<&Scalar> {
        match &self.objs[id].kind {
            Kind::Scalar(s) => Some(s),
            _ => None,
        }
    }

    fn encode_int(&self, path: &NodePath, orig: u8, idx: usize, v: &IntVal) -> CodecResult<Vec<u8>> {
        use op::*;
        let small = v.as_i64();
        let text_long = |suffix: bool| {
            let mut out = vec![LONG];
            out.extend_from_slice(v.decimal().as_bytes());
            if suffix {
                out.push(b'L');
            }
            out.push(b'\n');
            out
        };
        match orig {
            INT => {
                return Ok(match small {
                    Some(x) => format!("I{x}\n").into_bytes(),
                    None => text_long(true),
                })
            }
            LONG => {
                let suffix = self.original_op(idx).ends_with(b"L\n");
                return Ok(text_long(suffix));
            }
            _ => {}
        }
        let twos = v.twos_le().ok_or_else(|| CodecError::unencodable(CODEC, path, "integer is too large"))?;
        let in_range = |lo: i64, hi: i64| small.is_some_and(|x| (lo..=hi).contains(&x));
        let binary = self.protocol >= 2 || matches!(orig, LONG1 | LONG4);
        let family = [
            (BININT1, in_range(0, 255)),
            (BININT2, in_range(0, 65535)),
            (BININT, in_range(i64::from(i32::MIN), i64::from(i32::MAX))),
            (LONG1, binary && twos.len() <= 255),
            (LONG4, binary && twos.len() <= i32::MAX as usize),
        ];
        let code = match pick(orig, &family) {
            Some(c) => c,
            // Protocol 1 has no binary long: fall back to the text form.
            None if !binary => return Ok(text_long(true)),
            None => return Err(CodecError::unencodable(CODEC, path, "integer is too large for LONG4")),
        };
        let x = small.unwrap_or(0);
        Ok(match code {
            BININT1 => vec![BININT1, x as u8],
            BININT2 => {
                let mut o = vec![BININT2];
                o.extend_from_slice(&(x as u16).to_le_bytes());
                o
            }
            BININT => {
                let mut o = vec![BININT];
                o.extend_from_slice(&(x as i32).to_le_bytes());
                o
            }
            LONG1 => with_len(LONG1, 1, &twos),
            _ => with_len(LONG4, 4, &twos),
        })
    }

    fn encode_text(&self, path: &NodePath, orig: u8, s: &str) -> CodecResult<Vec<u8>> {
        use op::*;
        if orig == UNICODE {
            let mut out = vec![UNICODE];
            out.extend_from_slice(&raw_unicode_escape(s));
            out.push(b'\n');
            return Ok(out);
        }
        let n = s.len();
        let p4 = self.protocol >= 4;
        let family = [(SHORT_BINUNICODE, p4 && n <= 255), (BINUNICODE, n <= u32::MAX as usize), (BINUNICODE8, p4)];
        let code = pick(orig, &family)
            .ok_or_else(|| CodecError::unencodable(CODEC, path, "string is too long for BINUNICODE"))?;
        Ok(match code {
            SHORT_BINUNICODE => with_len(code, 1, s.as_bytes()),
            BINUNICODE => with_len(code, 4, s.as_bytes()),
            _ => with_len(code, 8, s.as_bytes()),
        })
    }

    fn encode_pystr(&self, path: &NodePath, orig: u8, b: &[u8]) -> CodecResult<Vec<u8>> {
        use op::*;
        if orig == STRING {
            let mut out = vec![STRING];
            out.extend_from_slice(&py2_str_repr(b));
            out.push(b'\n');
            return Ok(out);
        }
        let family = [(SHORT_BINSTRING, b.len() <= 255), (BINSTRING, b.len() <= i32::MAX as usize)];
        let code = pick(orig, &family)
            .ok_or_else(|| CodecError::unencodable(CODEC, path, "string is too long for BINSTRING"))?;
        Ok(if code == SHORT_BINSTRING { with_len(code, 1, b) } else { with_len(code, 4, b) })
    }

    fn encode_bytes(&self, path: &NodePath, orig: u8, b: &[u8]) -> CodecResult<Vec<u8>> {
        use op::*;
        if orig == BYTEARRAY8 {
            return Ok(with_len(BYTEARRAY8, 8, b));
        }
        let p4 = self.protocol >= 4;
        let family = [(SHORT_BINBYTES, b.len() <= 255), (BINBYTES, b.len() <= u32::MAX as usize), (BINBYTES8, p4)];
        let code = pick(orig, &family)
            .ok_or_else(|| CodecError::unencodable(CODEC, path, "bytes are too long for BINBYTES"))?;
        Ok(match code {
            SHORT_BINBYTES => with_len(code, 1, b),
            BINBYTES => with_len(code, 4, b),
            _ => with_len(code, 8, b),
        })
    }

    /// Encodes an edit of scalar occurrence `id`: returns the opcode index to
    /// replace, its new bytes and the object that defined the value.
    fn encode_edit(&self, id: ObjId, path: &NodePath, value: &Node) -> CodecResult<(usize, Vec<u8>, ObjId)> {
        let cur = self.current(id).cloned().ok_or_else(|| CodecError::path_not_found(CODEC, path))?;
        let creator = self.objs[id].alias_of.unwrap_or(id);
        let idx = self.objs[creator].op;
        let orig = self.ops[idx].code;
        let mismatch = |expected: &str| CodecError::type_mismatch(CODEC, path, expected, value.kind().name());
        let (_, bytes) = match cur {
            Scalar::Int(_) => {
                let v = match value {
                    Node::Bool(_) => return Err(mismatch("int")),
                    Node::String(s) => IntVal::parse_decimal(s).ok_or_else(|| mismatch("int"))?,
                    other => match other.as_scalar() {
                        Some(savelens_core::ScalarValue::Int(i)) => match i64::try_from(i) {
                            Ok(x) => IntVal::Small(x),
                            Err(_) => IntVal::Big(i.to_string()),
                        },
                        Some(savelens_core::ScalarValue::Float(f))
                            if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.0e18 =>
                        {
                            IntVal::Small(f as i64)
                        }
                        _ => return Err(mismatch("int")),
                    },
                };
                let bytes = self.encode_int(path, orig, idx, &v)?;
                (Scalar::Int(v), bytes)
            }
            Scalar::Bool(_) => {
                let Node::Bool(b) = value else { return Err(mismatch("bool")) };
                let bytes = match orig {
                    op::INT => {
                        if *b {
                            b"I01\n".to_vec()
                        } else {
                            b"I00\n".to_vec()
                        }
                    }
                    _ => vec![if *b { op::NEWTRUE } else { op::NEWFALSE }],
                };
                (Scalar::Bool(*b), bytes)
            }
            Scalar::Float(_) => {
                let f = match value.as_scalar() {
                    Some(v) => v.as_f64(),
                    None => return Err(mismatch("float")),
                };
                let bytes = if orig == op::FLOAT {
                    format!("F{}\n", py_float_repr(f)).into_bytes()
                } else {
                    let mut o = vec![op::BINFLOAT];
                    o.extend_from_slice(&f.to_be_bytes());
                    o
                };
                (Scalar::Float(f), bytes)
            }
            Scalar::Str(_) => {
                let Node::String(s) = value else { return Err(mismatch("str")) };
                (Scalar::Str(s.clone()), self.encode_text(path, orig, s)?)
            }
            Scalar::PyStr(_) => {
                let b = match value {
                    Node::String(s) => s.as_bytes().to_vec(),
                    Node::Bytes(b) => b.clone(),
                    Node::RawRegion(r) => r.bytes.clone(),
                    _ => return Err(mismatch("str")),
                };
                let bytes = self.encode_pystr(path, orig, &b)?;
                (Scalar::PyStr(b), bytes)
            }
            Scalar::Bytes(_) | Scalar::ByteArray(_) => {
                let b = match value {
                    Node::Bytes(b) => b.clone(),
                    Node::RawRegion(r) => r.bytes.clone(),
                    _ => return Err(mismatch("bytes")),
                };
                let bytes = self.encode_bytes(path, orig, &b)?;
                let s = if orig == op::BYTEARRAY8 { Scalar::ByteArray(b) } else { Scalar::Bytes(b) };
                (s, bytes)
            }
            Scalar::None => return Err(CodecError::structure(CODEC, path, "None is read-only")),
        };
        Ok((self.objs[id].op, bytes, creator))
    }

    fn renpy_store_count(&self) -> Option<usize> {
        let root = &self.objs[self.root];
        let first = *root.items.first()?;
        if !matches!(root.kind, Kind::Tuple) || !matches!(self.objs[first].kind, Kind::Dict) {
            return None;
        }
        let n = self.objs[first]
            .pairs
            .iter()
            .filter(|(k, _)| match &self.objs[*k].kind {
                Kind::Scalar(Scalar::Str(s)) => s.starts_with("store."),
                Kind::Scalar(Scalar::PyStr(b)) => b.starts_with(b"store."),
                _ => false,
            })
            .count();
        (n > 0).then_some(n)
    }
}

struct Editor<'a> {
    doc: &'a PickleDocument,
    binds: HashMap<NodePath, ObjId>,
    current: &'a Node,
    /// Opcode index → replacement bytes.
    replaced: HashMap<usize, Vec<u8>>,
    /// Objects whose defining opcode was edited.
    creators: HashSet<ObjId>,
}

impl PathEditable for Editor<'_> {
    fn codec(&self) -> &'static str {
        CODEC
    }

    fn set_value(&mut self, path: &NodePath, value: &Node) -> CodecResult<()> {
        match self.binds.get(path) {
            Some(&id) => {
                let (op_idx, bytes, creator) = self.doc.encode_edit(id, path, value)?;
                if self.doc.objs[id].alias_of.is_none() {
                    self.creators.insert(creator);
                }
                self.replaced.insert(op_idx, bytes);
                Ok(())
            }
            None => Err(match self.current.get(path) {
                Some(n) if n.is_container() => CodecError::structure(
                    CODEC,
                    path,
                    "containers cannot be replaced or resized; only scalar values are editable",
                ),
                Some(_) => CodecError::structure(
                    CODEC,
                    path,
                    "read-only node ($class, $ref, $global, None or another derived value)",
                ),
                None => CodecError::path_not_found(CODEC, path),
            }),
        }
    }
}

impl Document for PickleDocument {
    fn codec_id(&self) -> &'static str {
        CODEC
    }

    fn to_ast(&self) -> Node {
        self.project().0
    }

    fn apply_ast(&mut self, root: &Node) -> CodecResult<()> {
        let (current, binds) = self.project();
        let mut ed = Editor { doc: self, binds, current: &current, replaced: HashMap::new(), creators: HashSet::new() };
        apply_via_diff(&mut ed, &current, root)?;
        let Editor { mut replaced, creators, .. } = ed;
        if replaced.is_empty() {
            return Ok(());
        }
        // References to an edited value keep the old value as literals.
        for creator in creators {
            let old = self.original_op(self.objs[creator].op).to_vec();
            for &alias in self.aliases.get(&creator).map(Vec::as_slice).unwrap_or(&[]) {
                replaced.entry(self.objs[alias].op).or_insert_with(|| old.clone());
            }
        }
        let bytes = self.serialize_with(&replaced);
        *self = PickleDocument::parse(&bytes)?;
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
            "Python pickle protocol {}: {} opcodes, {} virtual objects, {} memo entries (decoded symbolically, no code executed)",
            self.protocol,
            self.ops.len(),
            self.objs.len(),
            self.memo_len
        )];
        if !self.frames.is_empty() {
            notes.push(format!("{} FRAME(s): frame lengths are recomputed after edits", self.frames.len()));
        }
        let mut classes: Vec<String> = self
            .objs
            .iter()
            .filter_map(|o| match &o.kind {
                Kind::Global { module, name } => Some(format!("{module}.{name}")),
                _ => None,
            })
            .collect();
        classes.sort();
        classes.dedup();
        if !classes.is_empty() {
            let shown: Vec<&str> = classes.iter().take(12).map(String::as_str).collect();
            let more = if classes.len() > 12 { format!(" and {} more", classes.len() - 12) } else { String::new() };
            notes.push(format!("referenced globals: {}{more}", shown.join(", ")));
        }
        let shared = self.aliases.len();
        if shared > 0 {
            notes.push(format!(
                "{shared} memoized value(s) are referenced from several places; editing one occurrence rewrites only that occurrence"
            ));
        }
        if let Some(n) = self.renpy_store_count() {
            notes.push(format!("Ren'Py save log: {n} store variable(s) under [0]"));
        }
        let trailing = self.data.len() - self.stop_end;
        if trailing > 0 {
            notes.push(format!("{trailing} byte(s) after STOP are kept verbatim"));
        }
        notes
    }

    fn clone_box(&self) -> Box<dyn Document> {
        Box::new(self.clone())
    }
}

/// Python pickle codec.
#[derive(Debug, Clone, Copy, Default)]
pub struct PickleCodec;

impl StructuralCodec for PickleCodec {
    fn id(&self) -> &'static str {
        CODEC
    }

    fn name(&self) -> &'static str {
        "Python pickle"
    }

    fn detect(&self, bytes: &[u8], _ctx: &DetectContext) -> Confidence {
        if bytes.len() < 2 {
            return 0;
        }
        if bytes[0] == op::PROTO {
            return match bytes[1] {
                2..=5 => match scan(bytes) {
                    Ok((_, end)) if end == bytes.len() => 90,
                    Ok(_) => 50,
                    Err(_) => 0,
                },
                0 | 1 => match parse(bytes) {
                    Ok(p) if p.stop_end == bytes.len() => 60,
                    _ => 0,
                },
                _ => 0,
            };
        }
        // Protocol 0/1: must end with STOP and run through the symbolic VM.
        if bytes[bytes.len() - 1] != op::STOP {
            return 0;
        }
        match parse(bytes) {
            Ok(p) if p.stop_end == bytes.len() && p.ops.len() >= 2 => 60,
            _ => 0,
        }
    }

    fn decode(&self, bytes: &[u8], _ctx: &DetectContext) -> CodecResult<Box<dyn Document>> {
        Ok(Box::new(PickleDocument::parse(bytes)?))
    }
}

#[cfg(test)]
mod tests {
    use super::op::*;
    use super::*;

    fn p(path: &str) -> NodePath {
        NodePath::parse(path).unwrap()
    }

    fn su(s: &str) -> Vec<u8> {
        let mut v = vec![SHORT_BINUNICODE, s.len() as u8];
        v.extend_from_slice(s.as_bytes());
        v
    }

    fn bu(s: &str) -> Vec<u8> {
        let mut v = vec![BINUNICODE];
        v.extend_from_slice(&(s.len() as u32).to_le_bytes());
        v.extend_from_slice(s.as_bytes());
        v
    }

    fn has(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    fn roundtrip(bytes: &[u8]) -> PickleDocument {
        let doc = PickleDocument::parse(bytes).unwrap();
        assert_eq!(doc.encode().unwrap(), bytes, "lossless round trip");
        doc
    }

    fn edit(doc: &PickleDocument, path: &str, value: Node) -> (PickleDocument, Vec<u8>) {
        let mut d = doc.clone();
        let mut root = d.to_ast();
        root.set(&p(path), value, false).unwrap();
        d.apply_ast(&root).unwrap();
        let out = d.encode().unwrap();
        let re = PickleDocument::parse(&out).unwrap();
        assert!(savelens_core::bit_equal(&re.to_ast(), &d.to_ast()), "re-parse matches the edited tree");
        (re, out)
    }

    /// Protocol 2 dict with every int / float / string / bool form.
    fn proto2() -> Vec<u8> {
        let mut v = vec![PROTO, 2, EMPTY_DICT, BINPUT, 0, MARK];
        v.extend(bu("small"));
        v.extend([BININT1, 7]);
        v.extend(bu("mid"));
        v.extend([BININT2, 0x34, 0x12]);
        v.extend(bu("neg"));
        v.extend([BININT]);
        v.extend((-5i32).to_le_bytes());
        v.extend(bu("big"));
        v.extend([LONG1, 5, 0, 0, 0, 0, 1]);
        v.extend(bu("huge"));
        v.extend([LONG4, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        v.extend(bu("ratio"));
        v.push(BINFLOAT);
        v.extend(0.5f64.to_be_bytes());
        v.extend(bu("yes"));
        v.push(NEWTRUE);
        v.extend(bu("no"));
        v.push(NEWFALSE);
        v.extend(bu("nothing"));
        v.push(NONE);
        v.extend([SHORT_BINSTRING, 4]);
        v.extend(b"name");
        v.extend([SHORT_BINSTRING, 4]);
        v.extend(b"Hero");
        v.extend(bu("bin"));
        v.extend([SHORT_BINBYTES, 3, 1, 2, 3]);
        v.extend(bu("items"));
        v.extend([EMPTY_LIST, BINPUT, 1, MARK, BININT1, 1, BININT1, 2, APPENDS]);
        v.extend(bu("alias"));
        v.extend([BINGET, 1]);
        v.extend(bu("pair"));
        v.extend([BININT1, 1, BININT1, 2, TUPLE2]);
        v.extend(bu("t3"));
        v.extend([BININT1, 1, TUPLE1, BININT1, 2, MARK, NONE, TUPLE, TUPLE3]);
        v.extend([SETITEMS]);
        v.extend([BININT1, 42]);
        v.extend(bu("int key"));
        v.extend([SETITEM]);
        v.extend([EMPTY_TUPLE, BINPUT, 9, POP, MARK, NONE, POP_MARK, NONE, DUP, POP, POP]);
        v.push(STOP);
        v
    }

    #[test]
    fn protocol2_projection_and_round_trip() {
        let data = proto2();
        let c = PickleCodec;
        assert_eq!(c.detect(&data, &DetectContext::default()), 90);
        let doc = roundtrip(&data);
        let ast = doc.to_ast();
        assert_eq!(ast.get(&p("small")), Some(&Node::I64(7)));
        assert_eq!(ast.get(&p("mid")), Some(&Node::I64(0x1234)));
        assert_eq!(ast.get(&p("neg")), Some(&Node::I64(-5)));
        assert_eq!(ast.get(&p("big")), Some(&Node::I64(1 << 32)));
        assert_eq!(ast.get(&p("huge")), Some(&Node::String("18446744073709551616".into())));
        assert_eq!(ast.get(&p("ratio")), Some(&Node::F64(0.5)));
        assert_eq!(ast.get(&p("yes")), Some(&Node::Bool(true)));
        assert_eq!(ast.get(&p("no")), Some(&Node::Bool(false)));
        assert_eq!(ast.get(&p("nothing")), Some(&Node::Null));
        assert_eq!(ast.get(&p("name")), Some(&Node::String("Hero".into())));
        assert_eq!(ast.get(&p("bin")), Some(&Node::Bytes(vec![1, 2, 3])));
        assert_eq!(ast.get(&p("items")), Some(&Node::Array(vec![Node::I64(1), Node::I64(2)])));
        assert_eq!(ast.get(&p("alias")), Some(&Node::String("$ref:1".into())));
        assert_eq!(ast.get(&p("pair")), Some(&Node::Array(vec![Node::I64(1), Node::I64(2)])));
        assert_eq!(ast.get(&p("t3[2][0]")), Some(&Node::Null));
        assert_eq!(ast.get(&p("[\"42\"]")), Some(&Node::String("int key".into())));
    }

    #[test]
    fn int_edits_upgrade_within_the_family() {
        let doc = roundtrip(&proto2());
        let (re, out) = edit(&doc, "small", Node::I64(200));
        assert_eq!(re.to_ast().get(&p("small")), Some(&Node::I64(200)));
        assert_eq!(out.len(), proto2().len(), "BININT1 kept");
        let (_, out) = edit(&doc, "small", Node::I64(1000));
        assert!(has(&out, &[BININT2, 0xe8, 0x03]), "upgraded to BININT2");
        let (_, out) = edit(&doc, "small", Node::I64(-1));
        assert!(has(&out, &[BININT, 0xff, 0xff, 0xff, 0xff]), "upgraded to BININT");
        let (re, out) = edit(&doc, "small", Node::I64(1 << 40));
        assert!(has(&out, &[LONG1, 6, 0, 0, 0, 0, 0, 1]), "upgraded to LONG1");
        assert_eq!(re.to_ast().get(&p("small")), Some(&Node::I64(1 << 40)));
        // Big ints are edited through decimal strings.
        let (re, _) = edit(&doc, "huge", Node::String("-123456789012345678901234567890".into()));
        assert_eq!(re.to_ast().get(&p("huge")), Some(&Node::String("-123456789012345678901234567890".into())));
        // LONG1 that now fits stays LONG1.
        let (_, out) = edit(&doc, "big", Node::I64(5));
        assert!(has(&out, &[LONG1, 1, 5]));
        // Floats, bools, strings, bytes.
        let (re, _) = edit(&doc, "ratio", Node::F64(2.25));
        assert_eq!(re.to_ast().get(&p("ratio")), Some(&Node::F64(2.25)));
        let (re, _) = edit(&doc, "yes", Node::Bool(false));
        assert_eq!(re.to_ast().get(&p("yes")), Some(&Node::Bool(false)));
        let (re, _) = edit(&doc, "name", Node::String("Ånna".into()));
        assert_eq!(re.to_ast().get(&p("name")), Some(&Node::String("Ånna".into())));
        let (re, _) = edit(&doc, "bin", Node::Bytes(vec![9; 300]));
        assert_eq!(re.to_ast().get(&p("bin")), Some(&Node::Bytes(vec![9; 300])));
    }

    #[test]
    fn rejects_structural_and_read_only_edits() {
        let doc = roundtrip(&proto2());
        let mut d = doc.clone();
        let mut root = d.to_ast();
        root.set(&p("added"), Node::I64(1), true).unwrap();
        assert!(matches!(d.apply_ast(&root), Err(CodecError::StructureUnsupported { .. })));
        let mut root = doc.to_ast();
        root.set(&p("items"), Node::Array(vec![Node::I64(1)]), false).unwrap();
        assert!(d.apply_ast(&root).is_err());
        let mut root = doc.to_ast();
        root.set(&p("alias"), Node::String("$ref:7".into()), false).unwrap();
        assert!(d.apply_ast(&root).is_err());
        let mut root = doc.to_ast();
        root.set(&p("small"), Node::String("abc".into()), false).unwrap();
        assert!(matches!(d.apply_ast(&root), Err(CodecError::TypeMismatch { .. })));
        assert_eq!(d.encode().unwrap(), proto2(), "failed edits leave the document untouched");
    }

    /// Protocol 5 with frames, memoize, stack_global, sets, buffers.
    fn proto5() -> Vec<u8> {
        let mut body = vec![EMPTY_DICT, MEMOIZE, MARK];
        body.extend(su("store.money"));
        body.push(MEMOIZE);
        body.extend([BININT1, 120]);
        body.extend(su("player"));
        body.push(MEMOIZE);
        body.extend(su("store"));
        body.push(MEMOIZE);
        body.extend(su("Player"));
        body.push(MEMOIZE);
        body.extend([STACK_GLOBAL, MEMOIZE, EMPTY_TUPLE, NEWOBJ, MEMOIZE, EMPTY_DICT, MEMOIZE]);
        body.extend(su("hp"));
        body.push(MEMOIZE);
        body.push(BINFLOAT);
        body.extend(87.5f64.to_be_bytes());
        body.extend([SETITEM, BUILD]);
        body.extend(su("tags"));
        body.extend([EMPTY_SET, MARK, BININT1, 1, BININT1, 2, ADDITEMS]);
        body.extend(su("frozen"));
        body.extend([MARK, BININT1, 3, FROZENSET]);
        body.extend(su("raw"));
        body.extend([BYTEARRAY8, 2, 0, 0, 0, 0, 0, 0, 0, 0xaa, 0xbb]);
        body.extend(su("b8"));
        body.extend([BINBYTES8, 1, 0, 0, 0, 0, 0, 0, 0, 0x01]);
        body.extend(su("u8"));
        body.extend([BINUNICODE8, 2, 0, 0, 0, 0, 0, 0, 0]);
        body.extend(b"ok");
        body.extend(su("buf"));
        body.extend([NEXT_BUFFER, READONLY_BUFFER]);
        body.extend(su("kw"));
        body.extend(su("m"));
        body.extend(su("K"));
        body.extend([STACK_GLOBAL, BININT1, 1, TUPLE1, EMPTY_DICT]);
        body.extend(su("x"));
        body.extend([BININT1, 5, SETITEM, NEWOBJ_EX]);
        body.extend(su("again"));
        body.extend([BINGET, 1]);
        body.extend([SETITEMS, STOP]);
        let mut v = vec![PROTO, 5, FRAME];
        v.extend((body.len() as u64).to_le_bytes());
        v.extend(body);
        v
    }

    #[test]
    fn protocol5_frames_are_recomputed() {
        let data = proto5();
        assert_eq!(PickleCodec.detect(&data, &DetectContext::default()), 90);
        let doc = roundtrip(&data);
        let ast = doc.to_ast();
        assert_eq!(ast.get(&p("[\"store.money\"]")), Some(&Node::I64(120)));
        assert_eq!(ast.get(&p("player[\"$class\"]")), Some(&Node::String("store.Player".into())));
        assert_eq!(ast.get(&p("player.hp")), Some(&Node::F64(87.5)));
        assert_eq!(ast.get(&p("tags")), Some(&Node::Array(vec![Node::I64(1), Node::I64(2)])));
        assert_eq!(ast.get(&p("frozen")), Some(&Node::Array(vec![Node::I64(3)])));
        assert_eq!(ast.get(&p("raw")), Some(&Node::Bytes(vec![0xaa, 0xbb])));
        assert_eq!(ast.get(&p("u8")), Some(&Node::String("ok".into())));
        assert_eq!(ast.get(&p("buf")), Some(&Node::String("$buffer:1".into())));
        assert_eq!(ast.get(&p("kw[\"$class\"]")), Some(&Node::String("m.K".into())));
        assert_eq!(ast.get(&p("kw[\"$kwargs\"].x")), Some(&Node::I64(5)));
        assert_eq!(ast.get(&p("again")), Some(&Node::String("store.money".into())), "memoized str by value");

        let long = "x".repeat(300);
        let (re, out) = edit(&doc, "player.hp", Node::F64(1.0));
        assert_eq!(re.to_ast().get(&p("player.hp")), Some(&Node::F64(1.0)));
        assert_eq!(out.len(), data.len());
        let (re, out) = edit(&doc, "u8", Node::String(long.clone()));
        assert_eq!(re.to_ast().get(&p("u8")), Some(&Node::String(long.clone())));
        let frame_len = u64::from_le_bytes(out[3..11].try_into().unwrap());
        assert_eq!(frame_len as usize, out.len() - 11, "FRAME length recomputed");
        let (_, out) = edit(&doc, "[\"store.money\"]", Node::I64(999_999));
        assert!(has(&out, &[BININT, 0x3f, 0x42, 0x0f, 0x00]));
        assert_eq!(u64::from_le_bytes(out[3..11].try_into().unwrap()) as usize, out.len() - 11);
        let mut d = doc.clone();
        let mut root = d.to_ast();
        root.set(&p("player[\"$class\"]"), Node::String("x".into()), false).unwrap();
        assert!(d.apply_ast(&root).is_err(), "$class is read-only");
        // A GET of a string that is also a dict key: only this occurrence changes.
        let (re, _) = edit(&doc, "again", Node::String("renamed".into()));
        let ast = re.to_ast();
        assert_eq!(ast.get(&p("again")), Some(&Node::String("renamed".into())));
        assert_eq!(ast.get(&p("[\"store.money\"]")), Some(&Node::I64(120)), "key unchanged");
    }

    #[test]
    fn short_unicode_upgrades_to_binunicode() {
        let mut v = vec![PROTO, 4];
        v.extend(su("hello"));
        v.push(STOP);
        let doc = roundtrip(&v);
        let mut d = doc.clone();
        d.apply_ast(&Node::String("y".repeat(256))).unwrap();
        let out = d.encode().unwrap();
        assert_eq!(out[2], BINUNICODE);
        assert_eq!(PickleDocument::parse(&out).unwrap().to_ast(), Node::String("y".repeat(256)));
    }

    /// Protocol 0 text pickle (Python 2 style).
    fn proto0() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend(b"(dp0\nS'gold'\np1\nI100\nsS'name'\np2\nVJos\\u00e9 \\u005c\np3\nsS'hp'\np4\nF1.5\n");
        v.extend(b"sS'big'\np5\nL123456789012345678901234567890L\nsS'flag'\np6\nI01\nsS'esc'\np7\n");
        v.extend(b"S'it\\'s\\n\\x01'\np8\nsS'obj'\np9\n(icopy_reg\n_reconstructor\np10\n(dp11\nS'lvl'\n");
        v.extend(b"p12\nI3\nsbsS'pid'\np13\nPkey1\nsS'lst'\np14\n(lp15\nI1\naI2\nag15\nasS'old'\n");
        v.extend(b"(c__main__\nOld\np16\nI9\nosS'tup'\n(I1\nI2\ntp17\ns.");
        v
    }

    #[test]
    fn protocol0_text_forms() {
        let data = proto0();
        assert_eq!(PickleCodec.detect(&data, &DetectContext::default()), 60);
        let doc = roundtrip(&data);
        assert_eq!(doc.protocol(), 0);
        let ast = doc.to_ast();
        assert_eq!(ast.get(&p("gold")), Some(&Node::I64(100)));
        assert_eq!(ast.get(&p("name")), Some(&Node::String("José \\".into())));
        assert_eq!(ast.get(&p("hp")), Some(&Node::F64(1.5)));
        assert_eq!(ast.get(&p("big")), Some(&Node::String("123456789012345678901234567890".into())));
        assert_eq!(ast.get(&p("flag")), Some(&Node::Bool(true)));
        assert_eq!(ast.get(&p("esc")), Some(&Node::String("it's\n\u{1}".into())));
        assert_eq!(ast.get(&p("obj[\"$class\"]")), Some(&Node::String("copy_reg._reconstructor".into())));
        assert_eq!(ast.get(&p("pid[\"$persid\"]")), Some(&Node::String("key1".into())));
        assert_eq!(ast.get(&p("lst[2]")), Some(&Node::String("$ref:15".into())), "self reference");
        assert_eq!(ast.get(&p("old[\"$class\"]")), Some(&Node::String("__main__.Old".into())));
        assert_eq!(ast.get(&p("old[\"$args\"][0]")), Some(&Node::I64(9)));
        assert_eq!(ast.get(&p("tup")), Some(&Node::Array(vec![Node::I64(1), Node::I64(2)])));

        let (_, out) = edit(&doc, "gold", Node::I64(-250));
        assert!(has(&out, b"I-250\n"));
        let (_, out) = edit(&doc, "gold", Node::String("99999999999999999999".into()));
        assert!(has(&out, b"L99999999999999999999L\n"));
        let (_, out) = edit(&doc, "hp", Node::F64(1e16));
        assert!(has(&out, b"F1e+16\n"));
        let (_, out) = edit(&doc, "big", Node::I64(7));
        assert!(has(&out, b"L7L\n"));
        let (_, out) = edit(&doc, "flag", Node::Bool(false));
        assert!(has(&out, b"I00\n"));
        let (re, out) = edit(&doc, "name", Node::String("a\nb\u{263a}\\".into()));
        assert!(has(&out, b"Va\\u000ab\\u263a\\u005c\n"));
        assert_eq!(re.to_ast().get(&p("name")), Some(&Node::String("a\nb\u{263a}\\".into())));
        let (re, out) = edit(&doc, "esc", Node::String("q'\t".into()));
        assert!(has(&out, b"S'q\\'\\t'\n"));
        assert_eq!(re.to_ast().get(&p("esc")), Some(&Node::String("q'\t".into())));
    }

    #[test]
    fn instances_reduce_build_and_ext() {
        let mut v = vec![PROTO, 2, b'c'];
        v.extend(b"renpy.python\nRevertableDict\n");
        v.extend([BINPUT, 1, EMPTY_TUPLE, NEWOBJ, BINPUT, 2, MARK]);
        v.extend(bu("store.gold"));
        v.extend([BININT2, 0x10, 0x27, SETITEMS, EMPTY_DICT, MARK]);
        v.extend(bu("_rollback"));
        v.extend([NEWTRUE, SETITEMS, BUILD]);
        v.push(b'c');
        v.extend(b"__builtin__\nset\n");
        v.extend([EMPTY_LIST, BININT1, 4, APPEND, TUPLE1, REDUCE]);
        v.extend([EXT1, 1, EXT2, 2, 0, EXT4, 3, 0, 0, 0, TUPLE3]);
        v.extend([BININT1, 7, BINPERSID, BINGET, 2]);
        v.extend([TUPLE, STOP]);
        // TUPLE needs a mark: insert one right after PROTO.
        v.insert(2, MARK);
        let doc = roundtrip(&v);
        let ast = doc.to_ast();
        assert_eq!(ast.get(&p("[0][\"$class\"]")), Some(&Node::String("renpy.python.RevertableDict".into())));
        assert_eq!(ast.get(&p("[0][\"store.gold\"]")), Some(&Node::I64(10000)));
        assert_eq!(ast.get(&p("[0]._rollback")), Some(&Node::Bool(true)));
        assert_eq!(ast.get(&p("[1][\"$class\"]")), Some(&Node::String("__builtin__.set".into())));
        assert_eq!(ast.get(&p("[1][\"$args\"][0][0]")), Some(&Node::I64(4)));
        assert_eq!(
            ast.get(&p("[2]")),
            Some(&Node::Array(vec![
                Node::String("$ext:1".into()),
                Node::String("$ext:2".into()),
                Node::String("$ext:3".into())
            ]))
        );
        assert_eq!(ast.get(&p("[3][\"$persid\"]")), Some(&Node::I64(7)));
        assert_eq!(ast.get(&p("[4]")), Some(&Node::String("$ref:2".into())));
        let (re, _) = edit(&doc, "[0][\"store.gold\"]", Node::I64(70000));
        assert_eq!(re.to_ast().get(&p("[0][\"store.gold\"]")), Some(&Node::I64(70000)));
    }

    #[test]
    fn detection_and_errors() {
        let c = PickleCodec;
        let ctx = DetectContext::default();
        assert_eq!(c.detect(b"{\"a\":1}", &ctx), 0);
        assert_eq!(c.detect(&[PROTO, 9, STOP], &ctx), 0);
        assert_eq!(c.detect(&[PROTO, 2, NONE], &ctx), 0, "no STOP");
        let mut trailing = vec![PROTO, 2, NONE, STOP];
        trailing.extend(b"junk");
        assert_eq!(c.detect(&trailing, &ctx), 50);
        let doc = roundtrip(&trailing);
        assert!(doc.notes().iter().any(|n| n.contains("after STOP")));
        assert!(PickleDocument::parse(&[PROTO, 2, BINGET, 3, STOP]).is_err(), "undefined memo id");
        assert!(PickleDocument::parse(&[PROTO, 2, APPENDS, STOP]).is_err(), "no mark");
        assert!(PickleDocument::parse(&[PROTO, 2, BINUNICODE, 9, 0, 0, 0, b'a']).is_err(), "truncated");
        assert!(PickleDocument::parse(&[PROTO, 2, 0xff, STOP]).is_err(), "unknown opcode");
        // A FRAME that splits an opcode is rejected.
        let mut bad = vec![PROTO, 4, FRAME];
        bad.extend(1u64.to_le_bytes());
        bad.extend([BININT1, 1, STOP]);
        assert!(PickleDocument::parse(&bad).is_err());
    }

    #[test]
    fn bigint_helpers_match_i128() {
        for v in
            [0i128, 1, -1, 127, 128, -128, -129, 255, 256, -256, 32767, -32768, i64::MAX as i128 + 1, -(1i128 << 70)]
        {
            let bytes = decimal_to_twos_le(&v.to_string()).unwrap();
            assert_eq!(bytes, trim_twos(v.to_le_bytes().to_vec()), "{v}");
            assert_eq!(twos_le_to_decimal(&bytes), v.to_string());
            assert_eq!(IntVal::from_twos_le(&bytes).decimal(), v.to_string());
        }
        assert_eq!(decimal_to_twos_le("255").unwrap(), vec![0xff, 0x00]);
        assert_eq!(decimal_to_twos_le("-128").unwrap(), vec![0x80]);
        assert!(decimal_to_twos_le("0").unwrap().is_empty());
        assert!(decimal_to_twos_le("12a").is_none());
    }

    #[test]
    fn float_repr_matches_python() {
        for (x, s) in [
            (1.0, "1.0"),
            (0.5, "0.5"),
            (-2.25, "-2.25"),
            (1e16, "1e+16"),
            (1.5e-7, "1.5e-07"),
            (0.0001, "0.0001"),
            (123456789.0, "123456789.0"),
            (0.1, "0.1"),
        ] {
            assert_eq!(py_float_repr(x), s);
        }
        assert_eq!(py_float_repr(f64::INFINITY), "inf");
        assert_eq!(raw_unicode_unescape(b"\\\\u0041").unwrap(), "\\\\u0041");
        assert_eq!(raw_unicode_unescape(b"\\u0041").unwrap(), "A");
        assert_eq!(escape_decode(b"\\101\\x41\\q").unwrap(), b"AA\\q");
    }

    #[test]
    fn shared_strings_are_edited_per_occurrence() {
        // {"a": "Alex", "b": <GET of "Alex">, "c": <GET of "Alex">}
        let mut v = vec![PROTO, 2, EMPTY_DICT, MARK];
        v.extend(bu("a"));
        v.extend(bu("Alex"));
        v.extend([BINPUT, 5]);
        v.extend(bu("b"));
        v.extend([BINGET, 5]);
        v.extend(bu("c"));
        v.extend([BINGET, 5, SETITEMS, STOP]);
        let doc = roundtrip(&v);
        assert!(doc.notes().iter().any(|n| n.contains("only that occurrence")));
        let s = |x: &str| Some(Node::String(x.into()));
        let (re, _) = edit(&doc, "b", Node::String("Bea".into()));
        let ast = re.to_ast();
        assert_eq!(
            (ast.get(&p("a")).cloned(), ast.get(&p("b")).cloned(), ast.get(&p("c")).cloned()),
            (s("Alex"), s("Bea"), s("Alex"))
        );
        let (re, out) = edit(&doc, "a", Node::String("Zoe".into()));
        let ast = re.to_ast();
        assert_eq!(
            (ast.get(&p("a")).cloned(), ast.get(&p("b")).cloned(), ast.get(&p("c")).cloned()),
            (s("Zoe"), s("Alex"), s("Alex"))
        );
        assert!(!has(&out, &[BINGET, 5]), "references became literals of the old value");
    }
}
