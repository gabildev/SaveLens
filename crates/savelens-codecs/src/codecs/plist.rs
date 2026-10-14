//! Apple XML property-list codec.
//!
//! Built on the lossless XML tree of [`super::xml`]: every byte of the source
//! (declaration, DOCTYPE, indentation, comments) is kept, and edits only touch
//! the text of the edited value element. `<true/>` ↔ `<false/>` edits rename
//! the element; `<data>` blobs keep their original base64 line wrapping when
//! untouched and are re-wrapped with the same width and indentation when
//! edited.

use std::collections::HashSet;

use base64::Engine;
use savelens_core::{Node, NodeMap, NodePath, PathSeg};

use super::ini::{dedup_keys, normalize_scalar, render_float, semantic_eq};
use super::xml::{starts_like_xml, Element, XNode, XmlTree, MAX_DEPTH};
use crate::document::{apply_via_diff, coerce_like, Capabilities, Confidence, DetectContext, Document, PathEditable, StructuralCodec};
use crate::error::{CodecError, CodecResult};

const CODEC: &str = "plist";

fn b64() -> &'static base64::engine::GeneralPurpose {
    &base64::engine::general_purpose::STANDARD
}

fn is_xml_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

/// Fails when an element holds non-whitespace text (only structure allowed).
fn no_text(e: &Element) -> CodecResult<()> {
    let bad = e.children.iter().any(|c| match c {
        XNode::Text(t) => !t.chars().all(is_xml_ws),
        XNode::CData(c) => !c.chars().all(is_xml_ws),
        _ => false,
    });
    if bad {
        return Err(CodecError::malformed(CODEC, 0, format!("unexpected text inside <{}>", e.name)));
    }
    Ok(())
}

/// The single value element inside `<plist>`.
fn top_value(root: &Element) -> CodecResult<usize> {
    if root.name != "plist" {
        return Err(CodecError::malformed(CODEC, 0, format!("root element is <{}>, not <plist>", root.name)));
    }
    no_text(root)?;
    match root.child_elements().count() {
        1 => Ok(0),
        n => Err(CodecError::malformed(CODEC, 0, format!("<plist> must contain exactly one value, found {n}"))),
    }
}

/// `(key, value element index)` pairs of a `<dict>`, keys de-duplicated.
fn dict_entries(e: &Element) -> CodecResult<Vec<(String, usize)>> {
    no_text(e)?;
    let kids: Vec<&Element> = e.child_elements().collect();
    if kids.len() % 2 != 0 {
        return Err(CodecError::malformed(CODEC, 0, "<dict> has a key without a value"));
    }
    let mut raw_keys = Vec::with_capacity(kids.len() / 2);
    for pair in kids.chunks(2) {
        if pair[0].name != "key" {
            return Err(CodecError::malformed(
                CODEC,
                0,
                format!("<dict> expects <key>, found <{}>", pair[0].name),
            ));
        }
        if pair[1].name == "key" {
            return Err(CodecError::malformed(CODEC, 0, "<dict> has two consecutive <key> elements"));
        }
        raw_keys.push(pair[0].text());
    }
    let mut seen = HashSet::new();
    let keys = dedup_keys(raw_keys, &mut seen);
    Ok(keys.into_iter().enumerate().map(|(i, k)| (k, 2 * i + 1)).collect())
}

fn data_bytes(e: &Element) -> CodecResult<Vec<u8>> {
    let compact: String = e.text().chars().filter(|c| !c.is_ascii_whitespace()).collect();
    b64()
        .decode(compact.as_bytes())
        .map_err(|err| CodecError::malformed(CODEC, 0, format!("<data> is not valid base64: {err}")))
}

fn project(e: &Element, depth: usize) -> CodecResult<Node> {
    if depth > MAX_DEPTH {
        return Err(CodecError::malformed(CODEC, 0, "property list nested too deeply"));
    }
    Ok(match e.name.as_str() {
        "dict" => {
            let entries = dict_entries(e)?;
            let kids: Vec<&Element> = e.child_elements().collect();
            let mut m = NodeMap::with_capacity(entries.len());
            for (k, i) in entries {
                m.insert(k, project(kids[i], depth + 1)?);
            }
            Node::Map(m)
        }
        "array" => {
            no_text(e)?;
            Node::Array(e.child_elements().map(|c| project(c, depth + 1)).collect::<CodecResult<Vec<_>>>()?)
        }
        "integer" => {
            no_children(e)?;
            let t = e.text();
            let t = t.trim();
            match t.parse::<i64>() {
                Ok(v) => Node::I64(v),
                Err(_) => Node::U64(
                    t.parse::<u64>()
                        .map_err(|_| CodecError::malformed(CODEC, 0, format!("invalid <integer> `{t}`")))?,
                ),
            }
        }
        "real" => {
            no_children(e)?;
            let t = e.text();
            Node::F64(
                t.trim()
                    .parse::<f64>()
                    .map_err(|_| CodecError::malformed(CODEC, 0, format!("invalid <real> `{}`", t.trim())))?,
            )
        }
        "string" | "date" => {
            no_children(e)?;
            Node::String(e.text())
        }
        "true" | "false" => {
            no_children(e)?;
            no_text(e)?;
            Node::Bool(e.name == "true")
        }
        "data" => {
            no_children(e)?;
            Node::Bytes(data_bytes(e)?)
        }
        other => return Err(CodecError::malformed(CODEC, 0, format!("unknown property-list element <{other}>"))),
    })
}

