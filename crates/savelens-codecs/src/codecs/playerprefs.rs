//! Unity PlayerPrefs codec (Linux XML flavour).
//!
//! Unity on Linux stores `~/.config/unity3d/<Company>/<Product>/prefs` as
//!
//! ```xml
//! <unity_prefs version_major="1" version_minor="1">
//!     <pref name="KEY" type="int">42</pref>
//!     <pref name="Volume" type="float">0.75</pref>
//!     <pref name="Player" type="string">SGVybw==</pref>
//! </unity_prefs>
//! ```
//!
//! String values are Base64 of UTF-8 text. The codec uses a small dedicated
//! tokenizer that keeps every byte of the source: the prolog (XML
//! declaration, comments, BOM), the raw text of every tag (attribute order and
//! quoting), the whitespace / line endings between elements and the raw
//! spelling of every value. Only the text content of edited prefs is
//! re-rendered.
//!
//! Projection: a map from pref name to `I32` (`int`), `F32` (`float`) or
//! `String` (`string`: the Base64-decoded text when the value is valid Base64
//! of UTF-8 whose re-encoding reproduces the original exactly, otherwise the
//! raw text). Values that do not parse as their declared type, and prefs with
//! an unknown type, are projected as their (unescaped) text. Duplicate names
//! are disambiguated as `name#2`, `name#3`, …

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use savelens_core::{Node, NodeMap, NodePath, PathSeg};

use crate::document::{
    apply_via_diff, coerce_like, Capabilities, Confidence, DetectContext, Document, PathEditable, StructuralCodec,
};
use crate::error::{CodecError, CodecResult};

const CODEC: &str = "playerprefs";
const ROOT: &str = "unity_prefs";

/// How a pref's value is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueKind {
    Int,
    Float,
    /// Base64-encoded UTF-8 string.
    Base64,
    /// Plain (unescaped) text: unknown type, unparsable number or a string
    /// that is not canonical Base64.
    Text,
}

/// One `<pref>` element.
#[derive(Debug, Clone)]
struct Pref {
    /// Raw start tag, from `<` to `>` inclusive.
    start_tag: String,
    /// The start tag ends with `/>` (no content, no end tag).
    self_closing: bool,
    /// Raw text content (still XML-escaped).
    raw_value: String,
    /// Raw end tag (`</pref>` possibly with whitespace).
    end_tag: String,
    name: String,
    kind: ValueKind,
    /// Projected value.
    value: Node,
}

/// Content of the root element.
#[derive(Debug, Clone)]
enum Item {
    /// Whitespace, comments or processing instructions between prefs.
    Misc(String),
    Pref(Box<Pref>),
}

/// A parsed PlayerPrefs file.
#[derive(Debug, Clone)]
pub struct PlayerPrefsDocument {
    /// Everything before the root start tag (BOM, declaration, comments).
    prolog: String,
    root_start: String,
    items: Vec<Item>,
    root_end: String,
    /// Everything after the root end tag.
    epilog: String,
}

// ---------------------------------------------------------------------------
// XML helpers
// ---------------------------------------------------------------------------

fn unescape(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let end = after.find(';')?;
        let ent = &after[..end];
        let ch = match ent {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let code = if let Some(hex) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                    u32::from_str_radix(hex, 16).ok()?
                } else {
                    ent.strip_prefix('#')?.parse::<u32>().ok()?
                };
                char::from_u32(code)?
            }
        };
        out.push(ch);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':')
}

/// Parsed start tag: element name, attributes (unescaped) and whether it is
/// self-closing.
struct Tag {
    name: String,
    attrs: Vec<(String, String)>,
    self_closing: bool,
}

