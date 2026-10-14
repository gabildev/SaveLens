//! Lossless INI / CFG codec.
//!
//! Every line of the source is kept verbatim together with its exact line
//! ending (LF, CRLF or none for a missing final newline); a UTF-8 BOM is
//! remembered. Lines are classified as blank lines, comments (`;` / `#`),
//! `[section]` headers, `key<delim>value` entries (`=` or `:`) or opaque
//! lines that are not understood and are re-emitted untouched.
//!
//! Only the *value token* of an edited entry is re-rendered: quotes, the
//! whitespace around the delimiter, inline comments, the decimal style of
//! floats and the letter case of booleans are kept. Keys can be added to and
//! removed from sections, and whole sections can be appended or removed.
//!
//! This module also hosts small text-scalar helpers shared with the XML and
//! property-list codecs (typing of textual values, value-preserving
//! rendering, de-duplicated keys, semantic comparison of projections).

use std::collections::HashSet;

use savelens_core::node::{format_float, parse_numeric_string, NumericStringStyle};
use savelens_core::{bit_equal, Node, NodeMap, NodePath, PathSeg, ScalarValue};

use crate::document::{
    apply_via_diff, coerce_like, Capabilities, Confidence, DetectContext, Document, PathEditable, StructuralCodec,
};
use crate::error::{CodecError, CodecResult};

const CODEC: &str = "ini";
const BOM: &[u8] = &[0xef, 0xbb, 0xbf];

// ---------------------------------------------------------------------------
// Shared text-scalar helpers (used by the INI, XML and plist codecs)
// ---------------------------------------------------------------------------

/// Types a textual value: `true`/`false` (any case) → `Bool`, integers that
/// fit in `i64` → `I64`, finite decimal / exponent floats → `F64`, everything
/// else → `String`. Numbers with leading zeros (`007`) stay strings.
pub(crate) fn classify_scalar(text: &str) -> Node {
    if text.eq_ignore_ascii_case("true") {
        return Node::Bool(true);
    }
    if text.eq_ignore_ascii_case("false") {
        return Node::Bool(false);
    }
    match parse_numeric_string(text) {
        Some((ScalarValue::Int(i), _)) => match i64::try_from(i) {
            Ok(v) => Node::I64(v),
            Err(_) => Node::String(text.to_string()),
        },
        Some((ScalarValue::Float(f), _)) if f.is_finite() => Node::F64(f),
        _ => Node::String(text.to_string()),
    }
}

/// Renders a float keeping the style of `old_raw` when it reproduces `x`
/// exactly (`1.50` stays two-decimal, `1.5e3` stays exponent), falling back to
/// the shortest round-tripping spelling.
pub(crate) fn render_float(old_raw: &str, x: f64) -> Option<String> {
    if !x.is_finite() {
        return None;
    }
    let mut candidates = Vec::with_capacity(4);
    match parse_numeric_string(old_raw.trim()) {
        Some((_, NumericStringStyle::Fixed(d))) => candidates.push(format!("{x:.d$}")),
        Some((_, NumericStringStyle::Float)) => candidates.push(format!("{x:e}")),
        _ => {}
    }
    candidates.push(format_float(x));
    candidates.push(format!("{x}"));
    candidates.push(format!("{x:e}"));
    candidates.into_iter().find(|c| c.parse::<f64>().ok() == Some(x) && matches!(classify_scalar(c), Node::F64(_)))
}

/// Renders a typed scalar as text, keeping the boolean letter case and the
/// float style of `old_raw`.
pub(crate) fn render_scalar(old_raw: &str, value: &Node) -> Option<String> {
    Some(match value {
        Node::Bool(b) => {
            let word = if *b { "true" } else { "false" };
            let old = old_raw.trim();
            let is_bool = old.eq_ignore_ascii_case("true") || old.eq_ignore_ascii_case("false");
            if is_bool && old.len() > 1 && old.chars().all(|c| c.is_ascii_uppercase()) {
                word.to_ascii_uppercase()
            } else if is_bool && old.starts_with(|c: char| c.is_ascii_uppercase()) {
                let mut s = word.to_string();
                s[..1].make_ascii_uppercase();
                s
            } else {
                word.to_string()
            }
        }
        Node::I8(v) => v.to_string(),
        Node::I16(v) => v.to_string(),
        Node::I32(v) => v.to_string(),
        Node::I64(v) => v.to_string(),
        Node::U8(v) => v.to_string(),
        Node::U16(v) => v.to_string(),
        Node::U32(v) => v.to_string(),
        Node::U64(v) => v.to_string(),
        Node::F32(v) => render_float(old_raw, *v as f64)?,
        Node::F64(v) => render_float(old_raw, *v)?,
        Node::String(s) => s.clone(),
        _ => return None,
    })
}

