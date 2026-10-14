//! Lossless Kerbal Space Program `ConfigNode` codec (`.sfs` saves, `.craft`
//! vessels, `.cfg` part / settings files).
//!
//! The format is a tree of nodes:
//!
//! ```text
//! GAME
//! {
//!     version = 1.12.5
//!     FLIGHTSTATE
//!     {
//!         UT = 63290.1817719101
//!     }
//! }
//! ```
//!
//! A node is a name line followed by a `{` line (the brace may also end the
//! name line: `NAME {`), its `key = value` entries and child nodes, and a `}`
//! line. The value is the rest of the line after the first `=` (it may be
//! empty or contain further `=`); `//` starts a comment. Files may also carry
//! entries outside any node (`.craft` files do).
//!
//! **Lossless line model.** Like the INI codec, every line is kept verbatim
//! with its exact line ending (LF, CRLF, none for a missing final newline) and
//! a UTF-8 BOM is remembered; lines that are not understood but contain no
//! brace are kept as opaque lines. Edits only rewrite the *value token* of an
//! entry (indentation, the spacing around `=`, inline comments, the decimal
//! style of floats and the letter case of `True` / `False` are kept).
//!
//! **Projection.** The root is a map of the top-level entries and nodes; a
//! node is a map of its entries and child nodes in file order. Values are
//! typed like INI values (integers → `I64`, finite floats → `F64`,
//! `true`/`false` in any case → `Bool`, everything else → `String`).
//! Repeated names inside one node (entries and child nodes share the
//! namespace) become `NAME`, `NAME#2`, `NAME#3`, … — the same convention as
//! the INI codec, chosen over arrays so that every node keeps a stable path
//! and edits never need index bookkeeping.
//!
//! **Edits.** Values can be changed (also between types: a `String` edit of
//! a number is written as text when it reads back as that string). Entries
//! can be added to and removed from any node; a new entry is placed after
//! the node's last entry, using that entry's indentation and `=` spacing (or
//! the node's brace indentation plus the file's indentation unit). Adding,
//! removing or renaming nodes is rejected with
//! [`CodecError::StructureUnsupported`].
//!
//! Single-line nodes (`NAME { key = value }`) and anonymous `{` blocks are
//! not supported: such text fails to decode, so other codecs (or the raw
//! fallback) handle it.

use std::collections::HashSet;

use savelens_core::{Node, NodeMap, NodePath, PathSeg};

use super::ini::{classify_scalar, dedup_keys, normalize_scalar, render_scalar, semantic_eq};
use crate::document::{
    apply_via_diff, Capabilities, Confidence, DetectContext, Document, PathEditable, StructuralCodec,
};
use crate::error::{CodecError, CodecResult};

const CODEC: &str = "confignode";
const BOM: &[u8] = &[0xef, 0xbb, 0xbf];

// ---------------------------------------------------------------------------
// Line model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    key: String,
    /// Byte range of the key inside the line text.
    key_start: usize,
    key_end: usize,
    /// Byte range of the value token (without surrounding whitespace and
    /// without an inline comment).
    value_start: usize,
    value_end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Blank,
    Comment,
    Entry(Entry),
    /// A node name on its own line (the `{` follows on a later line).
    Name(String),
    /// `NAME {` on one line.
    NameOpen(String),
    Open,
    Close,
    /// Not understood, re-emitted untouched. Never contains a brace.
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

    fn value_node(&self) -> Option<Node> {
        let e = self.entry()?;
        Some(classify_scalar(&self.text[e.value_start..e.value_end]))
    }
}