fn parse_tag(raw: &str, offset: usize) -> CodecResult<Tag> {
    let err = |r: &str| CodecError::malformed(CODEC, offset, r.to_string());
    let inner =
        raw.strip_prefix('<').and_then(|s| s.strip_suffix('>')).ok_or_else(|| err("tag is not delimited by < >"))?;
    let (inner, self_closing) = match inner.strip_suffix('/') {
        Some(s) => (s, true),
        None => (inner, false),
    };
    let name_end = inner.find(|c: char| !is_name_char(c)).unwrap_or(inner.len());
    let name = inner[..name_end].to_string();
    if name.is_empty() {
        return Err(err("element without a name"));
    }
    let mut rest = &inner[name_end..];
    let mut attrs = Vec::new();
    loop {
        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            break;
        }
        if trimmed.len() == rest.len() {
            return Err(err("attributes must be separated by whitespace"));
        }
        let key_end = trimmed.find(|c: char| !is_name_char(c)).unwrap_or(trimmed.len());
        if key_end == 0 {
            return Err(err("invalid attribute name"));
        }
        let key = &trimmed[..key_end];
        let after_key = trimmed[key_end..].trim_start();
        let after_eq = after_key.strip_prefix('=').ok_or_else(|| err("attribute without `=`"))?.trim_start();
        let quote = after_eq
            .chars()
            .next()
            .filter(|c| *c == '"' || *c == '\'')
            .ok_or_else(|| err("unquoted attribute value"))?;
        let body = &after_eq[1..];
        let close = body.find(quote).ok_or_else(|| err("unterminated attribute value"))?;
        let value = unescape(&body[..close]).ok_or_else(|| err("invalid entity in attribute value"))?;
        attrs.push((key.to_string(), value));
        rest = &body[close + 1..];
    }
    Ok(Tag { name, attrs, self_closing })
}

/// Shortest decimal text that parses back to the same `f32`.
fn render_f32(v: f32) -> Option<String> {
    if !v.is_finite() {
        return None;
    }
    Some(format!("{v}"))
}