/// Converts an edit into the value to store in a textual slot.
///
/// * `typed == true`: the slot is re-typed from its text (INI unquoted values,
///   XML text leaves). Numeric edits are coerced to the existing type when
///   possible, otherwise they change the type; integers become `I64` and
///   floats `F64`.
/// * `typed == false`: the slot is always a string (quoted INI values, XML
///   attributes, plist strings). Numeric edits keep the numeric-string style
///   of the existing value.
pub(crate) fn normalize_scalar(
    codec: &'static str,
    path: &NodePath,
    existing: &Node,
    edit: &Node,
    typed: bool,
) -> CodecResult<Node> {
    match edit {
        Node::Map(_) | Node::Array(_) => {
            return Err(CodecError::structure(codec, path, "a scalar value cannot be replaced by a container"));
        }
        Node::Null | Node::Bytes(_) | Node::RawRegion(_) => {
            return Err(CodecError::type_mismatch(codec, path, existing.kind().name(), edit.kind().name()));
        }
        _ => {}
    }
    if !typed {
        return match edit {
            Node::String(_) => Ok(edit.clone()),
            other => match (existing, other.as_scalar()) {
                (Node::String(_), Some(v)) => Ok(existing.with_scalar(v, path)?),
                _ => Err(CodecError::type_mismatch(codec, path, "string", other.kind().name())),
            },
        };
    }
    let mut v = edit.clone();
    if !matches!(edit, Node::String(_)) && (existing.kind().is_numeric() || matches!(existing, Node::Bool(_))) {
        if let Ok(c) = coerce_like(codec, path, existing, edit) {
            v = c;
        }
    }
    Ok(match v {
        Node::I8(x) => Node::I64(x as i64),
        Node::I16(x) => Node::I64(x as i64),
        Node::I32(x) => Node::I64(x as i64),
        Node::U8(x) => Node::I64(x as i64),
        Node::U16(x) => Node::I64(x as i64),
        Node::U32(x) => Node::I64(x as i64),
        Node::U64(x) => {
            Node::I64(i64::try_from(x).map_err(|_| {
                CodecError::unencodable(codec, path, format!("{x} does not fit a signed 64-bit integer"))
            })?)
        }
        Node::F32(x) => Node::F64(x as f64),
        other => other,
    })
}

/// Structural comparison of projections where numeric leaves compare by
/// value (an `I32(5)` edit equals an `I64(5)` projection) and map key order
/// is irrelevant.
pub(crate) fn semantic_eq(a: &Node, b: &Node) -> bool {
    match (a, b) {
        (Node::Map(x), Node::Map(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| semantic_eq(v, w)))
        }
        (Node::Array(x), Node::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(v, w)| semantic_eq(v, w)),
        _ => match (a.as_scalar(), b.as_scalar()) {
            (Some(ScalarValue::Int(x)), Some(ScalarValue::Int(y))) => x == y,
            (Some(ScalarValue::Float(x)), Some(ScalarValue::Float(y))) => {
                x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan())
            }
            (Some(_), Some(_)) => false,
            _ => bit_equal(a, b),
        },
    }
}

