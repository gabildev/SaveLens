//! Lossless JSON codec.
//!
//! The parser builds a concrete syntax tree that keeps every byte of the
//! source: whitespace around every token, the raw spelling of numbers
//! (`1.0`, `1e3`, `-0`), the raw escaping of strings, a UTF-8 BOM and any
//! trailing bytes after the root value (some engines append `\0`).
//!
//! **Recursive nested JSON**: string values that contain another JSON
//! document (Unity `JsonUtility` / ES3 style), Base64-encoded JSON, or
//! Base64-encoded zlib/gzip/zstd/LZ4 JSON are expanded into editable
//! sub-trees and re-escaped / re-encoded on serialisation. Expansion only
//! happens when re-encoding the untouched nested document reproduces the
//! original string exactly.

use base64::Engine;
use savelens_core::{bit_equal, Base64Variant, Node, NodeMap, NodePath, PathSeg};

use crate::document::{Capabilities, Confidence, DetectContext, Document, StructuralCodec};
use crate::error::{CodecError, CodecResult};
use crate::layers::compression::CompressionLayer;
use crate::layers::Layer;

const CODEC: &str = "json";
const MAX_DEPTH: usize = 512;

/// Escaping / layout style of a document, inferred from its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonStyle {
    pub escape_slash: bool,
    pub ascii_only: bool,
    pub hex_upper: bool,
    /// Indentation unit (`"  "`, `"\t"`), `None` for compact documents.
    pub indent: Option<String>,
    pub newline: String,
    /// Whitespace after `:`.
    pub colon_ws: String,
}

impl Default for JsonStyle {
    fn default() -> Self {
        JsonStyle {
            escape_slash: false,
            ascii_only: false,
            hex_upper: false,
            indent: None,
            newline: "\n".into(),
            colon_ws: String::new(),
        }
    }
}

#[derive(Debug, Clone)]
enum NestedKind {
    /// JSON text escaped inside the string.
    Json,
    /// Base64 of JSON text.
    Base64Json(Base64Variant),
    /// Base64 of a compressed JSON text.
    Base64Compressed(Base64Variant, Box<CompressionLayer>),
}

#[derive(Debug, Clone)]
struct Nested {
    kind: NestedKind,
    doc: Box<JsonDoc>,
}

#[derive(Debug, Clone)]
struct JStr {
    /// Source text between the quotes (still escaped).
    raw: String,
    /// Decoded value.
    value: String,
    nested: Option<Nested>,
}

#[derive(Debug, Clone)]
struct Item {
    ws_before: String,
    value: JVal,
    ws_after: String,
}

#[derive(Debug, Clone)]
struct Member {
    ws_before_key: String,
    key: JStr,
    ws_before_colon: String,
    ws_after_colon: String,
    value: JVal,
    ws_after_value: String,
}

#[derive(Debug, Clone)]
enum JVal {
    Null,
    Bool(bool),
    Num(String),
    Str(JStr),
    Arr { items: Vec<Item>, empty_ws: String },
    Obj { members: Vec<Member>, empty_ws: String },
}

/// A parsed JSON document.
#[derive(Debug, Clone)]
pub struct JsonDoc {
    bom: bool,
    lead: String,
    root: JVal,
    trail: String,
    garbage: Vec<u8>,
    style: JsonStyle,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn err(&self, reason: impl Into<String>) -> CodecError {
        CodecError::malformed(CODEC, self.pos, reason)
    }

    fn ws(&mut self) -> String {
        let start = self.pos;
        while self.pos < self.src.len() && matches!(self.src[self.pos], b' ' | b'\t' | b'\n' | b'\r') {
            self.pos += 1;
        }
        // Whitespace is ASCII, always valid UTF-8.
        String::from_utf8_lossy(&self.src[start..self.pos]).into_owned()
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn expect(&mut self, b: u8) -> CodecResult<()> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err(format!("expected `{}`", b as char)))
        }
    }

    fn value(&mut self) -> CodecResult<JVal> {
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(JVal::Str(self.string()?)),
            Some(b't') => self.literal(b"true", JVal::Bool(true)),
            Some(b'f') => self.literal(b"false", JVal::Bool(false)),
            Some(b'n') => self.literal(b"null", JVal::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(c) => Err(self.err(format!("unexpected character `{}`", c as char))),
            None => Err(self.err("unexpected end of input")),
        }
    }

    fn literal(&mut self, word: &[u8], v: JVal) -> CodecResult<JVal> {
        if self.src[self.pos..].starts_with(word) {
            self.pos += word.len();
            Ok(v)
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn number(&mut self) -> CodecResult<JVal> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.err("invalid number")),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("expected digits after `.`"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("expected exponent digits"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        Ok(JVal::Num(String::from_utf8_lossy(&self.src[start..self.pos]).into_owned()))
    }

    fn string(&mut self) -> CodecResult<JStr> {
        self.expect(b'"')?;
        let start = self.pos;
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => break,
                Some(b'\\') => self.pos += 2,
                Some(c) if c < 0x20 => return Err(self.err("raw control character in string")),
                Some(_) => self.pos += 1,
            }
        }
        if self.pos > self.src.len() {
            return Err(self.err("unterminated escape"));
        }
        let raw = std::str::from_utf8(&self.src[start..self.pos])
            .map_err(|_| self.err("string is not valid UTF-8"))?
            .to_string();
        self.pos += 1;
        let value = unescape(&raw).map_err(|r| CodecError::malformed(CODEC, start, r))?;
        Ok(JStr { raw, value, nested: None })
    }

    fn enter(&mut self) -> CodecResult<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        Ok(())
    }

    fn array(&mut self) -> CodecResult<JVal> {
        self.enter()?;
        self.expect(b'[')?;
        let mut items = Vec::new();
        let first_ws = self.ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(JVal::Arr { items, empty_ws: first_ws });
        }
        let mut ws_before = first_ws;
        loop {
            let value = self.value()?;
            let ws_after = self.ws();
            items.push(Item { ws_before, value, ws_after });
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    ws_before = self.ws();
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected `,` or `]`")),
            }
        }
        self.depth -= 1;
        Ok(JVal::Arr { items, empty_ws: String::new() })
    }

    fn object(&mut self) -> CodecResult<JVal> {
        self.enter()?;
        self.expect(b'{')?;
        let mut members = Vec::new();
        let first_ws = self.ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(JVal::Obj { members, empty_ws: first_ws });
        }
        let mut ws_before_key = first_ws;
        loop {
            if self.peek() != Some(b'"') {
                return Err(self.err("expected object key"));
            }
            let key = self.string()?;
            let ws_before_colon = self.ws();
            self.expect(b':')?;
            let ws_after_colon = self.ws();
            let value = self.value()?;
            let ws_after_value = self.ws();
            members.push(Member { ws_before_key, key, ws_before_colon, ws_after_colon, value, ws_after_value });
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    ws_before_key = self.ws();
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected `,` or `}`")),
            }
        }
        self.depth -= 1;
        Ok(JVal::Obj { members, empty_ws: String::new() })
    }
}