fn classify(type_attr: &str, text: &str) -> (ValueKind, Node) {
    match type_attr {
        "int" => match text.parse::<i32>() {
            Ok(i) => (ValueKind::Int, Node::I32(i)),
            Err(_) => (ValueKind::Text, Node::String(text.to_string())),
        },
        "float" => match text.parse::<f32>() {
            Ok(f) if f.is_finite() => (ValueKind::Float, Node::F32(f)),
            _ => (ValueKind::Text, Node::String(text.to_string())),
        },
        "string" => {
            let decoded = STANDARD
                .decode(text)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .filter(|s| STANDARD.encode(s.as_bytes()) == text);
            match decoded {
                Some(s) => (ValueKind::Base64, Node::String(s)),
                None => (ValueKind::Text, Node::String(text.to_string())),
            }
        }
        _ => (ValueKind::Text, Node::String(text.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Tokenizer / parser
// ---------------------------------------------------------------------------

struct Scanner<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Scanner<'a> {
    fn err(&self, reason: impl Into<String>) -> CodecError {
        CodecError::malformed(CODEC, self.pos, reason)
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    /// Consumes up to and including `end`.
    fn until(&mut self, end: &str, what: &str) -> CodecResult<&'a str> {
        let start = self.pos;
        let i = self.rest().find(end).ok_or_else(|| self.err(format!("unterminated {what}")))?;
        self.pos += i + end.len();
        Ok(&self.src[start..self.pos])
    }

    /// Consumes whitespace, comments, processing instructions and (in the
    /// prolog) a DOCTYPE.
    fn misc(&mut self, allow_doctype: bool) -> CodecResult<&'a str> {
        let start = self.pos;
        loop {
            let rest = self.rest();
            let trimmed = rest.trim_start_matches([' ', '\t', '\r', '\n']);
            self.pos += rest.len() - trimmed.len();
            if trimmed.starts_with("<!--") {
                self.until("-->", "comment")?;
            } else if trimmed.starts_with("<?") {
                self.until("?>", "processing instruction")?;
            } else if allow_doctype && trimmed.starts_with("<!DOCTYPE") {
                self.until(">", "DOCTYPE")?;
            } else {
                break;
            }
        }
        Ok(&self.src[start..self.pos])
    }

    /// Reads a raw tag starting at `<` (attribute values may contain `>`).
    fn raw_tag(&mut self) -> CodecResult<&'a str> {
        let start = self.pos;
        let bytes = self.src.as_bytes();
        if bytes.get(self.pos) != Some(&b'<') {
            return Err(self.err("expected a tag"));
        }
        let mut quote: Option<u8> = None;
        let mut i = self.pos + 1;
        while i < bytes.len() {
            let b = bytes[i];
            match quote {
                Some(q) if b == q => quote = None,
                Some(_) => {}
                None if b == b'"' || b == b'\'' => quote = Some(b),
                None if b == b'>' => {
                    self.pos = i + 1;
                    return Ok(&self.src[start..self.pos]);
                }
                None if b == b'<' => break,
                None => {}
            }
            i += 1;
        }
        Err(self.err("unterminated tag"))
    }
}

impl PlayerPrefsDocument {
    /// Parses PlayerPrefs XML.
    pub fn parse(bytes: &[u8]) -> CodecResult<PlayerPrefsDocument> {
        let src = std::str::from_utf8(bytes)
            .map_err(|e| CodecError::malformed(CODEC, e.valid_up_to(), "file is not valid UTF-8"))?;
        let mut sc = Scanner { src, pos: 0 };
        if sc.rest().starts_with('\u{feff}') {
            sc.pos += '\u{feff}'.len_utf8();
        }
        sc.misc(true)?;
        let prolog = src[..sc.pos].to_string();
        let root_at = sc.pos;
        let root_start = sc.raw_tag()?.to_string();
        let tag = parse_tag(&root_start, root_at)?;
        if tag.name != ROOT {
            return Err(CodecError::malformed(CODEC, root_at, format!("root element is <{}>, not <{ROOT}>", tag.name)));
        }
        let mut items = Vec::new();
        let root_end;
        if tag.self_closing {
            root_end = String::new();
        } else {
            loop {
                let misc = sc.misc(false)?;
                if !misc.is_empty() {
                    items.push(Item::Misc(misc.to_string()));
                }
                let at = sc.pos;
                if sc.rest().starts_with("</") {
                    let end = sc.raw_tag()?;
                    let name = end[2..end.len() - 1].trim_end();
                    if name != ROOT {
                        return Err(CodecError::malformed(CODEC, at, format!("unexpected end tag {end}")));
                    }
                    root_end = end.to_string();
                    break;
                }
                if sc.rest().is_empty() {
                    return Err(sc.err(format!("missing </{ROOT}>")));
                }
                if !sc.rest().starts_with('<') {
                    return Err(sc.err("unexpected text inside <unity_prefs>"));
                }
                items.push(Item::Pref(Box::new(Self::parse_pref(&mut sc)?)));
            }
        }
        let epilog = sc.rest().to_string();
        if !epilog.trim_matches([' ', '\t', '\r', '\n', '\0']).is_empty() {
            return Err(CodecError::malformed(CODEC, sc.pos, "content after the root element"));
        }
        Ok(PlayerPrefsDocument { prolog, root_start, items, root_end, epilog })
    }

    fn parse_pref(sc: &mut Scanner<'_>) -> CodecResult<Pref> {
        let at = sc.pos;
        let start_tag = sc.raw_tag()?.to_string();
        let tag = parse_tag(&start_tag, at)?;
        if tag.name != "pref" {
            return Err(CodecError::malformed(CODEC, at, format!("unexpected element <{}>", tag.name)));
        }
        let attr = |k: &str| tag.attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let name = attr("name").ok_or_else(|| CodecError::malformed(CODEC, at, "<pref> without a name attribute"))?;
        let type_attr = attr("type").unwrap_or_default();
        let (raw_value, end_tag) = if tag.self_closing {
            (String::new(), String::new())
        } else {
            let vstart = sc.pos;
            let len = sc.rest().find('<').ok_or_else(|| sc.err("unterminated <pref>"))?;
            sc.pos += len;
            let raw = sc.src[vstart..sc.pos].to_string();
            let eat = sc.pos;
            if !sc.rest().starts_with("</") {
                return Err(sc.err("nested elements inside <pref> are not supported"));
            }
            let end = sc.raw_tag()?;
            if end[2..end.len() - 1].trim_end() != "pref" {
                return Err(CodecError::malformed(CODEC, eat, format!("unexpected end tag {end}")));
            }
            (raw, end.to_string())
        };
        let text =
            unescape(&raw_value).ok_or_else(|| CodecError::malformed(CODEC, at, "invalid entity in pref value"))?;
        let (kind, value) = classify(&type_attr, &text);
        Ok(Pref { start_tag, self_closing: tag.self_closing, raw_value, end_tag, name, kind, value })
    }

    /// Serialises the document.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = String::new();
        out.push_str(&self.prolog);
        out.push_str(&self.root_start);
        for item in &self.items {
            match item {
                Item::Misc(s) => out.push_str(s),
                Item::Pref(p) => {
                    out.push_str(&p.start_tag);
                    out.push_str(&p.raw_value);
                    out.push_str(&p.end_tag);
                }
            }
        }
        out.push_str(&self.root_end);
        out.push_str(&self.epilog);
        out.into_bytes()
    }

    fn prefs(&self) -> impl Iterator<Item = &Pref> {
        self.items.iter().filter_map(|i| match i {
            Item::Pref(p) => Some(&**p),
            Item::Misc(_) => None,
        })
    }

    /// AST keys in pref order (duplicates disambiguated).
    fn keys(&self) -> Vec<String> {
        let mut map = NodeMap::new();
        let mut keys = Vec::new();
        for p in self.prefs() {
            let key = if map.contains_key(&p.name) {
                (2..).map(|n| format!("{}#{n}", p.name)).find(|k| !map.contains_key(k)).unwrap_or_default()
            } else {
                p.name.clone()
            };
            map.insert(key.clone(), Node::Null);
            keys.push(key);
        }
        keys
    }

    fn project(&self) -> Node {
        Node::Map(self.keys().into_iter().zip(self.prefs()).map(|(k, p)| (k, p.value.clone())).collect())
    }

    fn pref_mut(&mut self, key: &str) -> Option<&mut Pref> {
        let pos = self.keys().iter().position(|k| k == key)?;
        self.items
            .iter_mut()
            .filter_map(|i| match i {
                Item::Pref(p) => Some(&mut **p),
                Item::Misc(_) => None,
            })
            .nth(pos)
    }
}