fn is_ws(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// The part of a line before a `//` comment.
fn code_part(text: &str) -> &str {
    match text.find("//") {
        Some(i) => &text[..i],
        None => text,
    }
}

fn classify_line(text: &str) -> Kind {
    let code = code_part(text);
    let t = code.trim_matches(is_ws);
    if t.is_empty() {
        return if text.trim_matches(is_ws).is_empty() { Kind::Blank } else { Kind::Comment };
    }
    if t == "{" {
        return Kind::Open;
    }
    if t == "}" {
        return Kind::Close;
    }
    if let Some(eq) = code.find('=') {
        let key = code[..eq].trim_matches(is_ws);
        if key.is_empty() || key.contains(['{', '}']) {
            return Kind::Opaque;
        }
        let key_start = code.len() - code.trim_start_matches(is_ws).len();
        let key_end = key_start + key.len();
        let after = &code[eq + 1..];
        let value_start = eq + 1 + (after.len() - after.trim_start_matches(is_ws).len());
        let value_end = value_start + code[value_start..].trim_end_matches(is_ws).len();
        return Kind::Entry(Entry { key: key.to_string(), key_start, key_end, value_start, value_end });
    }
    if let Some(name) = t.strip_suffix('{') {
        let name = name.trim_matches(is_ws);
        if !name.is_empty() && !name.contains(['{', '}']) {
            return Kind::NameOpen(name.to_string());
        }
        return Kind::Opaque;
    }
    if t.contains(['{', '}']) {
        return Kind::Opaque;
    }
    Kind::Name(t.to_string())
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
// Tree layout
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Block {
    /// Line of the `{` (the name line for `NAME {`).
    open: usize,
    items: Vec<Item>,
}

#[derive(Debug, Clone)]
enum ItemKind {
    Entry(usize),
    Block(Block),
}

#[derive(Debug, Clone)]
struct Item {
    /// Projection key (`NAME`, `NAME#2`, …).
    key: String,
    kind: ItemKind,
}

/// A node under construction while scanning the lines.
struct Frame {
    open: usize,
    name: String,
    raw: Vec<(String, ItemKind)>,
}

fn finish(raw: Vec<(String, ItemKind)>) -> Vec<Item> {
    let mut seen = HashSet::with_capacity(raw.len());
    let (names, kinds): (Vec<String>, Vec<ItemKind>) = raw.into_iter().unzip();
    dedup_keys(names, &mut seen).into_iter().zip(kinds).map(|(key, kind)| Item { key, kind }).collect()
}

/// Builds the node tree; fails on unbalanced or ambiguous structure.
fn build_tree(lines: &[Line], offsets: &[usize]) -> CodecResult<Vec<Item>> {
    let err = |i: usize, reason: String| CodecError::malformed(CODEC, offsets.get(i).copied().unwrap_or(0), reason);
    let mut root: Vec<(String, ItemKind)> = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut pending: Option<(String, usize)> = None;
    for (i, l) in lines.iter().enumerate() {
        if matches!(l.kind, Kind::Blank | Kind::Comment) {
            continue;
        }
        if let Some((name, at)) = &pending {
            if l.kind != Kind::Open {
                return Err(err(*at, format!("line {}: node name `{name}` is not followed by `{{`", at + 1)));
            }
        }
        match &l.kind {
            Kind::Blank | Kind::Comment => {}
            Kind::Open => {
                let Some((name, _)) = pending.take() else {
                    return Err(err(i, format!("line {}: `{{` without a node name", i + 1)));
                };
                stack.push(Frame { open: i, name, raw: Vec::new() });
            }
            Kind::NameOpen(name) => stack.push(Frame { open: i, name: name.clone(), raw: Vec::new() }),
            Kind::Name(name) => pending = Some((name.clone(), i)),
            Kind::Entry(e) => {
                let target = match stack.last_mut() {
                    Some(f) => &mut f.raw,
                    None => &mut root,
                };
                target.push((e.key.clone(), ItemKind::Entry(i)));
            }
            Kind::Close => {
                let Some(frame) = stack.pop() else {
                    return Err(err(i, format!("line {}: `}}` without a matching `{{`", i + 1)));
                };
                let block = Block { open: frame.open, items: finish(frame.raw) };
                let target = match stack.last_mut() {
                    Some(f) => &mut f.raw,
                    None => &mut root,
                };
                target.push((frame.name, ItemKind::Block(block)));
            }
            Kind::Opaque => {
                if code_part(&l.text).contains(['{', '}']) {
                    return Err(err(i, format!("line {}: unsupported brace syntax (single-line node?)", i + 1)));
                }
            }
        }
    }
    if let Some((name, at)) = pending {
        return Err(err(at, format!("line {}: node name `{name}` is not followed by `{{`", at + 1)));
    }
    if let Some(f) = stack.last() {
        return Err(err(f.open, format!("line {}: node `{}` is never closed", f.open + 1, f.name)));
    }
    Ok(finish(root))
}

fn count_blocks(items: &[Item]) -> usize {
    items
        .iter()
        .map(|i| match &i.kind {
            ItemKind::Block(b) => 1 + count_blocks(&b.items),
            ItemKind::Entry(_) => 0,
        })
        .sum()
}

fn count_entries(items: &[Item]) -> usize {
    items
        .iter()
        .map(|i| match &i.kind {
            ItemKind::Block(b) => count_entries(&b.items),
            ItemKind::Entry(_) => 1,
        })
        .sum()
}

fn last_entry(items: &[Item]) -> Option<usize> {
    items.iter().rev().find_map(|i| match i.kind {
        ItemKind::Entry(l) => Some(l),
        ItemKind::Block(_) => None,
    })
}

/// Indentation (leading spaces / tabs) of a line.
fn indent_of(text: &str) -> &str {
    &text[..text.len() - text.trim_start_matches(is_ws).len()]
}

// ---------------------------------------------------------------------------
// Document
// ---------------------------------------------------------------------------

/// A parsed ConfigNode document.
#[derive(Debug, Clone)]
pub struct ConfigNodeDocument {
    bom: bool,
    lines: Vec<Line>,
    tree: Vec<Item>,
}

/// The node (or the root) addressed by a path prefix.
struct Parent<'a> {
    items: &'a [Item],
    /// `None` for the root.
    open: Option<usize>,
}

impl ConfigNodeDocument {
    /// Parses ConfigNode text (must be UTF-8 with balanced braces).
    pub fn parse(bytes: &[u8]) -> CodecResult<ConfigNodeDocument> {
        let (bom, body) = match bytes.strip_prefix(BOM) {
            Some(rest) => (true, rest),
            None => (false, bytes),
        };
        let text = std::str::from_utf8(body).map_err(|e| {
            CodecError::malformed(
                CODEC,
                e.valid_up_to() + if bom { 3 } else { 0 },
                "ConfigNode text is not valid UTF-8",
            )
        })?;
        let lines = split_lines(text);
        let tree = Self::layout(&lines, bom)?;
        Ok(ConfigNodeDocument { bom, lines, tree })
    }

    fn layout(lines: &[Line], bom: bool) -> CodecResult<Vec<Item>> {
        let mut offsets = Vec::with_capacity(lines.len());
        let mut off = if bom { BOM.len() } else { 0 };
        for l in lines {
            offsets.push(off);
            off += l.text.len() + l.eol.len();
        }
        build_tree(lines, &offsets)
    }

    fn relayout(&mut self) -> CodecResult<()> {
        self.tree = Self::layout(&self.lines, self.bom)?;
        Ok(())
    }

    /// Serialises the document.
    pub fn serialize(&self) -> Vec<u8> {
        let cap = self.lines.iter().map(|l| l.text.len() + l.eol.len()).sum::<usize>() + BOM.len();
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

    /// Number of nodes in the file.
    pub fn node_count(&self) -> usize {
        count_blocks(&self.tree)
    }

    fn project_items(&self, items: &[Item]) -> Node {
        let mut m = NodeMap::with_capacity(items.len());
        for it in items {
            let v = match &it.kind {
                ItemKind::Entry(l) => match self.lines[*l].value_node() {
                    Some(v) => v,
                    None => continue,
                },
                ItemKind::Block(b) => self.project_items(&b.items),
            };
            m.insert(it.key.clone(), v);
        }
        Node::Map(m)
    }

    /// Semantic projection.
    pub fn project(&self) -> Node {
        self.project_items(&self.tree)
    }

    /// Resolves the node holding the last path segment.
    fn parent(&self, path: &NodePath) -> CodecResult<(Parent<'_>, String)> {
        let segs = path.segments();
        let Some((last, prefix)) = segs.split_last() else {
            return Err(CodecError::structure(CODEC, path, "the document root cannot be replaced"));
        };
        let mut cur = Parent { items: &self.tree, open: None };
        for seg in prefix {
            let PathSeg::Key(k) = seg else {
                return Err(CodecError::path_not_found(CODEC, path));
            };
            let block = cur
                .items
                .iter()
                .find_map(|i| match &i.kind {
                    ItemKind::Block(b) if &i.key == k => Some(b),
                    _ => None,
                })
                .ok_or_else(|| CodecError::path_not_found(CODEC, path))?;
            cur = Parent { items: &block.items, open: Some(block.open) };
        }
        match last {
            PathSeg::Key(k) => Ok((cur, k.clone())),
            _ => Err(CodecError::path_not_found(CODEC, path)),
        }
    }

    /// Line of the entry addressed by `path`.
    fn locate_entry(&self, path: &NodePath) -> CodecResult<usize> {
        let (parent, key) = self.parent(path)?;
        match parent.items.iter().find(|i| i.key == key).map(|i| &i.kind) {
            Some(ItemKind::Entry(l)) => Ok(*l),
            Some(ItemKind::Block(_)) => Err(CodecError::structure(
                CODEC,
                path,
                "ConfigNode nodes cannot be replaced, added or removed (only their entries can)",
            )),
            None => Err(CodecError::path_not_found(CODEC, path)),
        }
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

    /// Indentation unit of the file: the extra indentation of the first
    /// entry inside a node relative to the node's brace (tab by default).
    fn indent_unit(&self) -> String {
        fn find(doc: &ConfigNodeDocument, items: &[Item]) -> Option<String> {
            for it in items {
                if let ItemKind::Block(b) = &it.kind {
                    if let Some(l) = last_entry(&b.items) {
                        let outer = indent_of(&doc.lines[b.open].text);
                        let inner = indent_of(&doc.lines[l].text);
                        if let Some(unit) = inner.strip_prefix(outer) {
                            if !unit.is_empty() {
                                return Some(unit.to_string());
                            }
                        }
                    }
                    if let Some(u) = find(doc, &b.items) {
                        return Some(u);
                    }
                }
            }
            None
        }
        find(self, &self.tree).unwrap_or_else(|| "\t".to_string())
    }

    /// The `=` spacing of the first entry of the file (` = ` by default).
    fn default_separator(&self) -> String {
        self.lines
            .iter()
            .find_map(|l| l.entry().map(|e| l.text[e.key_end..e.value_start].to_string()))
            .unwrap_or_else(|| " = ".to_string())
    }

    /// Renders a value token, verifying that the line reads back as `target`.
    fn render_token(path: &NodePath, line_text: &str, entry: &Entry, target: &Node) -> CodecResult<String> {
        let old = &line_text[entry.value_start..entry.value_end];
        let tok = render_scalar(old, target).ok_or_else(|| {
            CodecError::unencodable(
                CODEC,
                path,
                format!("{} values cannot be written to a ConfigNode", target.kind().name()),
            )
        })?;
        if tok.contains(['\n', '\r']) {
            return Err(CodecError::unencodable(CODEC, path, "ConfigNode values cannot contain line breaks"));
        }
        let candidate = format!("{}{}{}", &line_text[..entry.value_start], tok, &line_text[entry.value_end..]);
        let reread = Line::new(candidate, String::new());
        let ok = reread.entry().is_some_and(|e| e.key == entry.key)
            && reread.value_node().is_some_and(|v| semantic_eq(&v, target));
        if !ok {
            let got = reread.value_node().map(|v| v.preview(40)).unwrap_or_else(|| "an unparsable line".into());
            return Err(CodecError::unencodable(
                CODEC,
                path,
                format!("`{tok}` would be read back as {got} (values are unquoted; `//` starts a comment and surrounding spaces are dropped)"),
            ));
        }
        Ok(tok)
    }

    /// Inserts `text` as a new line after line `after` (`None`: at the start).
    fn insert_line(&mut self, after: Option<usize>, text: String) {
        let eol = self.dominant_eol();
        let pos = after.map(|i| i + 1).unwrap_or(0);
        let mut new_eol = eol.clone();
        if let Some(i) = after {
            if self.lines[i].eol.is_empty() {
                // The previous last line had no newline: keep the file ending without one.
                self.lines[i].eol = eol;
                new_eol = String::new();
            }
        }
        self.lines.insert(pos, Line::new(text, new_eol));
    }

    /// Removes line `idx`, keeping a missing final newline missing.
    fn remove_line(&mut self, idx: usize) {
        let was_last_without_eol = idx + 1 == self.lines.len() && self.lines[idx].eol.is_empty();
        self.lines.remove(idx);
        if was_last_without_eol {
            if let Some(l) = self.lines.last_mut() {
                l.eol.clear();
            }
        }
    }
}

/// Key with a `#N` de-duplication suffix stripped when its base name already
/// exists in the node (so that `NAME#3` adds another `NAME`).
fn written_key<'k>(items: &[Item], key: &'k str) -> &'k str {
    if let Some((base, n)) = key.rsplit_once('#') {
        if !base.is_empty() && n.parse::<usize>().is_ok_and(|n| n >= 2) && items.iter().any(|i| i.key == base) {
            return base;
        }
    }
    key
}

impl PathEditable for ConfigNodeDocument {
    fn codec(&self) -> &'static str {
        CODEC
    }

    fn set_value(&mut self, path: &NodePath, value: &Node) -> CodecResult<()> {
        let idx = self.locate_entry(path)?;
        let line = &self.lines[idx];
        let entry = line.entry().cloned().ok_or_else(|| CodecError::path_not_found(CODEC, path))?;
        let existing = line.value_node().ok_or_else(|| CodecError::path_not_found(CODEC, path))?;
        let target = normalize_scalar(CODEC, path, &existing, value, true)?;
        let tok = Self::render_token(path, &line.text, &entry, &target)?;
        let text = format!("{}{}{}", &line.text[..entry.value_start], tok, &line.text[entry.value_end..]);
        let eol = line.eol.clone();
        self.lines[idx] = Line::new(text, eol);
        Ok(())
    }

    fn insert(&mut self, path: &NodePath, value: &Node) -> CodecResult<()> {
        if value.is_container() {
            return Err(CodecError::structure(
                CODEC,
                path,
                "ConfigNode nodes cannot be added (only entries with scalar values can)",
            ));
        }
        let (parent, key) = self.parent(path)?;
        let key = written_key(parent.items, &key).to_string();
        if key.is_empty()
            || key != key.trim_matches(is_ws)
            || key.contains(['=', '{', '}', '\n', '\r'])
            || key.contains("//")
        {
            return Err(CodecError::unencodable(CODEC, path, format!("`{key}` is not a valid ConfigNode key")));
        }
        let template = last_entry(parent.items);
        let (indent, sep) = match template.and_then(|i| self.lines.get(i).map(|l| (l, l.entry()))) {
            Some((l, Some(e))) => (l.text[..e.key_start].to_string(), l.text[e.key_end..e.value_start].to_string()),
            _ => {
                let indent = match parent.open {
                    Some(open) => format!("{}{}", indent_of(&self.lines[open].text), self.indent_unit()),
                    None => String::new(),
                };
                (indent, self.default_separator())
            }
        };
        let after = template.or(parent.open);
        let target = normalize_scalar(CODEC, path, &classify_scalar(""), value, true)?;
        let prefix = format!("{indent}{key}{sep}");
        let base = Entry {
            key: key.clone(),
            key_start: indent.len(),
            key_end: indent.len() + key.len(),
            value_start: prefix.len(),
            value_end: prefix.len(),
        };
        let tok = Self::render_token(path, &prefix, &base, &target)?;
        self.insert_line(after, format!("{prefix}{tok}"));
        self.relayout()
    }

    fn remove(&mut self, path: &NodePath) -> CodecResult<()> {
        let idx = self.locate_entry(path)?;
        self.remove_line(idx);
        self.relayout()
    }
}