fn unescape(raw: &str) -> Result<String, String> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                let hi = u16::from_str_radix(&hex, 16).map_err(|_| format!("invalid \\u escape `{hex}`"))?;
                if (0xd800..0xdc00).contains(&hi) {
                    // Expect a low surrogate.
                    let rest: String = chars.clone().take(6).collect();
                    if let Some(lo_hex) = rest.strip_prefix("\\u") {
                        if let Ok(lo) = u16::from_str_radix(lo_hex, 16) {
                            if (0xdc00..0xe000).contains(&lo) {
                                for _ in 0..6 {
                                    chars.next();
                                }
                                let code = 0x10000 + ((hi as u32 - 0xd800) << 10) + (lo as u32 - 0xdc00);
                                out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                                continue;
                            }
                        }
                    }
                    out.push('\u{fffd}');
                } else {
                    out.push(char::from_u32(hi as u32).unwrap_or('\u{fffd}'));
                }
            }
            Some(other) => return Err(format!("invalid escape `\\{other}`")),
            None => return Err("dangling backslash".into()),
        }
    }
    Ok(out)
}

/// Escapes a string for JSON following `style`.
pub fn escape(s: &str, style: &JsonStyle) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    let hex = |v: u32| if style.hex_upper { format!("\\u{v:04X}") } else { format!("\\u{v:04x}") };
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '/' if style.escape_slash => out.push_str("\\/"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&hex(c as u32)),
            c if style.ascii_only && (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&hex(*unit as u32));
                }
            }
            c => out.push(c),
        }
    }
    out
}

fn collect_style(v: &JVal, s: &mut StyleScan) {
    match v {
        JVal::Str(st) => s.scan_str(&st.raw),
        JVal::Arr { items, .. } => {
            for it in items {
                s.ws(&it.ws_before);
                s.ws(&it.ws_after);
                collect_style(&it.value, s);
            }
        }
        JVal::Obj { members, .. } => {
            for m in members {
                s.scan_str(&m.key.raw);
                s.ws(&m.ws_before_key);
                if s.colon.is_none() {
                    s.colon = Some(m.ws_after_colon.clone());
                }
                collect_style(&m.value, s);
            }
        }
        _ => {}
    }
}

#[derive(Default)]
struct StyleScan {
    escape_slash: bool,
    unicode_escapes_non_ascii: bool,
    raw_non_ascii: bool,
    hex_upper: bool,
    crlf: bool,
    colon: Option<String>,
}

impl StyleScan {
    fn scan_str(&mut self, raw: &str) {
        if raw.contains("\\/") {
            self.escape_slash = true;
        }
        if !raw.is_ascii() {
            self.raw_non_ascii = true;
        }
        let b = raw.as_bytes();
        let mut i = 0;
        while i + 5 < b.len() + 1 {
            if b[i] == b'\\' && i + 1 < b.len() {
                if b[i + 1] == b'u' && i + 6 <= b.len() {
                    let h = &raw[i + 2..i + 6];
                    if h.bytes().any(|c| c.is_ascii_uppercase()) {
                        self.hex_upper = true;
                    }
                    if u32::from_str_radix(h, 16).map(|v| v > 0x7e).unwrap_or(false) {
                        self.unicode_escapes_non_ascii = true;
                    }
                }
                i += 2;
            } else {
                i += 1;
            }
        }
    }

    fn ws(&mut self, w: &str) {
        if w.contains("\r\n") {
            self.crlf = true;
        }
    }
}