/// De-duplicates keys in order: repeated keys become `key#2`, `key#3`…
pub(crate) fn dedup_keys<I: IntoIterator<Item = String>>(keys: I, seen: &mut HashSet<String>) -> Vec<String> {
    keys.into_iter()
        .map(|k| {
            if seen.insert(k.clone()) {
                return k;
            }
            let mut n = 2usize;
            loop {
                let cand = format!("{k}#{n}");
                if seen.insert(cand.clone()) {
                    return cand;
                }
                n += 1;
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Line model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    key: String,
    /// Byte range of the key inside the line text.
    key_start: usize,
    key_end: usize,
    /// Byte range of the value token (including quotes when quoted).
    value_start: usize,
    value_end: usize,
    quoted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Blank,
    Comment,
    Section { name: String },
    Entry(Entry),
    Opaque,
}

#[derive(Debug, Clone)]
struct Line {
    /// Line text without the line ending.
    text: String,
    /// `"\n"`, `"\r\n"` or `""` (last line without final newline).
    eol: String,
    kind: Kind,
}

impl Line {
    fn new(text: String, eol: String) -> Line {
        let kind = classify_line(&text);
        Line { text, eol, kind }
    }

    fn entry(&self) -> Option<&Entry> {
        match &self.kind {
            Kind::Entry(e) => Some(e),
            _ => None,
        }
    }

    /// Text content of an entry's value (without quotes).
    fn value_content(&self) -> Option<&str> {
        let e = self.entry()?;
        let tok = &self.text[e.value_start..e.value_end];
        Some(if e.quoted { &tok[1..tok.len() - 1] } else { tok })
    }

    /// Projected value of an entry.
    fn value_node(&self) -> Option<Node> {
        let e = self.entry()?;
        let content = self.value_content()?;
        Some(if e.quoted { Node::String(content.to_string()) } else { classify_scalar(content) })
    }
}

fn is_ws(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// Offset of an inline comment start (`;`/`#` preceded by whitespace) at or
/// after `from`, returning the offset of the whitespace run before it.
fn inline_comment(text: &str, from: usize) -> Option<usize> {
    let b = text.as_bytes();
    let mut i = from;
    while i < b.len() {
        if (b[i] == b';' || b[i] == b'#') && i > from && (b[i - 1] == b' ' || b[i - 1] == b'\t') {
            let mut s = i;
            while s > from && (b[s - 1] == b' ' || b[s - 1] == b'\t') {
                s -= 1;
            }
            return Some(s);
        }
        i += 1;
    }
    None
}

fn rest_is_ws_or_comment(rest: &str) -> bool {
    let t = rest.trim_start_matches(is_ws);
    t.is_empty() || ((t.starts_with(';') || t.starts_with('#')) && t.len() < rest.len())
}

fn classify_line(text: &str) -> Kind {
    let trimmed = text.trim_matches(is_ws);
    if trimmed.is_empty() {
        return Kind::Blank;
    }
    if trimmed.starts_with(';') || trimmed.starts_with('#') {
        return Kind::Comment;
    }
    if trimmed.starts_with('[') {
        if let Some(close) = trimmed.find(']') {
            let name = trimmed[1..close].trim_matches(is_ws);
            let rest = &trimmed[close + 1..];
            let rest_ok = rest.is_empty() || rest_is_ws_or_comment(rest);
            if !name.is_empty() && !name.starts_with('{') && !name.starts_with('"') && !name.contains('[') && rest_ok {
                return Kind::Section { name: name.to_string() };
            }
        }
        return Kind::Opaque;
    }
    let Some(delim) = text.find(['=', ':']) else {
        return Kind::Opaque;
    };
    let key_start = text.len() - text.trim_start_matches(is_ws).len();
    let key = text[..delim].trim_matches(is_ws);
    if key.is_empty() {
        return Kind::Opaque;
    }
    let key_end = key_start + key.len();
    let after = &text[delim + 1..];
    let value_start = delim + 1 + (after.len() - after.trim_start_matches(is_ws).len());
    let rest = &text[value_start..];
    // Quoted value: `"..."` followed only by whitespace / a comment.
    if let Some(inner) = rest.strip_prefix('"') {
        if let Some(close) = inner.find('"') {
            let end = value_start + 1 + close + 1;
            if rest_is_ws_or_comment(&text[end..]) {
                return Kind::Entry(Entry {
                    key: key.to_string(),
                    key_start,
                    key_end,
                    value_start,
                    value_end: end,
                    quoted: true,
                });
            }
        }
    }
    let raw_end = inline_comment(text, value_start).unwrap_or(text.len());
    let value_end = value_start + text[value_start..raw_end].trim_end_matches(is_ws).len();
    Kind::Entry(Entry { key: key.to_string(), key_start, key_end, value_start, value_end, quoted: false })
}

fn split_lines(text: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        match rest.find('\n') {
            Some(i) => {
                let (body, eol) = match rest[..i].strip_suffix('\r') {
                    Some(b) => (b, "\r\n"),
                    None => (&rest[..i], "\n"),
                };
                lines.push(Line::new(body.to_string(), eol.to_string()));
                rest = &rest[i + 1..];
            }
            None => {
                lines.push(Line::new(rest.to_string(), String::new()));
                rest = "";
            }
        }
    }
    lines
}

// ---------------------------------------------------------------------------
// Document
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SectionInfo {
    key: String,
    header: usize,
    /// Exclusive end line (next header or end of file).
    end: usize,
    entries: Vec<(String, usize)>,
}

/// `(key, line index)` pairs in document order.
type KeyedLines = Vec<(String, usize)>;

#[derive(Debug, Clone, Default)]
struct Layout {
    globals: Vec<(String, usize)>,
    sections: Vec<SectionInfo>,
}

/// A parsed INI document.
#[derive(Debug, Clone)]
pub struct IniDocument {
    bom: bool,
    lines: Vec<Line>,
}

impl IniDocument {
    /// Parses INI text (must be UTF-8).
    pub fn parse(bytes: &[u8]) -> CodecResult<IniDocument> {
        let (bom, body) = match bytes.strip_prefix(BOM) {
            Some(rest) => (true, rest),
            None => (false, bytes),
        };
        let text = std::str::from_utf8(body).map_err(|e| {
            CodecError::malformed(CODEC, e.valid_up_to() + if bom { 3 } else { 0 }, "INI text is not valid UTF-8")
        })?;
        Ok(IniDocument { bom, lines: split_lines(text) })
    }

    /// Serialises the document.
    pub fn serialize(&self) -> Vec<u8> {
        let cap = self.lines.iter().map(|l| l.text.len() + l.eol.len()).sum::<usize>() + 3;
        let mut out = Vec::with_capacity(cap);
        if self.bom {
            out.extend_from_slice(BOM);
        }
        for l in &self.lines {
            out.extend_from_slice(l.text.as_bytes());
            out.extend_from_slice(l.eol.as_bytes());
        }
        out
    }

    fn layout(&self) -> Layout {
        let mut globals_raw = Vec::new();
        let mut sections: Vec<(String, usize, KeyedLines)> = Vec::new();
        for (i, l) in self.lines.iter().enumerate() {
            match &l.kind {
                Kind::Section { name } => sections.push((name.clone(), i, Vec::new())),
                Kind::Entry(e) => match sections.last_mut() {
                    Some(s) => s.2.push((e.key.clone(), i)),
                    None => globals_raw.push((e.key.clone(), i)),
                },
                _ => {}
            }
        }
        let mut seen = HashSet::new();
        let gkeys = dedup_keys(globals_raw.iter().map(|(k, _)| k.clone()), &mut seen);
        let skeys = dedup_keys(sections.iter().map(|(k, _, _)| k.clone()), &mut seen);
        let globals = gkeys.into_iter().zip(globals_raw.iter().map(|(_, i)| *i)).collect();
        let n = sections.len();
        let headers: Vec<usize> = sections.iter().map(|s| s.1).collect();
        let sections = sections
            .into_iter()
            .zip(skeys)
            .enumerate()
            .map(|(si, ((_, header, entries), key))| {
                let mut s = HashSet::new();
                let ekeys = dedup_keys(entries.iter().map(|(k, _)| k.clone()), &mut s);
                SectionInfo {
                    key,
                    header,
                    end: if si + 1 < n { headers[si + 1] } else { self.lines.len() },
                    entries: ekeys.into_iter().zip(entries.iter().map(|(_, i)| *i)).collect(),
                }
            })
            .collect();
        Layout { globals, sections }
    }

    /// Semantic projection.
    pub fn project(&self) -> Node {
        let layout = self.layout();
        let mut root = NodeMap::with_capacity(layout.globals.len() + layout.sections.len());
        for (k, i) in &layout.globals {
            if let Some(v) = self.lines[*i].value_node() {
                root.insert(k.clone(), v);
            }
        }
        for s in &layout.sections {
            let mut m = NodeMap::with_capacity(s.entries.len());
            for (k, i) in &s.entries {
                if let Some(v) = self.lines[*i].value_node() {
                    m.insert(k.clone(), v);
                }
            }
            root.insert(s.key.clone(), Node::Map(m));
        }
        Node::Map(root)
    }

    fn dominant_eol(&self) -> String {
        let crlf = self.lines.iter().filter(|l| l.eol == "\r\n").count();
        let lf = self.lines.iter().filter(|l| l.eol == "\n").count();
        if crlf > lf {
            "\r\n".into()
        } else {
            "\n".into()
        }
    }

    /// Line index of the entry addressed by `path` (`key` or `section.key`).
    fn locate_entry(&self, path: &NodePath) -> CodecResult<usize> {
        let layout = self.layout();
        let segs = path.segments();
        let not_found = || CodecError::path_not_found(CODEC, path);
        match segs {
            [PathSeg::Key(k)] => {
                if let Some((_, i)) = layout.globals.iter().find(|(g, _)| g == k) {
                    return Ok(*i);
                }
                if layout.sections.iter().any(|s| &s.key == k) {
                    return Err(CodecError::structure(CODEC, path, "a section cannot be replaced by a scalar value"));
                }
                Err(not_found())
            }
            [PathSeg::Key(s), PathSeg::Key(k)] => {
                let sec = layout.sections.iter().find(|x| &x.key == s).ok_or_else(not_found)?;
                sec.entries.iter().find(|(e, _)| e == k).map(|(_, i)| *i).ok_or_else(not_found)
            }
            _ => Err(not_found()),
        }
    }

    /// Renders a value token for an entry line, verifying that the resulting
    /// line reads back as `target`.
    fn render_token(
        path: &NodePath,
        line_text: &str,
        entry: &Entry,
        quoted: bool,
        target: &Node,
    ) -> CodecResult<String> {
        let old_tok = &line_text[entry.value_start..entry.value_end];
        let old_content = if entry.quoted { &old_tok[1..old_tok.len() - 1] } else { old_tok };
        let text = render_scalar(old_content, target).ok_or_else(|| {
            CodecError::unencodable(CODEC, path, format!("{} values cannot be written to INI", target.kind().name()))
        })?;
        if text.contains(['\n', '\r']) {
            return Err(CodecError::unencodable(CODEC, path, "INI values cannot contain line breaks"));
        }
        if quoted && text.contains('"') {
            return Err(CodecError::unencodable(CODEC, path, "quoted INI values cannot contain `\"`"));
        }
        let tok = if quoted { format!("\"{text}\"") } else { text };
        let candidate = format!("{}{}{}", &line_text[..entry.value_start], tok, &line_text[entry.value_end..]);
        let reread = Line::new(candidate, String::new());
        let ok = reread.entry().is_some_and(|e| e.key == entry.key)
            && reread.value_node().is_some_and(|v| semantic_eq(&v, target));
        if !ok {
            let got = reread.value_node().map(|v| v.preview(40)).unwrap_or_else(|| "an unparsable line".into());
            return Err(CodecError::unencodable(
                CODEC,
                path,
                format!(
                    "`{}` would be read back as {got} in this {} value",
                    tok,
                    if quoted { "quoted" } else { "unquoted" }
                ),
            ));
        }
        Ok(tok)
    }

    /// Builds a new entry line for `key` using `template` (an existing entry
    /// line) for indentation, delimiter spacing and quoting.
    fn new_entry_line(&self, path: &NodePath, template: Option<usize>, key: &str, value: &Node) -> CodecResult<String> {
        if key.is_empty()
            || key != key.trim_matches(is_ws)
            || key.contains(['=', ':', '\n', '\r'])
            || key.starts_with(['[', ';', '#'])
        {
            return Err(CodecError::unencodable(CODEC, path, format!("`{key}` is not a valid INI key")));
        }
        if value.is_container() {
            return Err(CodecError::structure(CODEC, path, "INI entries hold scalar values only"));
        }
        let (indent, sep, template_quoted) = match template
            .and_then(|i| self.lines.get(i))
            .and_then(|l| l.entry().map(|e| (l, e)))
        {
            Some((l, e)) => (l.text[..e.key_start].to_string(), l.text[e.key_end..e.value_start].to_string(), e.quoted),
            None => (String::new(), "=".to_string(), false),
        };
        let target = normalize_scalar(CODEC, path, &classify_scalar(""), value, true)?;
        let prefix = format!("{indent}{key}{sep}");
        let base = Entry {
            key: key.to_string(),
            key_start: indent.len(),
            key_end: indent.len() + key.len(),
            value_start: prefix.len(),
            value_end: prefix.len(),
            quoted: false,
        };
        let want_quotes = template_quoted && matches!(target, Node::String(_));
        let tok = match Self::render_token(path, &prefix, &base, want_quotes, &target) {
            Ok(t) => t,
            Err(_) if !want_quotes && matches!(target, Node::String(_)) => {
                Self::render_token(path, &prefix, &base, true, &target)?
            }
            Err(e) => return Err(e),
        };
        Ok(format!("{prefix}{tok}"))
    }

    /// Inserts lines after line `after` (`None`: at the very beginning).
    fn insert_lines(&mut self, after: Option<usize>, texts: Vec<String>) {
        let eol = self.dominant_eol();
        let pos = after.map(|i| i + 1).unwrap_or(0);
        let n = texts.len();
        let mut last_without_eol = false;
        if let Some(i) = after {
            if self.lines[i].eol.is_empty() {
                self.lines[i].eol = eol.clone();
                last_without_eol = true;
            }
        }
        if after.is_none() && self.lines.is_empty() {
            last_without_eol = false;
        }
        let new_lines: Vec<Line> = texts
            .into_iter()
            .enumerate()
            .map(|(j, t)| {
                let e = if j + 1 == n && last_without_eol { String::new() } else { eol.clone() };
                Line::new(t, e)
            })
            .collect();
        self.lines.splice(pos..pos, new_lines);
    }

    /// Removes lines `start..end`, keeping a missing final newline missing.
    fn remove_lines(&mut self, start: usize, end: usize) {
        let removed_last_without_eol = end == self.lines.len() && self.lines.last().is_some_and(|l| l.eol.is_empty());
        self.lines.drain(start..end);
        if removed_last_without_eol && start == self.lines.len() {
            if let Some(l) = self.lines.last_mut() {
                l.eol.clear();
            }
        }
    }
}

impl PathEditable for IniDocument {
    fn codec(&self) -> &'static str {
        CODEC
    }

    fn set_value(&mut self, path: &NodePath, value: &Node) -> CodecResult<()> {
        let idx = self.locate_entry(path)?;
        let line = &self.lines[idx];
        let entry = line.entry().cloned().ok_or_else(|| CodecError::path_not_found(CODEC, path))?;
        let existing = line.value_node().ok_or_else(|| CodecError::path_not_found(CODEC, path))?;
        let target = normalize_scalar(CODEC, path, &existing, value, !entry.quoted)?;
        let tok = Self::render_token(path, &line.text, &entry, entry.quoted, &target)?;
        let text = format!("{}{}{}", &line.text[..entry.value_start], tok, &line.text[entry.value_end..]);
        let eol = line.eol.clone();
        self.lines[idx] = Line::new(text, eol);
        Ok(())
    }

    fn insert(&mut self, path: &NodePath, value: &Node) -> CodecResult<()> {
        let layout = self.layout();
        match path.segments() {
            [PathSeg::Key(name)] => match value {
                Node::Map(entries) => {
                    if name.is_empty() || name != name.trim_matches(is_ws) || name.contains([']', '[', '\n', '\r']) {
                        return Err(CodecError::unencodable(
                            CODEC,
                            path,
                            format!("`{name}` is not a valid section name"),
                        ));
                    }
                    let template = layout
                        .sections
                        .iter()
                        .rev()
                        .find_map(|s| s.entries.last().map(|(_, i)| *i))
                        .or_else(|| layout.globals.last().map(|(_, i)| *i));
                    let mut texts = Vec::with_capacity(entries.len() + 2);
                    let blank_before =
                        layout.sections.iter().any(|s| s.header > 0 && self.lines[s.header - 1].kind == Kind::Blank);
                    if blank_before && self.lines.last().is_some_and(|l| l.kind != Kind::Blank) {
                        texts.push(String::new());
                    }
                    texts.push(format!("[{name}]"));
                    for (k, v) in entries {
                        texts.push(self.new_entry_line(&path.key(k.clone()), template, k, v)?);
                    }
                    let after = self.lines.len().checked_sub(1);
                    self.insert_lines(after, texts);
                    Ok(())
                }
                Node::Array(_) => Err(CodecError::structure(CODEC, path, "INI has no arrays")),
                scalar => {
                    let template = layout.globals.last().map(|(_, i)| *i);
                    let line =
                        self.new_entry_line(path, template.or_else(|| first_entry(&self.lines)), name, scalar)?;
                    let after = match (template, layout.sections.first()) {
                        (Some(i), _) => Some(i),
                        (None, Some(s)) => s.header.checked_sub(1),
                        (None, None) => self.lines.len().checked_sub(1),
                    };
                    self.insert_lines(after, vec![line]);
                    Ok(())
                }
            },
            [PathSeg::Key(s), PathSeg::Key(k)] => {
                let sec = layout
                    .sections
                    .iter()
                    .find(|x| &x.key == s)
                    .ok_or_else(|| CodecError::path_not_found(CODEC, path))?;
                let last = sec.entries.last().map(|(_, i)| *i);
                let template = last.or_else(|| first_entry(&self.lines));
                let line = self.new_entry_line(path, template, k, value)?;
                self.insert_lines(Some(last.unwrap_or(sec.header)), vec![line]);
                Ok(())
            }
            _ => Err(CodecError::structure(CODEC, path, "INI only nests one level (section → key)")),
        }
    }

    fn remove(&mut self, path: &NodePath) -> CodecResult<()> {
        let layout = self.layout();
        if let [PathSeg::Key(k)] = path.segments() {
            if let Some(s) = layout.sections.iter().find(|s| &s.key == k) {
                self.remove_lines(s.header, s.end);
                return Ok(());
            }
        }
        let idx = self.locate_entry(path)?;
        self.remove_lines(idx, idx + 1);
        Ok(())
    }
}

fn first_entry(lines: &[Line]) -> Option<usize> {
    lines.iter().position(|l| l.entry().is_some())
}

impl Document for IniDocument {
    fn codec_id(&self) -> &'static str {
        CODEC
    }

    fn to_ast(&self) -> Node {
        self.project()
    }

    fn apply_ast(&mut self, root: &Node) -> CodecResult<()> {
        if !matches!(root, Node::Map(_)) {
            return Err(CodecError::type_mismatch(CODEC, "$", "map", root.kind().name()));
        }
        let current = self.project();
        let mut work = self.clone();
        apply_via_diff(&mut work, &current, root)?;
        let got = work.project();
        if !semantic_eq(&got, root) {
            let d = savelens_core::diff(root, &got);
            return Err(CodecError::structure(
                CODEC,
                d.changes.first().map(|c| c.path.to_string()).unwrap_or_else(|| "$".into()),
                "the edit cannot be represented exactly in INI (duplicate keys or key renames shift `#N` suffixes)",
            ));
        }
        *self = work;
        Ok(())
    }

    fn encode(&self) -> CodecResult<Vec<u8>> {
        Ok(self.serialize())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::FULL
    }

    fn notes(&self) -> Vec<String> {
        let mut n = Vec::new();
        if self.bom {
            n.push("UTF-8 BOM".into());
        }
        let crlf = self.lines.iter().filter(|l| l.eol == "\r\n").count();
        let lf = self.lines.iter().filter(|l| l.eol == "\n").count();
        n.push(match (crlf, lf) {
            (0, _) => "LF line endings".to_string(),
            (_, 0) => "CRLF line endings".to_string(),
            _ => format!("mixed line endings ({crlf} CRLF, {lf} LF)"),
        });
        if self.lines.last().is_some_and(|l| l.eol.is_empty()) {
            n.push("no final newline".into());
        }
        let layout = self.layout();
        n.push(format!("{} section(s), {} global key(s)", layout.sections.len(), layout.globals.len()));
        let opaque = self.lines.iter().filter(|l| l.kind == Kind::Opaque).count();
        if opaque > 0 {
            n.push(format!("{opaque} unrecognised line(s) kept verbatim"));
        }
        n
    }

    fn clone_box(&self) -> Box<dyn Document> {
        Box::new(self.clone())
    }
}

/// The INI / CFG codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct IniCodec;

impl StructuralCodec for IniCodec {
    fn id(&self) -> &'static str {
        CODEC
    }

    fn name(&self) -> &'static str {
        "INI / CFG"
    }

    fn detect(&self, bytes: &[u8], ctx: &DetectContext) -> Confidence {
        let body = bytes.strip_prefix(BOM).unwrap_or(bytes);
        let Ok(text) = std::str::from_utf8(body) else { return 0 };
        if text.chars().any(|c| c.is_control() && !matches!(c, '\t' | '\r' | '\n')) {
            return 0;
        }
        let trimmed = text.trim_start();
        match trimmed.chars().next() {
            None | Some('{') | Some('<') => return 0,
            Some('[') => {
                let first_line = trimmed.lines().next().unwrap_or_default();
                if !matches!(classify_line(first_line), Kind::Section { .. }) {
                    return 0;
                }
            }
            _ => {}
        }
        let lines = split_lines(text);
        // Brace-delimited node formats (KSP ConfigNode) are not INI.
        let brace_line = lines.iter().any(|l| matches!(l.text.trim_matches(is_ws), "{" | "}"));
        if brace_line && text.matches('{').count() == text.matches('}').count() {
            return 0;
        }
        let mut meaningful = 0usize;
        let mut good = 0usize;
        let mut entries = 0usize;
        for l in &lines {
            match l.kind {
                Kind::Blank | Kind::Comment => {}
                Kind::Section { .. } => {
                    meaningful += 1;
                    good += 1;
                }
                Kind::Entry(_) => {
                    meaningful += 1;
                    good += 1;
                    entries += 1;
                }
                Kind::Opaque => meaningful += 1,
            }
        }
        if entries == 0 || good * 10 < meaningful * 6 {
            return 0;
        }
        match ctx.extension().as_deref() {
            Some("ini" | "cfg" | "conf" | "txt") => 75,
            _ => 60,
        }
    }

    fn decode(&self, bytes: &[u8], _ctx: &DetectContext) -> CodecResult<Box<dyn Document>> {
        Ok(Box::new(IniDocument::parse(bytes)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> NodePath {
        NodePath::parse(s).unwrap()
    }

    fn rt(src: &[u8]) -> IniDocument {
        let doc = IniDocument::parse(src).unwrap();
        assert_eq!(doc.encode().unwrap(), src);
        doc
    }

    #[test]
    fn lossless_layouts() {
        rt(b"");
        rt(b"a=1");
        rt(b"a=1\n");
        rt(b"\xef\xbb\xbf[S]\r\nx = 1\r\ny:2\n\n; c\n# d\n  [ T ]  ; t\n\tk  =  \"v ; x\"   ; c\nnot understood\n");
        rt(b"[x]\r\na=1\nb=2\r\n\r\n");
    }

    #[test]
    fn projection_types() {
        let doc = rt(b"top=5\n[Game]\nint=-12\nf=1.50\ne=1.5e3\nb=TRUE\nlz=007\ns=hello world ; note\nq=\"42\"\nempty=\ndup=1\ndup=2\n[top]\nk=v\n");
        let ast = doc.to_ast();
        let g = |s: &str| ast.get(&p(s)).cloned().unwrap();
        assert_eq!(g("top"), Node::I64(5));
        assert_eq!(g("Game.int"), Node::I64(-12));
        assert_eq!(g("Game.f"), Node::F64(1.5));
        assert_eq!(g("Game.e"), Node::F64(1500.0));
        assert_eq!(g("Game.b"), Node::Bool(true));
        assert_eq!(g("Game.lz"), Node::String("007".into()));
        assert_eq!(g("Game.s"), Node::String("hello world".into()));
        assert_eq!(g("Game.q"), Node::String("42".into()));
        assert_eq!(g("Game.empty"), Node::String(String::new()));
        assert_eq!(g("Game[\"dup#2\"]"), Node::I64(2));
        assert_eq!(g("[\"top#2\"].k"), Node::String("v".into()));
    }

    #[test]
    fn value_edits_keep_style() {
        let src = "[Game]\r\nratio = 1.50 ; r\r\nflag=TRUE\r\nname = \"Hero\"\r\ncount: 3\r\n";
        let mut doc = rt(src.as_bytes());
        let mut ast = doc.to_ast();
        ast.set(&p("Game.ratio"), Node::F64(2.25), false).unwrap();
        ast.set(&p("Game.flag"), Node::Bool(false), false).unwrap();
        ast.set(&p("Game.name"), Node::String("Sir; Knight".into()), false).unwrap();
        ast.set(&p("Game.count"), Node::I32(40), false).unwrap();
        doc.apply_ast(&ast).unwrap();
        let out = String::from_utf8(doc.encode().unwrap()).unwrap();
        assert_eq!(out, "[Game]\r\nratio = 2.25 ; r\r\nflag=FALSE\r\nname = \"Sir; Knight\"\r\ncount: 40\r\n");
    }

    #[test]
    fn unrepresentable_edits_rejected() {
        let mut doc = rt(b"[a]\nname=Bob\nq=\"x\"\n");
        let mut ast = doc.to_ast();
        ast.set(&p("a.name"), Node::String("12".into()), false).unwrap();
        assert!(doc.apply_ast(&ast).is_err());
        let mut ast = doc.to_ast();
        ast.set(&p("a.name"), Node::String("x ; y".into()), false).unwrap();
        assert!(doc.apply_ast(&ast).is_err());
        let mut ast = doc.to_ast();
        ast.set(&p("a.q"), Node::String("a\"b".into()), false).unwrap();
        assert!(doc.apply_ast(&ast).is_err());
        assert_eq!(doc.encode().unwrap(), b"[a]\nname=Bob\nq=\"x\"\n");
    }

    #[test]
    fn structural_edits() {
        let mut doc = rt(b"g = 1\n\n[A]\nx = 1\n; tail comment\n\n[B]\ny=2");
        let mut ast = doc.to_ast();
        ast.set(&p("A.z"), Node::String("new".into()), true).unwrap();
        ast.set(&p("h"), Node::Bool(true), true).unwrap();
        ast.remove(&p("B.y")).unwrap();
        ast.set(&p("C"), Node::map_from([("k", Node::I64(7))]), true).unwrap();
        doc.apply_ast(&ast).unwrap();
        let out = String::from_utf8(doc.encode().unwrap()).unwrap();
        assert_eq!(out, "g = 1\nh = true\n\n[A]\nx = 1\nz = new\n; tail comment\n\n[B]\n\n[C]\nk = 7");
        let re = IniDocument::parse(out.as_bytes()).unwrap();
        assert!(semantic_eq(&re.to_ast(), &ast));

        let mut ast = doc.to_ast();
        ast.remove(&p("A")).unwrap();
        doc.apply_ast(&ast).unwrap();
        assert_eq!(String::from_utf8(doc.encode().unwrap()).unwrap(), "g = 1\nh = true\n\n[B]\n\n[C]\nk = 7");
    }

    #[test]
    fn new_string_values_get_quoted_when_needed() {
        let mut doc = rt(b"[A]\nx=1\n");
        let mut ast = doc.to_ast();
        ast.set(&p("A.s"), Node::String("123".into()), true).unwrap();
        doc.apply_ast(&ast).unwrap();
        assert_eq!(doc.encode().unwrap(), b"[A]\nx=1\ns=\"123\"\n");
    }

    #[test]
    fn detection() {
        let c = IniCodec;
        let ctx = DetectContext::default();
        assert_eq!(c.detect(b"[General]\nvolume=0.8\n", &ctx), 60);
        assert_eq!(c.detect(b"[General]\nvolume=0.8\n", &DetectContext::for_file("options.ini")), 75);
        assert_eq!(c.detect(b"{\"a\":1}", &ctx), 0);
        assert_eq!(c.detect(b"[1, 2, 3]", &ctx), 0);
        assert_eq!(c.detect(b"[\n  {\"a\": \"b=c\"}\n]", &ctx), 0);
        assert_eq!(c.detect(b"<?xml version=\"1.0\"?><a b=\"1\"/>", &ctx), 0);
        assert_eq!(c.detect(b"just some prose\nwithout any keys\n", &ctx), 0);
        assert_eq!(c.detect(&[0u8, 1, 2, 3, b'=', b'4'], &ctx), 0);
        assert_eq!(c.detect(b"[Only]\n", &ctx), 0);
        assert_eq!(c.detect(b"GAME\n{\n\tfunds = 5\n}\n", &ctx), 0);
        assert_eq!(c.detect(b"a = x\n{\nb = 2\n", &ctx), 60, "unbalanced braces are not a node format");
    }

    #[test]
    fn helpers() {
        assert_eq!(render_float("1.50", 2.5).as_deref(), Some("2.50"));
        assert_eq!(render_float("1.5e3", 2500.0).as_deref(), Some("2.5e3"));
        assert_eq!(render_float("1.0", 0.125).as_deref(), Some("0.125"));
        assert_eq!(render_scalar("True", &Node::Bool(false)).as_deref(), Some("False"));
        assert_eq!(render_scalar("TRUE", &Node::Bool(false)).as_deref(), Some("FALSE"));
        assert!(semantic_eq(&Node::I32(5), &Node::I64(5)));
        assert!(!semantic_eq(&Node::String("5".into()), &Node::I64(5)));
    }
}