impl Document for ConfigNodeDocument {
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
                "the edit cannot be represented exactly in a ConfigNode (repeated names shift `#N` suffixes)",
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
        n.push(format!("{} node(s), {} value(s)", count_blocks(&self.tree), count_entries(&self.tree)));
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

/// The Kerbal Space Program ConfigNode codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct ConfigNodeCodec;

impl StructuralCodec for ConfigNodeCodec {
    fn id(&self) -> &'static str {
        CODEC
    }

    fn name(&self) -> &'static str {
        "KSP ConfigNode"
    }

    fn detect(&self, bytes: &[u8], _ctx: &DetectContext) -> Confidence {
        let body = bytes.strip_prefix(BOM).unwrap_or(bytes);
        let Ok(text) = std::str::from_utf8(body) else { return 0 };
        if text.chars().any(|c| c.is_control() && !matches!(c, '\t' | '\r' | '\n')) {
            return 0;
        }
        match ConfigNodeDocument::parse(bytes) {
            Ok(doc) if doc.node_count() > 0 => 85,
            _ => 0,
        }
    }

    fn decode(&self, bytes: &[u8], _ctx: &DetectContext) -> CodecResult<Box<dyn Document>> {
        Ok(Box::new(ConfigNodeDocument::parse(bytes)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> NodePath {
        NodePath::parse(s).unwrap()
    }

    fn rt(src: &[u8]) -> ConfigNodeDocument {
        let doc = ConfigNodeDocument::parse(src).unwrap();
        assert_eq!(doc.encode().unwrap(), src);
        doc
    }

    const SAMPLE: &str = concat!(
        "// header comment\r\n",
        "GAME\r\n",
        "{\r\n",
        "\tversion = 1.12.5\r\n",
        "\tTitle = My Save (CAREER)\r\n",
        "\tlinkURL = \r\n",
        "\tformula = a = b + c\r\n",
        "\tFLIGHTSTATE\r\n",
        "\t{\r\n",
        "\t\tUT = 63290.1817719101\r\n",
        "\t\tactiveVessel = -1\r\n",
        "\t\tVESSEL\r\n",
        "\t\t{\r\n",
        "\t\t\tname = Rocket\r\n",
        "\t\t\tlanded = False\r\n",
        "\t\t\tPART\r\n",
        "\t\t\t{\r\n",
        "\t\t\t\tname = probeCore\r\n",
        "\t\t\t}\r\n",
        "\t\t\tPART\r\n",
        "\t\t\t{\r\n",
        "\t\t\t\tname = fuelTank // inline\r\n",
        "\t\t\t\tname = dup\r\n",
        "\t\t\t}\r\n",
        "\t\t}\r\n",
        "\t\tVESSEL {\r\n",
        "\t\t\tname = Probe\r\n",
        "\t\t}\r\n",
        "\t}\r\n",
        "\r\n",
        "\tSCENARIO\r\n",
        "\t{\r\n",
        "\t\tname = Funding\r\n",
        "\t\tfunds = 125000.5\r\n",
        "\t}\r\n",
        "}\r\n",
    );

    #[test]
    fn lossless_layouts() {
        rt(b"");
        rt(SAMPLE.as_bytes());
        rt(b"\xef\xbb\xbfA\n{\n  x = 1\n}");
        rt(b"ship = Untitled\nversion = 1.0\nPART\n{\n\tpart = a_1\n}\n");
        rt(b"A\n{\n}\nB\n// gap\n\n{\n  y=2\n}\n");
    }

    #[test]
    fn projection() {
        let doc = rt(SAMPLE.as_bytes());
        let ast = doc.to_ast();
        let g = |s: &str| ast.get(&p(s)).cloned().unwrap();
        assert_eq!(g("GAME.version"), Node::String("1.12.5".into()));
        assert_eq!(g("GAME.Title"), Node::String("My Save (CAREER)".into()));
        assert_eq!(g("GAME.linkURL"), Node::String(String::new()));
        assert_eq!(g("GAME.formula"), Node::String("a = b + c".into()));
        assert_eq!(g("GAME.FLIGHTSTATE.UT"), Node::F64(63290.1817719101));
        assert_eq!(g("GAME.FLIGHTSTATE.activeVessel"), Node::I64(-1));
        assert_eq!(g("GAME.FLIGHTSTATE.VESSEL.landed"), Node::Bool(false));
        assert_eq!(g("GAME.FLIGHTSTATE.VESSEL.PART.name"), Node::String("probeCore".into()));
        assert_eq!(g("GAME.FLIGHTSTATE.VESSEL[\"PART#2\"].name"), Node::String("fuelTank".into()));
        assert_eq!(g("GAME.FLIGHTSTATE.VESSEL[\"PART#2\"][\"name#2\"]"), Node::String("dup".into()));
        assert_eq!(g("GAME.FLIGHTSTATE[\"VESSEL#2\"].name"), Node::String("Probe".into()));
        assert_eq!(g("GAME.SCENARIO.funds"), Node::F64(125000.5));
        assert_eq!(doc.node_count(), 7);
    }

    #[test]
    fn value_edits_keep_layout() {
        let mut doc = rt(SAMPLE.as_bytes());
        let mut ast = doc.to_ast();
        ast.set(&p("GAME.SCENARIO.funds"), Node::F64(9_999_999.0), false).unwrap();
        ast.set(&p("GAME.FLIGHTSTATE.VESSEL.landed"), Node::Bool(true), false).unwrap();
        ast.set(&p("GAME.FLIGHTSTATE.VESSEL[\"PART#2\"].name"), Node::String("bigTank".into()), false).unwrap();
        ast.set(&p("GAME.linkURL"), Node::String("x=y".into()), false).unwrap();
        ast.set(&p("GAME.FLIGHTSTATE.activeVessel"), Node::I32(3), false).unwrap();
        doc.apply_ast(&ast).unwrap();
        let out = String::from_utf8(doc.encode().unwrap()).unwrap();
        let expected = SAMPLE
            .replace("funds = 125000.5\r", "funds = 9999999.0\r")
            .replace("landed = False", "landed = True")
            .replace("fuelTank // inline", "bigTank // inline")
            .replace("linkURL = \r", "linkURL = x=y\r")
            .replace("activeVessel = -1", "activeVessel = 3");
        assert_eq!(out, expected);
        assert!(semantic_eq(&ConfigNodeDocument::parse(out.as_bytes()).unwrap().to_ast(), &ast));
    }

    #[test]
    fn unrepresentable_edits_are_rejected() {
        let mut doc = rt(SAMPLE.as_bytes());
        for (path, v) in [
            ("GAME.Title", Node::String(" padded ".into())),
            ("GAME.Title", Node::String("a // b".into())),
            ("GAME.Title", Node::String("12".into())),
            ("GAME.Title", Node::String("two\nlines".into())),
            ("GAME.FLIGHTSTATE.UT", Node::F64(f64::NAN)),
        ] {
            let mut ast = doc.to_ast();
            ast.set(&p(path), v, false).unwrap();
            assert!(doc.apply_ast(&ast).is_err(), "{path}");
        }
        assert_eq!(doc.encode().unwrap(), SAMPLE.as_bytes());
    }

    #[test]
    fn entries_can_be_added_and_removed() {
        let mut doc = rt(SAMPLE.as_bytes());
        let mut ast = doc.to_ast();
        ast.set(&p("GAME.SCENARIO.science"), Node::F64(12.5), true).unwrap();
        ast.set(&p("GAME.FLIGHTSTATE[\"VESSEL#2\"][\"name#2\"]"), Node::String("Alias".into()), true).unwrap();
        ast.remove(&p("GAME.FLIGHTSTATE.activeVessel")).unwrap();
        doc.apply_ast(&ast).unwrap();
        let out = String::from_utf8(doc.encode().unwrap()).unwrap();
        let expected = SAMPLE
            .replace("\t\tfunds = 125000.5\r\n", "\t\tfunds = 125000.5\r\n\t\tscience = 12.5\r\n")
            .replace("\t\t\tname = Probe\r\n", "\t\t\tname = Probe\r\n\t\t\tname = Alias\r\n")
            .replace("\t\tactiveVessel = -1\r\n", "");
        assert_eq!(out, expected);

        // An entry in a node without entries uses the brace indentation + the file's unit.
        let mut doc = rt(b"A\n{\n    B\n    {\n    }\n    k = v\n}\n");
        let mut ast = doc.to_ast();
        ast.set(&p("A.B.x"), Node::I64(1), true).unwrap();
        doc.apply_ast(&ast).unwrap();
        assert_eq!(doc.encode().unwrap(), b"A\n{\n    B\n    {\n        x = 1\n    }\n    k = v\n}\n");
    }

    #[test]
    fn node_structure_changes_are_rejected() {
        let mut doc = rt(SAMPLE.as_bytes());
        let mut ast = doc.to_ast();
        ast.remove(&p("GAME.SCENARIO")).unwrap();
        assert!(matches!(doc.apply_ast(&ast), Err(CodecError::StructureUnsupported { .. })));
        let mut ast = doc.to_ast();
        ast.set(&p("GAME.NEWNODE"), Node::map_from([("a", Node::I64(1))]), true).unwrap();
        assert!(matches!(doc.apply_ast(&ast), Err(CodecError::StructureUnsupported { .. })));
        let mut ast = doc.to_ast();
        ast.set(&p("GAME.SCENARIO"), Node::I64(1), false).unwrap();
        assert!(doc.apply_ast(&ast).is_err());
        assert_eq!(doc.encode().unwrap(), SAMPLE.as_bytes());
    }

    #[test]
    fn malformed_structure_is_rejected() {
        for bad in [&b"A\n{\n x = 1\n"[..], b"}\n", b"{\n x = 1\n}\n", b"A\nx = 1\n", b"A { x = 1 }\n", b"A\n"] {
            assert!(ConfigNodeDocument::parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn truncated_or_corrupted_input_never_panics() {
        let b = SAMPLE.as_bytes();
        for n in 0..b.len() {
            if let Ok(doc) = ConfigNodeDocument::parse(&b[..n]) {
                assert_eq!(doc.encode().unwrap(), &b[..n]);
                let _ = doc.to_ast();
            }
        }
        for i in 0..b.len() {
            for x in [b'{', b'}', b'=', b'/', b'\n', 0xff] {
                let mut c = b.to_vec();
                c[i] = x;
                if let Ok(doc) = ConfigNodeDocument::parse(&c) {
                    assert_eq!(doc.encode().unwrap(), c);
                    let _ = doc.to_ast();
                }
            }
        }
    }

    #[test]
    fn detection() {
        let c = ConfigNodeCodec;
        let ctx = DetectContext::default();
        assert_eq!(c.detect(SAMPLE.as_bytes(), &ctx), 85);
        assert_eq!(c.detect(b"PART {\n name = x\n}\n", &ctx), 85);
        assert_eq!(c.detect(b"a = 1\nb = 2\n", &ctx), 0);
        assert_eq!(c.detect(b"[General]\nvolume=0.8\n", &ctx), 0);
        assert_eq!(c.detect(b"{\"a\": 1}", &ctx), 0);
        assert_eq!(c.detect(b"{\n  \"a\": 1\n}\n", &ctx), 0);
        assert_eq!(c.detect(b"just prose\n", &ctx), 0);
        assert_eq!(c.detect(&[0u8, 1, 2], &ctx), 0);
    }
}