impl Pref {
    fn set(&mut self, path: &NodePath, value: &Node) -> CodecResult<()> {
        let v = coerce_like(CODEC, path, &self.value, value)?;
        let text = match (self.kind, &v) {
            (ValueKind::Int, Node::I32(i)) => i.to_string(),
            (ValueKind::Float, Node::F32(f)) => render_f32(*f)
                .ok_or_else(|| CodecError::unencodable(CODEC, path, "PlayerPrefs floats must be finite"))?,
            (ValueKind::Base64, Node::String(s)) => STANDARD.encode(s.as_bytes()),
            (ValueKind::Text, Node::String(s)) => escape_text(s),
            (_, other) => {
                return Err(CodecError::type_mismatch(CODEC, path, self.value.kind().name(), other.kind().name()))
            }
        };
        if self.self_closing && !text.is_empty() {
            let body = self.start_tag.strip_suffix("/>").unwrap_or(&self.start_tag).trim_end();
            self.start_tag = format!("{body}>");
            self.end_tag = "</pref>".into();
            self.self_closing = false;
        }
        self.raw_value = text;
        self.value = v;
        Ok(())
    }
}

impl PathEditable for PlayerPrefsDocument {
    fn codec(&self) -> &'static str {
        CODEC
    }

    fn set_value(&mut self, path: &NodePath, value: &Node) -> CodecResult<()> {
        let key = match path.segments() {
            [PathSeg::Key(k)] => k.clone(),
            _ => return Err(CodecError::structure(CODEC, path, "PlayerPrefs is a flat map of scalar values")),
        };
        let pref = self.pref_mut(&key).ok_or_else(|| CodecError::path_not_found(CODEC, path))?;
        pref.set(path, value)
    }
}