fn no_children(e: &Element) -> CodecResult<()> {
    if e.child_elements().next().is_some() {
        return Err(CodecError::malformed(CODEC, 0, format!("<{}> cannot contain elements", e.name)));
    }
    Ok(())
}

/// Re-encodes `bytes` following the layout of the original `<data>` text:
/// leading/trailing whitespace, line width, line ending and indentation.
fn layout_base64(old_text: &str, bytes: &[u8]) -> String {
    let encoded = b64().encode(bytes);
    let lead_len = old_text.len() - old_text.trim_start_matches(is_xml_ws).len();
    let trail_len = old_text.len() - old_text.trim_end_matches(is_xml_ws).len();
    if lead_len == old_text.len() {
        return format!("{old_text}{encoded}");
    }
    let lead = &old_text[..lead_len];
    let trail = &old_text[old_text.len() - trail_len..];
    let body = &old_text[lead_len..old_text.len() - trail_len];
    let lines: Vec<&str> = body.split('\n').collect();
    if lines.len() < 2 {
        return format!("{lead}{encoded}{trail}");
    }
    let first = lines[0];
    let (first, eol) = match first.strip_suffix('\r') {
        Some(f) => (f, "\r\n"),
        None => (first, "\n"),
    };
    let width = first.trim_end_matches(is_xml_ws).len().max(4);
    let second = lines[1];
    let indent = &second[..second.len() - second.trim_start_matches([' ', '\t']).len()];
    let chunks: Vec<&str> = encoded
        .as_bytes()
        .chunks(width)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect();
    format!("{lead}{}{trail}", chunks.join(&format!("{eol}{indent}")))
}

fn set_scalar(e: &mut Element, path: &NodePath, value: &Node) -> CodecResult<()> {
    match e.name.as_str() {
        "dict" | "array" => Err(CodecError::structure(CODEC, path, "containers cannot be replaced; edit their values instead")),
        "integer" => {
            let old = e.text();
            let existing = project(e, 0)?;
            let target = coerce_like(CODEC, path, &existing, value)?;
            let text = match &target {
                Node::I64(v) => v.to_string(),
                Node::U64(v) => v.to_string(),
                other => return Err(CodecError::type_mismatch(CODEC, path, "integer", other.kind().name())),
            };
            e.set_text(&keep_padding(&old, &text));
            Ok(())
        }
        "real" => {
            let old = e.text();
            let target = coerce_like(CODEC, path, &Node::F64(0.0), value)?;
            let Node::F64(x) = target else {
                return Err(CodecError::type_mismatch(CODEC, path, "real", target.kind().name()));
            };
            let text = if x.is_nan() {
                "nan".to_string()
            } else if x.is_infinite() {
                if x > 0.0 { "+infinity" } else { "-infinity" }.to_string()
            } else {
                render_float(old.trim(), x).ok_or_else(|| CodecError::unencodable(CODEC, path, format!("cannot write {x} as <real>")))?
            };
            e.set_text(&keep_padding(&old, &text));
            Ok(())
        }
        "string" | "date" => {
            let existing = Node::String(e.text());
            let target = normalize_scalar(CODEC, path, &existing, value, false)?;
            let Node::String(s) = target else {
                return Err(CodecError::type_mismatch(CODEC, path, "string", target.kind().name()));
            };
            e.set_text(&s);
            Ok(())
        }
        "true" | "false" => {
            let target = coerce_like(CODEC, path, &Node::Bool(e.name == "true"), value)?;
            let Node::Bool(b) = target else {
                return Err(CodecError::type_mismatch(CODEC, path, "bool", target.kind().name()));
            };
            e.name = if b { "true" } else { "false" }.to_string();
            Ok(())
        }
        "data" => {
            let target = coerce_like(CODEC, path, &Node::Bytes(Vec::new()), value)?;
            let Node::Bytes(bytes) = target else {
                return Err(CodecError::type_mismatch(CODEC, path, "bytes", target.kind().name()));
            };
            let old = e.text();
            e.set_text(&layout_base64(&old, &bytes));
            Ok(())
        }
        other => Err(CodecError::malformed(CODEC, 0, format!("unknown property-list element <{other}>"))),
    }
}