fn infer_indent(root: &JVal) -> Option<String> {
    let first_ws = match root {
        JVal::Obj { members, .. } => members.first().map(|m| m.ws_before_key.as_str()),
        JVal::Arr { items, .. } => items.first().map(|i| i.ws_before.as_str()),
        _ => None,
    }?;
    let after_nl = first_ws.rsplit_once('\n')?.1;
    if after_nl.is_empty() {
        None
    } else {
        Some(after_nl.to_string())
    }
}

impl JsonDoc {
    /// Parses a document (with nested-JSON expansion).
    pub fn parse(src: &[u8]) -> CodecResult<JsonDoc> {
        let (bom, body) = match src.strip_prefix(&[0xef, 0xbb, 0xbf]) {
            Some(rest) => (true, rest),
            None => (false, src),
        };
        let mut p = Parser { src: body, pos: 0, depth: 0 };
        let lead = p.ws();
        let mut root = p.value()?;
        let trail = p.ws();
        let garbage = body[p.pos..].to_vec();
        let mut scan = StyleScan::default();
        collect_style(&root, &mut scan);
        let style = JsonStyle {
            escape_slash: scan.escape_slash,
            ascii_only: scan.unicode_escapes_non_ascii && !scan.raw_non_ascii,
            hex_upper: scan.hex_upper,
            indent: infer_indent(&root),
            newline: if scan.crlf { "\r\n".into() } else { "\n".into() },
            colon_ws: scan.colon.unwrap_or_default(),
        };
        expand_nested(&mut root, &style, 0);
        Ok(JsonDoc { bom, lead, root, trail, garbage, style })
    }

    /// Detected style.
    pub fn style(&self) -> &JsonStyle {
        &self.style
    }

    /// Serialises back to bytes.
    pub fn serialize(&self) -> CodecResult<Vec<u8>> {
        let mut out = String::new();
        out.push_str(&self.lead);
        write_val(&self.root, &self.style, &mut out)?;
        out.push_str(&self.trail);
        let mut bytes = Vec::with_capacity(out.len() + 3 + self.garbage.len());
        if self.bom {
            bytes.extend_from_slice(&[0xef, 0xbb, 0xbf]);
        }
        bytes.extend_from_slice(out.as_bytes());
        bytes.extend_from_slice(&self.garbage);
        Ok(bytes)
    }

    /// Semantic projection.
    pub fn project(&self) -> Node {
        project(&self.root)
    }
}

// ---------------------------------------------------------------------------
// Nested JSON
// ---------------------------------------------------------------------------

fn looks_like_json_text(s: &str) -> bool {
    let t = s.trim();
    t.len() >= 2 && ((t.starts_with('{') && t.ends_with('}')) || (t.starts_with('[') && t.ends_with(']')))
}

fn nested_payload(nested: &Nested) -> CodecResult<Vec<u8>> {
    nested.doc.serialize()
}

fn encode_nested(nested: &Nested, outer: &JsonStyle) -> CodecResult<String> {
    let text = nested_payload(nested)?;
    Ok(match &nested.kind {
        NestedKind::Json => {
            let s = String::from_utf8(text)
                .map_err(|_| CodecError::unencodable(CODEC, "nested", "nested JSON is not UTF-8"))?;
            escape(&s, outer)
        }
        NestedKind::Base64Json(v) => b64_engine(*v).encode(&text),
        NestedKind::Base64Compressed(v, layer) => b64_engine(*v).encode(layer.wrap(&text)?),
    })
}

fn b64_engine(v: Base64Variant) -> &'static base64::engine::GeneralPurpose {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    match v {
        Base64Variant::Standard => &STANDARD,
        Base64Variant::StandardNoPad => &STANDARD_NO_PAD,
        Base64Variant::UrlSafe => &URL_SAFE,
        Base64Variant::UrlSafeNoPad => &URL_SAFE_NO_PAD,
    }
}

fn try_nested(st: &JStr, outer: &JsonStyle, depth: usize) -> Option<Nested> {
    if depth > 8 || st.value.len() < 2 {
        return None;
    }
    // 1. Escaped JSON text.
    if looks_like_json_text(&st.value) {
        if let Ok(doc) = JsonDoc::parse(st.value.as_bytes()) {
            if doc.garbage.is_empty() && matches!(doc.root, JVal::Obj { .. } | JVal::Arr { .. }) {
                let n = Nested { kind: NestedKind::Json, doc: Box::new(doc) };
                if encode_nested(&n, outer).ok().as_deref() == Some(st.raw.as_str()) {
                    return Some(n);
                }
            }
        }
        return None;
    }
    // 2. Base64 (optionally compressed) JSON.
    let v = &st.value;
    if v.len() < 16 || !v.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'-' | b'_')) {
        return None;
    }
    let url = v.bytes().any(|b| b == b'-' || b == b'_');
    let padded = v.ends_with('=') || v.len().is_multiple_of(4);
    let variant = match (url, padded) {
        (false, true) => Base64Variant::Standard,
        (false, false) => Base64Variant::StandardNoPad,
        (true, true) => Base64Variant::UrlSafe,
        (true, false) => Base64Variant::UrlSafeNoPad,
    };
    let bytes = b64_engine(variant).decode(v).ok()?;
    let candidates: Vec<(NestedKind, Vec<u8>)> = {
        let mut c = Vec::new();
        if let Some((layer, inner)) = CompressionLayer::detect(&bytes) {
            c.push((NestedKind::Base64Compressed(variant, Box::new(layer)), inner));
        }
        c.push((NestedKind::Base64Json(variant), bytes));
        c
    };
    for (kind, payload) in candidates {
        let Ok(text) = std::str::from_utf8(&payload) else { continue };
        if !looks_like_json_text(text) {
            continue;
        }
        let Ok(doc) = JsonDoc::parse(&payload) else { continue };
        if !doc.garbage.is_empty() {
            continue;
        }
        let n = Nested { kind, doc: Box::new(doc) };
        if encode_nested(&n, outer).ok().as_deref() == Some(st.raw.as_str()) {
            return Some(n);
        }
    }
    None
}