impl Document for PlayerPrefsDocument {
    fn codec_id(&self) -> &'static str {
        CODEC
    }

    fn to_ast(&self) -> Node {
        self.project()
    }

    fn apply_ast(&mut self, root: &Node) -> CodecResult<()> {
        let current = self.project();
        let mut work = self.clone();
        apply_via_diff(&mut work, &current, root)?;
        *self = work;
        Ok(())
    }

    fn encode(&self) -> CodecResult<Vec<u8>> {
        Ok(self.serialize())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::VALUES_ONLY
    }

    fn notes(&self) -> Vec<String> {
        let total = self.prefs().count();
        let text = self.prefs().filter(|p| p.kind == ValueKind::Text).count();
        let mut n = vec![format!("{total} PlayerPrefs entries")];
        if text > 0 {
            n.push(format!("{text} value(s) shown as raw text (unknown type, unparsable number or non-Base64 string)"));
        }
        let mut seen = std::collections::HashSet::new();
        let dups = self.prefs().filter(|p| !seen.insert(p.name.as_str())).count();
        if dups > 0 {
            n.push(format!("{dups} duplicate pref name(s) shown as name#N"));
        }
        n
    }

    fn clone_box(&self) -> Box<dyn Document> {
        Box::new(self.clone())
    }
}

/// The Unity PlayerPrefs codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlayerPrefsCodec;