/// Keeps whitespace that surrounded the old text around the new one.
fn keep_padding(old: &str, new: &str) -> String {
    let t = old.trim_matches(is_xml_ws);
    if t.is_empty() {
        return new.to_string();
    }
    let lead = &old[..old.len() - old.trim_start_matches(is_xml_ws).len()];
    let trail = &old[old.trim_end_matches(is_xml_ws).len()..];
    format!("{lead}{new}{trail}")
}

fn set_in(e: &mut Element, segs: &[PathSeg], full: &NodePath, value: &Node) -> CodecResult<()> {
    let Some((first, rest)) = segs.split_first() else {
        return set_scalar(e, full, value);
    };
    let idx = match (e.name.as_str(), first) {
        ("dict", PathSeg::Key(k)) => dict_entries(e)?
            .into_iter()
            .find(|(x, _)| x == k)
            .map(|(_, i)| i)
            .ok_or_else(|| CodecError::path_not_found(CODEC, full))?,
        ("array", PathSeg::Index(i)) => *i,
        _ => return Err(CodecError::path_not_found(CODEC, full)),
    };
    let child = e.child_element_mut(idx).ok_or_else(|| CodecError::path_not_found(CODEC, full))?;
    set_in(child, rest, full, value)
}

/// A parsed XML property list.
#[derive(Debug, Clone)]
pub struct PlistDocument {
    tree: XmlTree,
}

impl PlistDocument {
    /// Parses and validates an XML property list.
    pub fn parse(bytes: &[u8]) -> CodecResult<PlistDocument> {
        let tree = XmlTree::parse(bytes, CODEC)?;
        let doc = PlistDocument { tree };
        doc.project()?;
        Ok(doc)
    }

    fn value_element(&self) -> CodecResult<&Element> {
        let i = top_value(&self.tree.root)?;
        self.tree
            .root
            .child_elements()
            .nth(i)
            .ok_or_else(|| CodecError::malformed(CODEC, 0, "<plist> is empty"))
    }

    fn project(&self) -> CodecResult<Node> {
        project(self.value_element()?, 0)
    }
}

impl PathEditable for PlistDocument {
    fn codec(&self) -> &'static str {
        CODEC
    }

    fn set_value(&mut self, path: &NodePath, value: &Node) -> CodecResult<()> {
        let i = top_value(&self.tree.root)?;
        let top = self
            .tree
            .root
            .child_element_mut(i)
            .ok_or_else(|| CodecError::malformed(CODEC, 0, "<plist> is empty"))?;
        set_in(top, path.segments(), path, value)
    }
}

impl Document for PlistDocument {
    fn codec_id(&self) -> &'static str {
        CODEC
    }

    fn to_ast(&self) -> Node {
        // Validated in `parse`; edits are verified before they are committed.
        self.project().unwrap_or(Node::Null)
    }

    fn apply_ast(&mut self, root: &Node) -> CodecResult<()> {
        let current = self.project()?;
        let mut work = self.clone();
        apply_via_diff(&mut work, &current, root)?;
        let got = work.project()?;
        if !semantic_eq(&got, root) {
            return Err(CodecError::structure(CODEC, "$", "the edit cannot be represented exactly in this property list"));
        }
        *self = work;
        Ok(())
    }

    fn encode(&self) -> CodecResult<Vec<u8>> {
        Ok(self.tree.serialize())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::VALUES_ONLY
    }

    fn notes(&self) -> Vec<String> {
        let mut n = Vec::new();
        if let Some(v) = self.tree.root.attrs.iter().find(|a| a.name == "version") {
            n.push(format!("plist version {}", v.raw));
        }
        if self.tree.doctype().is_some() {
            n.push("Apple PLIST DOCTYPE".into());
        }
        if self.tree.bom {
            n.push("UTF-8 BOM".into());
        }
        n
    }

    fn clone_box(&self) -> Box<dyn Document> {
        Box::new(self.clone())
    }
}

/// The XML property-list codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlistCodec;