fn expand_nested(v: &mut JVal, style: &JsonStyle, depth: usize) {
    match v {
        JVal::Str(st) => {
            st.nested = try_nested(st, style, depth);
        }
        JVal::Arr { items, .. } => {
            for it in items {
                expand_nested(&mut it.value, style, depth);
            }
        }
        JVal::Obj { members, .. } => {
            for m in members {
                expand_nested(&mut m.value, style, depth);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

fn write_val(v: &JVal, style: &JsonStyle, out: &mut String) -> CodecResult<()> {
    match v {
        JVal::Null => out.push_str("null"),
        JVal::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        JVal::Num(raw) => out.push_str(raw),
        JVal::Str(st) => {
            out.push('"');
            match &st.nested {
                Some(n) => out.push_str(&encode_nested(n, style)?),
                None => out.push_str(&st.raw),
            }
            out.push('"');
        }
        JVal::Arr { items, empty_ws } => {
            out.push('[');
            if items.is_empty() {
                out.push_str(empty_ws);
            }
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&it.ws_before);
                write_val(&it.value, style, out)?;
                out.push_str(&it.ws_after);
            }
            out.push(']');
        }
        JVal::Obj { members, empty_ws } => {
            out.push('{');
            if members.is_empty() {
                out.push_str(empty_ws);
            }
            for (i, m) in members.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&m.ws_before_key);
                out.push('"');
                out.push_str(&m.key.raw);
                out.push('"');
                out.push_str(&m.ws_before_colon);
                out.push(':');
                out.push_str(&m.ws_after_colon);
                write_val(&m.value, style, out)?;
                out.push_str(&m.ws_after_value);
            }
            out.push('}');
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Projection
// ---------------------------------------------------------------------------

fn project_number(raw: &str) -> Node {
    let is_int = !raw.contains(['.', 'e', 'E']);
    if is_int {
        if let Ok(i) = raw.parse::<i64>() {
            return Node::I64(i);
        }
        if let Ok(u) = raw.parse::<u64>() {
            return Node::U64(u);
        }
    }
    Node::F64(raw.parse::<f64>().unwrap_or(f64::NAN))
}

/// AST keys of an object, de-duplicating repeated keys as `key#2`, `key#3`…
fn ast_keys(members: &[Member]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    members
        .iter()
        .map(|m| {
            let mut k = m.key.value.clone();
            if !seen.insert(k.clone()) {
                let mut n = 2;
                loop {
                    let cand = format!("{}#{n}", m.key.value);
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

fn project(v: &JVal) -> Node {
    match v {
        JVal::Null => Node::Null,
        JVal::Bool(b) => Node::Bool(*b),
        JVal::Num(raw) => project_number(raw),
        JVal::Str(st) => match &st.nested {
            Some(n) => project(&n.doc.root),
            None => Node::String(st.value.clone()),
        },
        JVal::Arr { items, .. } => Node::Array(items.iter().map(|i| project(&i.value)).collect()),
        JVal::Obj { members, .. } => {
            let keys = ast_keys(members);
            let mut m = NodeMap::with_capacity(members.len());
            for (k, mem) in keys.into_iter().zip(members) {
                m.insert(k, project(&mem.value));
            }
            Node::Map(m)
        }
    }
}

// ---------------------------------------------------------------------------
// Editing (reconcile CST with an edited AST)
// ---------------------------------------------------------------------------

fn render_number(node: &Node, path: &NodePath) -> CodecResult<String> {
    Ok(match node {
        Node::I8(v) => v.to_string(),
        Node::I16(v) => v.to_string(),
        Node::I32(v) => v.to_string(),
        Node::I64(v) => v.to_string(),
        Node::U8(v) => v.to_string(),
        Node::U16(v) => v.to_string(),
        Node::U32(v) => v.to_string(),
        Node::U64(v) => v.to_string(),
        Node::F32(v) if v.is_finite() => format!("{v:?}"),
        Node::F64(v) if v.is_finite() => format!("{v:?}"),
        Node::F32(_) | Node::F64(_) => {
            return Err(CodecError::unencodable(CODEC, path, "JSON cannot represent NaN or infinity"))
        }
        other => return Err(CodecError::type_mismatch(CODEC, path, "number", other.kind().name())),
    })
}

struct Ctx<'s> {
    style: &'s JsonStyle,
}

impl Ctx<'_> {
    fn indent(&self, level: usize) -> Option<String> {
        self.style.indent.as_ref().map(|unit| format!("{}{}", self.style.newline, unit.repeat(level)))
    }

    /// Builds a fresh CST value for a node.
    fn render(&self, node: &Node, level: usize, path: &NodePath) -> CodecResult<JVal> {
        Ok(match node {
            Node::Null => JVal::Null,
            Node::Bool(b) => JVal::Bool(*b),
            Node::String(s) => JVal::Str(JStr { raw: escape(s, self.style), value: s.clone(), nested: None }),
            Node::Bytes(b) => {
                let s = base64::engine::general_purpose::STANDARD.encode(b);
                JVal::Str(JStr { raw: s.clone(), value: s, nested: None })
            }
            Node::RawRegion(_) => {
                return Err(CodecError::unencodable(CODEC, path, "raw regions cannot be stored in JSON"))
            }
            Node::Array(items) => {
                let inner = self.indent(level + 1).unwrap_or_default();
                let mut out = Vec::with_capacity(items.len());
                for (i, it) in items.iter().enumerate() {
                    out.push(Item {
                        ws_before: inner.clone(),
                        value: self.render(it, level + 1, &path.index(i))?,
                        ws_after: String::new(),
                    });
                }
                if let Some(last) = out.last_mut() {
                    last.ws_after = self.indent(level).unwrap_or_default();
                }
                JVal::Arr { items: out, empty_ws: String::new() }
            }
            Node::Map(m) => {
                let inner = self.indent(level + 1).unwrap_or_default();
                let mut out = Vec::with_capacity(m.len());
                for (k, v) in m {
                    out.push(Member {
                        ws_before_key: inner.clone(),
                        key: JStr { raw: escape(k, self.style), value: k.clone(), nested: None },
                        ws_before_colon: String::new(),
                        ws_after_colon: self.style.colon_ws.clone(),
                        value: self.render(v, level + 1, &path.key(k.clone()))?,
                        ws_after_value: String::new(),
                    });
                }
                if let Some(last) = out.last_mut() {
                    last.ws_after_value = self.indent(level).unwrap_or_default();
                }
                JVal::Obj { members: out, empty_ws: String::new() }
            }
            n => JVal::Num(render_number(n, path)?),
        })
    }

    fn reconcile(&self, v: &mut JVal, node: &Node, level: usize, path: &mut NodePath) -> CodecResult<()> {
        // Unchanged sub-trees keep their exact source text.
        if bit_equal(&project(v), node) {
            return Ok(());
        }
        // Nested documents: edit inside, keep the wrapping.
        if let JVal::Str(st) = v {
            if let Some(n) = &mut st.nested {
                if node.is_container() {
                    let inner_style = n.doc.style.clone();
                    let inner = Ctx { style: &inner_style };
                    return inner.reconcile(&mut n.doc.root, node, 0, path);
                }
                st.nested = None;
            }
        }
        match (v, node) {
            (JVal::Obj { members, empty_ws }, Node::Map(m)) => {
                let keys = ast_keys(members);
                let was_empty = members.is_empty();
                let original_closing = members.last().map(|x| x.ws_after_value.clone());
                let inner_after = if members.len() >= 2 { members[0].ws_after_value.clone() } else { String::new() };
                let template = members.last().cloned();
                let original_last_key = keys.last().cloned();
                let mut old: Vec<Option<Member>> = std::mem::take(members).into_iter().map(Some).collect();
                let mut new_members = Vec::with_capacity(m.len());
                let mut fresh = Vec::with_capacity(m.len());
                for (k, child) in m {
                    path.push(PathSeg::Key(k.clone()));
                    let slot = keys.iter().position(|x| x == k).and_then(|i| old[i].take());
                    match slot {
                        Some(mut mem) => {
                            self.reconcile(&mut mem.value, child, level + 1, path)?;
                            fresh.push(Some(k.clone()) == original_last_key);
                            new_members.push(mem);
                        }
                        None => {
                            let value = self.render(child, level + 1, path)?;
                            let (wb, wbc, wac) = match &template {
                                Some(t) => {
                                    (t.ws_before_key.clone(), t.ws_before_colon.clone(), t.ws_after_colon.clone())
                                }
                                None => (
                                    self.indent(level + 1).unwrap_or_default(),
                                    String::new(),
                                    self.style.colon_ws.clone(),
                                ),
                            };
                            // A de-duplicated AST key (`k#2`) maps back to the plain key text.
                            new_members.push(Member {
                                ws_before_key: wb,
                                key: JStr { raw: escape(k, self.style), value: k.clone(), nested: None },
                                ws_before_colon: wbc,
                                ws_after_colon: wac,
                                value,
                                ws_after_value: String::new(),
                            });
                            fresh.push(true);
                        }
                    }
                    path.pop();
                }
                // Fix whitespace before commas / the closing brace.
                let closing = if was_empty {
                    self.indent(level).unwrap_or_else(|| empty_ws.clone())
                } else {
                    original_closing.unwrap_or_default()
                };
                let n = new_members.len();
                for (i, mem) in new_members.iter_mut().enumerate() {
                    if i + 1 == n {
                        mem.ws_after_value = closing.clone();
                    } else if fresh[i] {
                        mem.ws_after_value = inner_after.clone();
                    }
                }
                if n == 0 {
                    *empty_ws = String::new();
                }
                *members = new_members;
                Ok(())
            }
            (JVal::Arr { items, empty_ws }, Node::Array(a)) => {
                let was_empty = items.is_empty();
                let original_len = items.len();
                let original_closing = items.last().map(|x| x.ws_after.clone());
                let inner_after = if items.len() >= 2 { items[0].ws_after.clone() } else { String::new() };
                let template_before = items
                    .last()
                    .map(|x| x.ws_before.clone())
                    .unwrap_or_else(|| self.indent(level + 1).unwrap_or_default());
                items.truncate(a.len());
                for (i, child) in a.iter().enumerate() {
                    path.push(PathSeg::Index(i));
                    if i < items.len() {
                        self.reconcile(&mut items[i].value, child, level + 1, path)?;
                    } else {
                        items.push(Item {
                            ws_before: template_before.clone(),
                            value: self.render(child, level + 1, path)?,
                            ws_after: String::new(),
                        });
                    }
                    path.pop();
                }
                let closing = if was_empty {
                    self.indent(level).unwrap_or_else(|| empty_ws.clone())
                } else {
                    original_closing.unwrap_or_default()
                };
                let n = items.len();
                for (i, it) in items.iter_mut().enumerate() {
                    if i + 1 == n {
                        it.ws_after = closing.clone();
                    } else if i + 1 >= original_len {
                        // Was the last element or is new: needs separator whitespace.
                        it.ws_after = inner_after.clone();
                    }
                }
                if n == 0 {
                    *empty_ws = String::new();
                }
                Ok(())
            }
            (slot, node) => {
                *slot = self.render(node, level, path)?;
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Codec / Document
// ---------------------------------------------------------------------------

/// JSON document wrapper implementing [`Document`].
#[derive(Debug, Clone)]
pub struct JsonDocument {
    doc: JsonDoc,
}

impl JsonDocument {
    /// Parses JSON bytes.
    pub fn parse(bytes: &[u8]) -> CodecResult<JsonDocument> {
        Ok(JsonDocument { doc: JsonDoc::parse(bytes)? })
    }
}

impl Document for JsonDocument {
    fn codec_id(&self) -> &'static str {
        CODEC
    }

    fn to_ast(&self) -> Node {
        self.doc.project()
    }

    fn apply_ast(&mut self, root: &Node) -> CodecResult<()> {
        let mut work = self.doc.root.clone();
        let style = self.doc.style.clone();
        let ctx = Ctx { style: &style };
        ctx.reconcile(&mut work, root, 0, &mut NodePath::root())?;
        self.doc.root = work;
        Ok(())
    }

    fn encode(&self) -> CodecResult<Vec<u8>> {
        self.doc.serialize()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::FULL
    }

    fn notes(&self) -> Vec<String> {
        let mut n = Vec::new();
        if self.doc.bom {
            n.push("UTF-8 BOM".into());
        }
        if !self.doc.garbage.is_empty() {
            n.push(format!("{} trailing bytes after the JSON value kept verbatim", self.doc.garbage.len()));
        }
        let mut nested = 0;
        count_nested(&self.doc.root, &mut nested);
        if nested > 0 {
            n.push(format!("{nested} nested JSON string(s) expanded"));
        }
        n
    }

    fn clone_box(&self) -> Box<dyn Document> {
        Box::new(self.clone())
    }
}

fn count_nested(v: &JVal, n: &mut usize) {
    match v {
        JVal::Str(st) => {
            if let Some(x) = &st.nested {
                *n += 1;
                count_nested(&x.doc.root, n);
            }
        }
        JVal::Arr { items, .. } => items.iter().for_each(|i| count_nested(&i.value, n)),
        JVal::Obj { members, .. } => members.iter().for_each(|m| count_nested(&m.value, n)),
        _ => {}
    }
}

/// The JSON codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct JsonCodec;

impl StructuralCodec for JsonCodec {
    fn id(&self) -> &'static str {
        CODEC
    }

    fn name(&self) -> &'static str {
        "JSON (lossless, nested JSON aware)"
    }

    fn detect(&self, bytes: &[u8], ctx: &DetectContext) -> Confidence {
        let body = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
        let first = body.iter().find(|b| !b.is_ascii_whitespace());
        let ext_json = matches!(ctx.extension().as_deref(), Some("json"));
        match first {
            Some(b'{') | Some(b'[') => match JsonDoc::parse(bytes) {
                Ok(doc) if doc.garbage.is_empty() => 90,
                Ok(doc) if doc.garbage.iter().all(|&b| b == 0) => 85,
                Ok(_) => 40,
                Err(_) => 0,
            },
            Some(_) if ext_json && JsonDoc::parse(bytes).map(|d| d.garbage.is_empty()).unwrap_or(false) => 40,
            _ => 0,
        }
    }

    fn decode(&self, bytes: &[u8], _ctx: &DetectContext) -> CodecResult<Box<dyn Document>> {
        Ok(Box::new(JsonDocument::parse(bytes)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(src: &str) {
        let doc = JsonDocument::parse(src.as_bytes()).unwrap();
        assert_eq!(String::from_utf8(doc.encode().unwrap()).unwrap(), src);
    }

    #[test]
    fn lossless_formats() {
        rt("{}");
        rt("[]");
        rt("  { }  \n");
        rt(r#"{"a":1,"b":[1,2,3],"c":{"d":null,"e":true,"f":false}}"#);
        rt("{\n    \"gold\" : 1.50e+3 ,\n\t\"name\":\"H\\u00e9ro\\/x\"\n}\n");
        rt("[ 1 , -0 , 0.0 , 1E9 , \"\\ud83d\\ude00\" ]");
        rt("{\"dup\":1,\"dup\":2}");
        rt("\"just a string\"");
    }

    #[test]
    fn bom_and_trailing_garbage() {
        let mut src = vec![0xef, 0xbb, 0xbf];
        src.extend_from_slice(b"{\"a\":1}\n\0\0");
        let doc = JsonDocument::parse(&src).unwrap();
        assert_eq!(doc.encode().unwrap(), src);
        assert!(doc.notes().iter().any(|n| n.contains("trailing")));
    }

    #[test]
    fn projection_types() {
        let doc =
            JsonDocument::parse(br#"{"i":5,"big":18446744073709551615,"f":1.5,"s":"x","n":null,"dup":1,"dup":2}"#)
                .unwrap();
        let ast = doc.to_ast();
        let get = |p: &str| ast.get(&NodePath::parse(p).unwrap()).cloned().unwrap();
        assert_eq!(get("i"), Node::I64(5));
        assert_eq!(get("big"), Node::U64(u64::MAX));
        assert_eq!(get("f"), Node::F64(1.5));
        assert_eq!(get("s"), Node::String("x".into()));
        assert_eq!(get("n"), Node::Null);
        assert_eq!(get("[\"dup#2\"]"), Node::I64(2));
    }

    #[test]
    fn edit_preserves_formatting() {
        let src = "{\n  \"Player\": {\n    \"Gold\": 1200,\n    \"Name\": \"Hero\"\n  },\n  \"Ratio\": 0.50\n}\n";
        let mut doc = JsonDocument::parse(src.as_bytes()).unwrap();
        let mut ast = doc.to_ast();
        ast.set(&NodePath::parse("Player.Gold").unwrap(), Node::I64(1500), false).unwrap();
        doc.apply_ast(&ast).unwrap();
        let out = String::from_utf8(doc.encode().unwrap()).unwrap();
        assert_eq!(out, src.replace("1200", "1500"));
        // Ratio keeps its "0.50" spelling because it was not touched.
        assert!(out.contains("0.50"));
    }

    #[test]
    fn structural_edits_pretty() {
        let src = "{\n  \"a\": 1,\n  \"b\": [\n    1,\n    2\n  ]\n}";
        let mut doc = JsonDocument::parse(src.as_bytes()).unwrap();
        let mut ast = doc.to_ast();
        ast.set(&NodePath::parse("c").unwrap(), Node::map_from([("x", Node::Bool(true))]), true).unwrap();
        if let Some(Node::Array(b)) = ast.get_mut(&NodePath::parse("b").unwrap()) {
            b.push(Node::I64(3));
        }
        doc.apply_ast(&ast).unwrap();
        let out = String::from_utf8(doc.encode().unwrap()).unwrap();
        assert_eq!(out, "{\n  \"a\": 1,\n  \"b\": [\n    1,\n    2,\n    3\n  ],\n  \"c\": {\n    \"x\": true\n  }\n}");
        // And removal.
        let mut ast = doc.to_ast();
        ast.remove(&NodePath::parse("c").unwrap()).unwrap();
        ast.remove(&NodePath::parse("b[2]").unwrap()).unwrap();
        doc.apply_ast(&ast).unwrap();
        assert_eq!(String::from_utf8(doc.encode().unwrap()).unwrap(), src);
    }

    #[test]
    fn structural_edits_compact() {
        let src = r#"{"a":1,"b":[]}"#;
        let mut doc = JsonDocument::parse(src.as_bytes()).unwrap();
        let mut ast = doc.to_ast();
        ast.set(&NodePath::parse("b[0]").unwrap(), Node::String("x/y\"z".into()), true).unwrap();
        ast.set(&NodePath::parse("new").unwrap(), Node::F64(2.0), true).unwrap();
        doc.apply_ast(&ast).unwrap();
        assert_eq!(String::from_utf8(doc.encode().unwrap()).unwrap(), r#"{"a":1,"b":["x/y\"z"],"new":2.0}"#);
    }

    #[test]
    fn nested_json_string_expanded_and_reescaped() {
        let inner = r#"{"gold":100,"items":["a","b"]}"#;
        let src = format!("{{\"save\":{},\"v\":1}}", serde_json::to_string(inner).unwrap());
        let mut doc = JsonDocument::parse(src.as_bytes()).unwrap();
        assert_eq!(doc.encode().unwrap(), src.as_bytes());
        let mut ast = doc.to_ast();
        let p = NodePath::parse("save.gold").unwrap();
        assert_eq!(ast.get(&p), Some(&Node::I64(100)));
        ast.set(&p, Node::I64(5000), false).unwrap();
        doc.apply_ast(&ast).unwrap();
        let out = String::from_utf8(doc.encode().unwrap()).unwrap();
        let expected_inner = r#"{"gold":5000,"items":["a","b"]}"#;
        assert_eq!(out, format!("{{\"save\":{},\"v\":1}}", serde_json::to_string(expected_inner).unwrap()));
    }

    #[test]
    fn doubly_nested_json() {
        let level2 = r#"{"hp":10}"#;
        let level1 = format!("{{\"stats\":{}}}", serde_json::to_string(level2).unwrap());
        let src = format!("{{\"blob\":{}}}", serde_json::to_string(&level1).unwrap());
        let mut doc = JsonDocument::parse(src.as_bytes()).unwrap();
        assert_eq!(doc.encode().unwrap(), src.as_bytes());
        let mut ast = doc.to_ast();
        let p = NodePath::parse("blob.stats.hp").unwrap();
        assert_eq!(ast.get(&p), Some(&Node::I64(10)));
        ast.set(&p, Node::I64(99), false).unwrap();
        doc.apply_ast(&ast).unwrap();
        let reparsed = JsonDocument::parse(&doc.encode().unwrap()).unwrap();
        assert_eq!(reparsed.to_ast().get(&p), Some(&Node::I64(99)));
    }

    #[test]
    fn base64_nested_json_and_compressed() {
        use base64::engine::general_purpose::STANDARD;
        let inner = br#"{"coins":42,"name":"x"}"#;
        let b64 = STANDARD.encode(inner);
        let z = crate::layers::compression::deflate(inner, 6, true).unwrap();
        let bz = STANDARD.encode(&z);
        let src = format!("{{\"a\":\"{b64}\",\"b\":\"{bz}\"}}");
        let mut doc = JsonDocument::parse(src.as_bytes()).unwrap();
        assert_eq!(doc.encode().unwrap(), src.as_bytes());
        let mut ast = doc.to_ast();
        assert_eq!(ast.get(&NodePath::parse("a.coins").unwrap()), Some(&Node::I64(42)));
        assert_eq!(ast.get(&NodePath::parse("b.coins").unwrap()), Some(&Node::I64(42)));
        ast.set(&NodePath::parse("b.coins").unwrap(), Node::I64(7), false).unwrap();
        ast.set(&NodePath::parse("a.coins").unwrap(), Node::I64(8), false).unwrap();
        doc.apply_ast(&ast).unwrap();
        let re = JsonDocument::parse(&doc.encode().unwrap()).unwrap().to_ast();
        assert_eq!(re.get(&NodePath::parse("b.coins").unwrap()), Some(&Node::I64(7)));
        assert_eq!(re.get(&NodePath::parse("a.coins").unwrap()), Some(&Node::I64(8)));
    }

    #[test]
    fn plain_strings_not_expanded() {
        let src = r#"{"a":"[not json","b":"SGVsbG8gV29ybGQhISE=","c":"{}"}"#;
        let doc = JsonDocument::parse(src.as_bytes()).unwrap();
        let ast = doc.to_ast();
        assert_eq!(ast.get(&NodePath::parse("a").unwrap()), Some(&Node::String("[not json".into())));
        assert_eq!(ast.get(&NodePath::parse("b").unwrap()), Some(&Node::String("SGVsbG8gV29ybGQhISE=".into())));
        assert_eq!(ast.get(&NodePath::parse("c").unwrap()), Some(&Node::Map(NodeMap::new())));
        assert_eq!(doc.encode().unwrap(), src.as_bytes());
    }

    #[test]
    fn escape_styles() {
        let src = "{\"p\":\"a\\/b\",\"u\":\"\\u00E9t\\u00E9\"}";
        let mut doc = JsonDocument::parse(src.as_bytes()).unwrap();
        assert!(doc.doc.style.escape_slash);
        assert!(doc.doc.style.ascii_only);
        assert!(doc.doc.style.hex_upper);
        let mut ast = doc.to_ast();
        ast.set(&NodePath::parse("u").unwrap(), Node::String("\u{e9}/\u{fc}".into()), false).unwrap();
        doc.apply_ast(&ast).unwrap();
        assert_eq!(String::from_utf8(doc.encode().unwrap()).unwrap(), "{\"p\":\"a\\/b\",\"u\":\"\\u00E9\\/\\u00FC\"}");
    }

    #[test]
    fn errors() {
        assert!(JsonDocument::parse(b"{").is_err());
        assert!(JsonDocument::parse(b"{\"a\" 1}").is_err());
        assert!(JsonDocument::parse(b"[1,]").is_err());
        assert!(JsonDocument::parse(b"\"\x01\"").is_err());
        assert!(JsonDocument::parse(&[b'"', 0xff, b'"']).is_err());
        let deep = "[".repeat(1000) + &"]".repeat(1000);
        assert!(JsonDocument::parse(deep.as_bytes()).is_err());
        let mut doc = JsonDocument::parse(b"{\"a\":1}").unwrap();
        assert!(doc.apply_ast(&Node::map_from([("a", Node::F64(f64::NAN))])).is_err());
    }

    #[test]
    fn type_change_and_scalar_root() {
        let mut doc = JsonDocument::parse(b"{\"a\":1}").unwrap();
        doc.apply_ast(&Node::map_from([("a", Node::String("one".into()))])).unwrap();
        assert_eq!(doc.encode().unwrap(), b"{\"a\":\"one\"}");
    }

    #[test]
    fn detect_confidence() {
        let c = JsonCodec;
        let ctx = DetectContext::default();
        assert!(c.detect(b"{\"a\":1}", &ctx) >= 90);
        assert_eq!(c.detect(b"hello", &ctx), 0);
        assert_eq!(c.detect(b"{broken", &ctx), 0);
        assert_eq!(c.detect(b"42", &DetectContext::for_file("x.json")), 40);
    }
}