impl StructuralCodec for PlayerPrefsCodec {
    fn id(&self) -> &'static str {
        CODEC
    }

    fn name(&self) -> &'static str {
        "Unity PlayerPrefs (XML)"
    }

    fn detect(&self, bytes: &[u8], _ctx: &DetectContext) -> Confidence {
        let body = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
        if body.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'<')
            || !body.windows(ROOT.len()).any(|w| w == ROOT.as_bytes())
        {
            return 0;
        }
        if PlayerPrefsDocument::parse(bytes).is_ok() {
            95
        } else {
            0
        }
    }

    fn decode(&self, bytes: &[u8], _ctx: &DetectContext) -> CodecResult<Box<dyn Document>> {
        Ok(Box::new(PlayerPrefsDocument::parse(bytes)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "<unity_prefs version_major=\"1\" version_minor=\"1\">\n\t<pref name=\"Gold\" type=\"int\">1500</pref>\n\t<pref name='Volume' type='float'>0.3</pref>\n\t<pref name=\"Hero\" type=\"string\">SMOpcm9z</pref>\n\t<pref name=\"a&amp;b\" type=\"string\">not base64!</pref>\n\t<pref name=\"Empty\" type=\"string\"/>\n</unity_prefs>\n";

    fn path(k: &str) -> NodePath {
        NodePath::root().key(k)
    }

    #[test]
    fn roundtrip_and_projection() {
        let doc = PlayerPrefsDocument::parse(SRC.as_bytes()).unwrap();
        assert_eq!(doc.serialize(), SRC.as_bytes());
        let ast = doc.to_ast();
        assert_eq!(ast.get(&path("Gold")), Some(&Node::I32(1500)));
        assert_eq!(ast.get(&path("Volume")), Some(&Node::F32(0.3)));
        assert_eq!(ast.get(&path("Hero")), Some(&Node::String("H\u{e9}ros".into())));
        assert_eq!(ast.get(&path("a&b")), Some(&Node::String("not base64!".into())));
        assert_eq!(ast.get(&path("Empty")), Some(&Node::String(String::new())));
    }

    #[test]
    fn edits() {
        let mut doc = PlayerPrefsDocument::parse(SRC.as_bytes()).unwrap();
        let mut ast = doc.to_ast();
        ast.set(&path("Gold"), Node::I64(-7), false).unwrap();
        ast.set(&path("Volume"), Node::F64(0.1), false).unwrap();
        ast.set(&path("Hero"), Node::String("Zo\u{eb}".into()), false).unwrap();
        ast.set(&path("a&b"), Node::String("x<y>&z".into()), false).unwrap();
        ast.set(&path("Empty"), Node::String("hi".into()), false).unwrap();
        doc.apply_ast(&ast).unwrap();
        let out = String::from_utf8(doc.serialize()).unwrap();
        assert!(out.contains("type=\"int\">-7</pref>"));
        assert!(out.contains("type='float'>0.1</pref>"));
        assert!(out.contains(&format!(">{}</pref>", STANDARD.encode("Zo\u{eb}"))));
        assert!(out.contains(">x&lt;y&gt;&amp;z</pref>"));
        assert!(out.contains("<pref name=\"Empty\" type=\"string\">aGk=</pref>"));
        let re = PlayerPrefsDocument::parse(out.as_bytes()).unwrap();
        // The F64 / I64 edits were coerced to the fields' F32 / I32 types.
        let mut expected = ast;
        expected.set(&path("Gold"), Node::I32(-7), false).unwrap();
        expected.set(&path("Volume"), Node::F32(0.1), false).unwrap();
        assert_eq!(re.to_ast(), expected);
        assert_eq!(doc.to_ast(), expected);
    }

    #[test]
    fn rejects_bad_edits_and_input() {
        let mut doc = PlayerPrefsDocument::parse(SRC.as_bytes()).unwrap();
        let mut ast = doc.to_ast();
        ast.set(&path("Gold"), Node::I64(1 << 40), false).unwrap();
        assert!(doc.apply_ast(&ast).is_err());
        let mut ast = doc.to_ast();
        ast.set(&path("Volume"), Node::F32(f32::INFINITY), false).unwrap();
        assert!(doc.apply_ast(&ast).is_err());
        let mut ast = doc.to_ast();
        ast.set(&path("New"), Node::I32(1), true).unwrap();
        assert!(doc.apply_ast(&ast).is_err());
        assert_eq!(doc.serialize(), SRC.as_bytes());

        assert!(PlayerPrefsDocument::parse(b"<other/>").is_err());
        assert!(PlayerPrefsDocument::parse(b"<unity_prefs><pref name=\"a\" type=\"int\">1</pref>").is_err());
        assert!(PlayerPrefsDocument::parse(b"<unity_prefs><pref type=\"int\">1</pref></unity_prefs>").is_err());
        assert!(PlayerPrefsDocument::parse(b"<unity_prefs><pref name=\"a\"><b/></pref></unity_prefs>").is_err());
        for n in 0..SRC.len() {
            let _ = PlayerPrefsDocument::parse(&SRC.as_bytes()[..n]);
        }
    }

    #[test]
    fn helpers() {
        assert_eq!(unescape("a&lt;&#65;&#x42;&quot;").as_deref(), Some("a<AB\""));
        assert!(unescape("&bogus;").is_none());
        assert_eq!(render_f32(0.1).as_deref(), Some("0.1"));
        assert_eq!(render_f32(3.0).as_deref(), Some("3"));
        assert_eq!(render_f32(1e-7).unwrap().parse::<f32>().unwrap(), 1e-7);
        assert!(render_f32(f32::NAN).is_none());
    }

    #[test]
    fn detect() {
        let c = PlayerPrefsCodec;
        let ctx = DetectContext::default();
        assert_eq!(c.detect(SRC.as_bytes(), &ctx), 95);
        assert_eq!(c.detect(b"<?xml version=\"1.0\"?>\n<unity_prefs/>", &ctx), 95);
        assert_eq!(c.detect(b"<root/>", &ctx), 0);
        assert_eq!(c.detect(b"{}", &ctx), 0);
    }
}