impl StructuralCodec for PlistCodec {
    fn id(&self) -> &'static str {
        CODEC
    }

    fn name(&self) -> &'static str {
        "XML Property List"
    }

    fn detect(&self, bytes: &[u8], _ctx: &DetectContext) -> Confidence {
        if !starts_like_xml(bytes) {
            return 0;
        }
        match PlistDocument::parse(bytes) {
            Ok(_) => 90,
            Err(_) => 0,
        }
    }

    fn decode(&self, bytes: &[u8], _ctx: &DetectContext) -> CodecResult<Box<dyn Document>> {
        Ok(Box::new(PlistDocument::parse(bytes)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n\t<key>Gold</key>\n\t<integer>1200</integer>\n\t<key>Speed</key>\n\t<real>1.50</real>\n\t<key>Hard</key>\n\t<true/>\n\t<key>Name</key>\n\t<string>A &amp; B</string>\n\t<key>Items</key>\n\t<array>\n\t\t<integer>1</integer>\n\t\t<integer>2</integer>\n\t</array>\n\t<key>Blob</key>\n\t<data>\n\tAAECAwQFBgcICQoL\n\tDA0ODw==\n\t</data>\n</dict>\n</plist>\n";

    fn p(s: &str) -> NodePath {
        NodePath::parse(s).unwrap()
    }

    #[test]
    fn roundtrip_and_projection() {
        let doc = PlistDocument::parse(SRC.as_bytes()).unwrap();
        assert_eq!(doc.encode().unwrap(), SRC.as_bytes());
        let ast = doc.to_ast();
        assert_eq!(ast.get(&p("Gold")), Some(&Node::I64(1200)));
        assert_eq!(ast.get(&p("Speed")), Some(&Node::F64(1.5)));
        assert_eq!(ast.get(&p("Hard")), Some(&Node::Bool(true)));
        assert_eq!(ast.get(&p("Name")), Some(&Node::String("A & B".into())));
        assert_eq!(ast.get(&p("Items[1]")), Some(&Node::I64(2)));
        assert_eq!(ast.get(&p("Blob")), Some(&Node::Bytes((0u8..16).collect())));
    }

    #[test]
    fn edits() {
        let mut doc = PlistDocument::parse(SRC.as_bytes()).unwrap();
        let mut ast = doc.to_ast();
        ast.set(&p("Gold"), Node::I64(99), false).unwrap();
        ast.set(&p("Speed"), Node::F64(2.25), false).unwrap();
        ast.set(&p("Hard"), Node::Bool(false), false).unwrap();
        ast.set(&p("Name"), Node::String("<Hero>".into()), false).unwrap();
        ast.set(&p("Items[0]"), Node::I32(7), false).unwrap();
        ast.set(&p("Blob"), Node::Bytes((100u8..120).collect()), false).unwrap();
        doc.apply_ast(&ast).unwrap();
        let out = String::from_utf8(doc.encode().unwrap()).unwrap();
        let expected = SRC
            .replace("1200", "99")
            .replace("1.50", "2.25")
            .replace("<true/>", "<false/>")
            .replace("A &amp; B", "&lt;Hero&gt;")
            .replace("<integer>1</integer>", "<integer>7</integer>")
            .replace(
                "\tAAECAwQFBgcICQoL\n\tDA0ODw==\n",
                &format!("\t{}\n\t{}\n", &b64().encode((100u8..120).collect::<Vec<u8>>())[..16], &b64().encode((100u8..120).collect::<Vec<u8>>())[16..]),
            );
        assert_eq!(out, expected);
        let re = PlistDocument::parse(out.as_bytes()).unwrap();
        assert!(semantic_eq(&re.to_ast(), &ast));
    }

    #[test]
    fn structural_edits_rejected() {
        let mut doc = PlistDocument::parse(SRC.as_bytes()).unwrap();
        let mut ast = doc.to_ast();
        ast.set(&p("New"), Node::I64(1), true).unwrap();
        assert!(matches!(doc.apply_ast(&ast), Err(CodecError::StructureUnsupported { .. })));
        let mut ast = doc.to_ast();
        if let Some(Node::Array(a)) = ast.get_mut(&p("Items")) {
            a.push(Node::I64(3));
        }
        assert!(doc.apply_ast(&ast).is_err());
        assert_eq!(doc.encode().unwrap(), SRC.as_bytes());
    }

    #[test]
    fn detection() {
        let ctx = DetectContext::default();
        assert_eq!(PlistCodec.detect(SRC.as_bytes(), &ctx), 90);
        assert_eq!(PlistCodec.detect(b"<plist><dict><key>a</key></dict></plist>", &ctx), 0);
        assert_eq!(PlistCodec.detect(b"<root><a>1</a></root>", &ctx), 0);
        assert_eq!(PlistCodec.detect(b"<plist><dict>", &ctx), 0);
    }
}
