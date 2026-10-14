//! Lossless generic XML codec.
//!
//! A small, dependency-free tokenizer builds a tree that keeps every byte of
//! the source: the XML declaration, DOCTYPE (including an internal subset),
//! comments, processing instructions, CDATA sections, text with its entity
//! references kept raw, the attribute order / quote style / whitespace of
//! every tag, self-closing tags, a UTF-8 BOM and whitespace around the root.
//!
//! Projection: the root element becomes a map holding its attributes
//! (`@name`), child elements by tag name (repeated tags → `tag`, `tag#2`, …)
//! and the non-whitespace text of mixed content (`#text`). Child elements that
//! have neither attributes nor child elements project to a typed leaf.
//! Attribute values and text leaves are editable; edited text is re-escaped,
//! and text that lived in CDATA stays CDATA.

use std::collections::HashSet;

use savelens_core::{Node, NodeMap, NodePath, PathSeg};

use super::ini::{classify_scalar, dedup_keys, normalize_scalar, render_scalar, semantic_eq};
use crate::document::{
    apply_via_diff, Capabilities, Confidence, DetectContext, Document, PathEditable, StructuralCodec,
};
use crate::error::{CodecError, CodecResult};

const CODEC: &str = "xml";
const BOM: &[u8] = &[0xef, 0xbb, 0xbf];
/// Maximum element nesting depth.
pub(crate) const MAX_DEPTH: usize = 512;

// ---------------------------------------------------------------------------
// Tree
// ---------------------------------------------------------------------------

/// One attribute of a start tag, with its exact layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attr {
    pub ws_before: String,
    pub name: String,
    pub ws_before_eq: String,
    pub ws_after_eq: String,
    pub quote: char,
    /// Value as written (entities not decoded).
    pub raw: String,
}

/// An element with its children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Element {
    pub name: String,
    pub attrs: Vec<Attr>,
    /// Whitespace between the last attribute (or the name) and `>` / `/>`.
    pub tag_ws: String,
    pub self_closing: bool,
    pub children: Vec<XNode>,
    /// Whitespace between the end-tag name and `>`.
    pub end_ws: String,
}

/// A node of the XML tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum XNode {
    Element(Element),
    /// Character data as written (entities not decoded).
    Text(String),
    /// Content of a CDATA section.
    CData(String),
    /// Whole `<!-- … -->`.
    Comment(String),
    /// Whole `<? … ?>` (including the XML declaration).
    Pi(String),
    /// Whole `<!DOCTYPE … >`.
    Doctype(String),
}

/// A parsed XML document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct XmlTree {
    pub bom: bool,
    pub prolog: Vec<XNode>,
    pub root: Element,
    pub epilog: Vec<XNode>,
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == ':' || (c as u32) >= 0x80
}

fn is_name_char(c: char) -> bool {
    is_name_start(c) || c.is_ascii_digit() || c == '-' || c == '.'
}

fn is_xml_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

struct Tokenizer<'a> {
    src: &'a str,
    pos: usize,
    /// Byte offset of `src` inside the original input (BOM).
    base: usize,
    codec: &'static str,
}

impl<'a> Tokenizer<'a> {
    fn err(&self, reason: impl Into<String>) -> CodecError {
        CodecError::malformed(self.codec, self.base + self.pos, reason)
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn ws(&mut self) -> String {
        let r = self.rest();
        let n = r.len() - r.trim_start_matches(is_xml_ws).len();
        self.pos += n;
        r[..n].to_string()
    }

    fn name(&mut self) -> CodecResult<String> {
        let r = self.rest();
        match r.chars().next() {
            Some(c) if is_name_start(c) => {}
            _ => return Err(self.err("expected a name")),
        }
        let n = r.find(|c: char| !is_name_char(c)).unwrap_or(r.len());
        self.pos += n;
        Ok(r[..n].to_string())
    }

    /// Consumes up to and including `end`, returning the consumed text
    /// starting at `start`.
    fn until(&mut self, start: usize, end: &str, what: &str) -> CodecResult<String> {
        match self.src[self.pos..].find(end) {
            Some(i) => {
                self.pos += i + end.len();
                Ok(self.src[start..self.pos].to_string())
            }
            None => Err(CodecError::malformed(self.codec, self.base + start, format!("unterminated {what}"))),
        }
    }

    fn doctype(&mut self, start: usize) -> CodecResult<String> {
        let mut depth = 0usize;
        let mut quote: Option<char> = None;
        for (i, c) in self.src[self.pos..].char_indices() {
            match (quote, c) {
                (Some(q), c) if c == q => quote = None,
                (Some(_), _) => {}
                (None, '"' | '\'') => quote = Some(c),
                (None, '[') => depth += 1,
                (None, ']') => depth = depth.saturating_sub(1),
                (None, '>') if depth == 0 => {
                    self.pos += i + 1;
                    return Ok(self.src[start..self.pos].to_string());
                }
                _ => {}
            }
        }
        Err(CodecError::malformed(self.codec, self.base + start, "unterminated DOCTYPE"))
    }

    fn start_tag(&mut self) -> CodecResult<Element> {
        // At '<' + name start.
        self.pos += 1;
        let name = self.name()?;
        let mut attrs: Vec<Attr> = Vec::new();
        loop {
            let ws = self.ws();
            match self.peek() {
                Some('>') => {
                    self.pos += 1;
                    return Ok(Element {
                        name,
                        attrs,
                        tag_ws: ws,
                        self_closing: false,
                        children: Vec::new(),
                        end_ws: String::new(),
                    });
                }
                Some('/') => {
                    if !self.rest().starts_with("/>") {
                        return Err(self.err("expected `/>`"));
                    }
                    self.pos += 2;
                    return Ok(Element {
                        name,
                        attrs,
                        tag_ws: ws,
                        self_closing: true,
                        children: Vec::new(),
                        end_ws: String::new(),
                    });
                }
                Some(c) if is_name_start(c) => {
                    if ws.is_empty() {
                        return Err(self.err("attributes must be separated by whitespace"));
                    }
                    let aname = self.name()?;
                    if attrs.iter().any(|a| a.name == aname) {
                        return Err(self.err(format!("duplicate attribute `{aname}`")));
                    }
                    let ws_before_eq = self.ws();
                    if self.peek() != Some('=') {
                        return Err(self.err("expected `=` after attribute name"));
                    }
                    self.pos += 1;
                    let ws_after_eq = self.ws();
                    let quote = match self.peek() {
                        Some(q @ ('"' | '\'')) => q,
                        _ => return Err(self.err("expected a quoted attribute value")),
                    };
                    self.pos += 1;
                    let vstart = self.pos;
                    let Some(len) = self.rest().find(quote) else {
                        return Err(self.err("unterminated attribute value"));
                    };
                    let raw = self.src[vstart..vstart + len].to_string();
                    if raw.contains('<') {
                        return Err(CodecError::malformed(self.codec, self.base + vstart, "`<` in attribute value"));
                    }
                    validate_refs(&raw).map_err(|r| CodecError::malformed(self.codec, self.base + vstart, r))?;
                    self.pos = vstart + len + 1;
                    attrs.push(Attr { ws_before: ws, name: aname, ws_before_eq, ws_after_eq, quote, raw });
                }
                Some(c) => return Err(self.err(format!("unexpected `{c}` in start tag"))),
                None => return Err(self.err("unterminated start tag")),
            }
        }
    }
}

/// Checks that every `&` starts a well-formed reference.
fn validate_refs(raw: &str) -> Result<(), String> {
    let mut rest = raw;
    while let Some(i) = rest.find('&') {
        let after = &rest[i + 1..];
        let Some(end) = after.find(';') else {
            return Err("`&` without a terminating `;`".into());
        };
        let body = &after[..end];
        let ok = if let Some(hex) = body.strip_prefix("#x") {
            u32::from_str_radix(hex, 16).ok().and_then(char::from_u32).is_some()
        } else if let Some(dec) = body.strip_prefix('#') {
            dec.parse::<u32>().ok().and_then(char::from_u32).is_some()
        } else {
            body.chars().next().is_some_and(is_name_start) && body.chars().all(is_name_char)
        };
        if !ok {
            return Err(format!("invalid reference `&{body};`"));
        }
        rest = &after[end + 1..];
    }
    Ok(())
}

/// Decodes predefined entities and character references. Unknown named
/// entities (declared in a DTD) are kept literally.
pub(crate) fn decode_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let decoded = after.find(';').and_then(|end| {
            let body = &after[..end];
            let c = match body {
                "lt" => Some('<'),
                "gt" => Some('>'),
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => {
                    if let Some(hex) = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X")) {
                        u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                    } else if let Some(dec) = body.strip_prefix('#') {
                        dec.parse::<u32>().ok().and_then(char::from_u32)
                    } else {
                        None
                    }
                }
            };
            c.map(|c| (c, end))
        });
        match decoded {
            Some((c, end)) => {
                out.push(c);
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Escapes character data (`&`, `<`, `>`, and CR so it survives parsers'
/// newline normalisation).
pub(crate) fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#13;"),
            c => out.push(c),
        }
    }
    out
}

/// Escapes an attribute value delimited by `quote`.
pub(crate) fn escape_attr(s: &str, quote: char) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if quote == '"' => out.push_str("&quot;"),
            '\'' if quote == '\'' => out.push_str("&apos;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            c => out.push(c),
        }
    }
    out
}

/// Splits text into CDATA section contents so that `]]>` never appears
/// inside one section.
pub(crate) fn cdata_sections(s: &str) -> Vec<String> {
    let parts: Vec<&str> = s.split("]]>").collect();
    let n = parts.len();
    parts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let mut c = String::with_capacity(p.len() + 3);
            if i > 0 {
                c.push('>');
            }
            c.push_str(p);
            if i + 1 < n {
                c.push_str("]]");
            }
            c
        })
        .collect()
}

impl XmlTree {
    /// Parses a complete document with exactly one root element.
    pub(crate) fn parse(bytes: &[u8], codec: &'static str) -> CodecResult<XmlTree> {
        let (bom, body) = match bytes.strip_prefix(BOM) {
            Some(rest) => (true, rest),
            None => (false, bytes),
        };
        let base = if bom { 3 } else { 0 };
        let src = std::str::from_utf8(body)
            .map_err(|e| CodecError::malformed(codec, base + e.valid_up_to(), "XML text is not valid UTF-8"))?;
        let mut t = Tokenizer { src, pos: 0, base, codec };
        let mut stack: Vec<Element> = Vec::new();
        let mut prolog = Vec::new();
        let mut epilog = Vec::new();
        let mut root: Option<Element> = None;

        fn place(
            node: XNode,
            stack: &mut [Element],
            root: &mut Option<Element>,
            prolog: &mut Vec<XNode>,
            epilog: &mut Vec<XNode>,
            t: &Tokenizer<'_>,
        ) -> CodecResult<()> {
            if let Some(top) = stack.last_mut() {
                top.children.push(node);
                return Ok(());
            }
            match node {
                XNode::Element(e) => {
                    if root.is_some() {
                        return Err(t.err("more than one root element"));
                    }
                    *root = Some(e);
                }
                other => {
                    if root.is_none() {
                        prolog.push(other);
                    } else {
                        epilog.push(other);
                    }
                }
            }
            Ok(())
        }

        while t.pos < src.len() {
            let start = t.pos;
            let rest = t.rest();
            if !rest.starts_with('<') {
                let len = rest.find('<').unwrap_or(rest.len());
                let text = &rest[..len];
                if stack.is_empty() && !text.chars().all(is_xml_ws) {
                    return Err(t.err("text outside the root element"));
                }
                if text.contains("]]>") {
                    return Err(t.err("`]]>` in character data"));
                }
                validate_refs(text).map_err(|r| t.err(r))?;
                t.pos += len;
                place(XNode::Text(text.to_string()), &mut stack, &mut root, &mut prolog, &mut epilog, &t)?;
                continue;
            }
            if rest.starts_with("<!--") {
                t.pos += 4;
                let raw = t.until(start, "-->", "comment")?;
                place(XNode::Comment(raw), &mut stack, &mut root, &mut prolog, &mut epilog, &t)?;
            } else if rest.starts_with("<![CDATA[") {
                if stack.is_empty() {
                    return Err(t.err("CDATA outside the root element"));
                }
                t.pos += 9;
                let raw = t.until(start, "]]>", "CDATA section")?;
                let inner = raw[9..raw.len() - 3].to_string();
                place(XNode::CData(inner), &mut stack, &mut root, &mut prolog, &mut epilog, &t)?;
            } else if rest.starts_with("<!DOCTYPE") {
                if !stack.is_empty() || root.is_some() {
                    return Err(t.err("DOCTYPE after the root element started"));
                }
                t.pos += 9;
                let raw = t.doctype(start)?;
                place(XNode::Doctype(raw), &mut stack, &mut root, &mut prolog, &mut epilog, &t)?;
            } else if rest.starts_with("<?") {
                t.pos += 2;
                if !t.peek().is_some_and(is_name_start) {
                    return Err(t.err("processing instruction without a target"));
                }
                let raw = t.until(start, "?>", "processing instruction")?;
                place(XNode::Pi(raw), &mut stack, &mut root, &mut prolog, &mut epilog, &t)?;
            } else if rest.starts_with("</") {
                t.pos += 2;
                let name = t.name()?;
                let end_ws = t.ws();
                if t.peek() != Some('>') {
                    return Err(t.err("expected `>` in end tag"));
                }
                t.pos += 1;
                let Some(mut el) = stack.pop() else {
                    return Err(CodecError::malformed(codec, base + start, format!("unexpected end tag `</{name}>`")));
                };
                if el.name != name {
                    return Err(CodecError::malformed(
                        codec,
                        base + start,
                        format!("end tag `</{name}>` does not match `<{}>`", el.name),
                    ));
                }
                el.end_ws = end_ws;
                place(XNode::Element(el), &mut stack, &mut root, &mut prolog, &mut epilog, &t)?;
            } else if rest[1..].starts_with(is_name_start) {
                if stack.is_empty() && root.is_some() {
                    return Err(t.err("more than one root element"));
                }
                let el = t.start_tag()?;
                if el.self_closing {
                    place(XNode::Element(el), &mut stack, &mut root, &mut prolog, &mut epilog, &t)?;
                } else {
                    if stack.len() >= MAX_DEPTH {
                        return Err(t.err("elements nested too deeply"));
                    }
                    stack.push(el);
                }
            } else {
                return Err(t.err("invalid markup"));
            }
        }
        if let Some(open) = stack.last() {
            return Err(CodecError::malformed(
                codec,
                base + src.len(),
                format!("element `<{}>` is never closed", open.name),
            ));
        }
        let root = root.ok_or_else(|| CodecError::malformed(codec, base, "no root element"))?;
        Ok(XmlTree { bom, prolog, root, epilog })
    }

    /// Serialises the tree.
    pub(crate) fn serialize(&self) -> Vec<u8> {
        let mut out = String::new();
        for n in &self.prolog {
            write_node(n, &mut out);
        }
        write_element(&self.root, &mut out);
        for n in &self.epilog {
            write_node(n, &mut out);
        }
        let mut bytes = Vec::with_capacity(out.len() + 3);
        if self.bom {
            bytes.extend_from_slice(BOM);
        }
        bytes.extend_from_slice(out.as_bytes());
        bytes
    }

    /// The DOCTYPE declaration, if any.
    pub(crate) fn doctype(&self) -> Option<&str> {
        self.prolog.iter().find_map(|n| match n {
            XNode::Doctype(d) => Some(d.as_str()),
            _ => None,
        })
    }
}

fn write_node(n: &XNode, out: &mut String) {
    match n {
        XNode::Element(e) => write_element(e, out),
        XNode::Text(t) => out.push_str(t),
        XNode::CData(c) => {
            out.push_str("<![CDATA[");
            out.push_str(c);
            out.push_str("]]>");
        }
        XNode::Comment(s) | XNode::Pi(s) | XNode::Doctype(s) => out.push_str(s),
    }
}

fn write_element(e: &Element, out: &mut String) {
    out.push('<');
    out.push_str(&e.name);
    for a in &e.attrs {
        out.push_str(&a.ws_before);
        out.push_str(&a.name);
        out.push_str(&a.ws_before_eq);
        out.push('=');
        out.push_str(&a.ws_after_eq);
        out.push(a.quote);
        out.push_str(&a.raw);
        out.push(a.quote);
    }
    out.push_str(&e.tag_ws);
    if e.self_closing {
        out.push_str("/>");
        return;
    }
    out.push('>');
    for c in &e.children {
        write_node(c, out);
    }
    out.push_str("</");
    out.push_str(&e.name);
    out.push_str(&e.end_ws);
    out.push('>');
}

impl Element {
    /// Child elements in document order.
    pub(crate) fn child_elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|c| match c {
            XNode::Element(e) => Some(e),
            _ => None,
        })
    }

    /// Mutable child element by position among the child elements.
    pub(crate) fn child_element_mut(&mut self, n: usize) -> Option<&mut Element> {
        self.children
            .iter_mut()
            .filter_map(|c| match c {
                XNode::Element(e) => Some(e),
                _ => None,
            })
            .nth(n)
    }

    /// Decoded text content (text and CDATA children, concatenated).
    pub(crate) fn text(&self) -> String {
        let mut s = String::new();
        for c in &self.children {
            match c {
                XNode::Text(t) => s.push_str(&decode_entities(t)),
                XNode::CData(c) => s.push_str(c),
                _ => {}
            }
        }
        s
    }

    /// Replaces the text content with `value`. Text that was written as
    /// CDATA stays CDATA; comments and processing instructions are kept;
    /// a self-closing element is expanded to a start/end tag pair.
    pub(crate) fn set_text(&mut self, value: &str) {
        let first = self.children.iter().position(|c| matches!(c, XNode::Text(_) | XNode::CData(_)));
        let cdata = first.is_some_and(|i| matches!(self.children[i], XNode::CData(_)));
        let replacement: Vec<XNode> = if value.is_empty() {
            Vec::new()
        } else if cdata {
            cdata_sections(value).into_iter().map(XNode::CData).collect()
        } else {
            vec![XNode::Text(escape_text(value))]
        };
        let insert_at = first.unwrap_or(self.children.len());
        let mut kept: Vec<XNode> = Vec::with_capacity(self.children.len());
        let mut at = insert_at;
        for (i, c) in std::mem::take(&mut self.children).into_iter().enumerate() {
            if matches!(c, XNode::Text(_) | XNode::CData(_)) {
                if i < insert_at {
                    at -= 1;
                }
                continue;
            }
            kept.push(c);
        }
        let at = at.min(kept.len());
        kept.splice(at..at, replacement);
        self.children = kept;
        if self.self_closing && !value.is_empty() {
            self.self_closing = false;
            self.end_ws.clear();
        }
    }

    /// True when the element projects to a scalar leaf.
    fn is_leaf(&self) -> bool {
        self.attrs.is_empty() && self.child_elements().next().is_none()
    }

    /// AST keys of the child elements (`tag`, `tag#2`, …).
    fn child_keys(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        dedup_keys(self.child_elements().map(|e| e.name.clone()), &mut seen)
    }

    /// Indices (in `children`) of non-whitespace text / CDATA nodes.
    fn mixed_text_nodes(&self) -> Vec<usize> {
        self.children
            .iter()
            .enumerate()
            .filter(|(_, c)| match c {
                XNode::Text(t) => !t.chars().all(is_xml_ws),
                XNode::CData(c) => !c.chars().all(is_xml_ws),
                _ => false,
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn mixed_text(&self) -> String {
        self.mixed_text_nodes()
            .into_iter()
            .map(|i| match &self.children[i] {
                XNode::Text(t) => decode_entities(t),
                XNode::CData(c) => c.clone(),
                _ => String::new(),
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Projection & editing
// ---------------------------------------------------------------------------

fn project_map(e: &Element) -> Node {
    let mut m = NodeMap::new();
    for a in &e.attrs {
        m.insert(format!("@{}", a.name), Node::String(decode_entities(&a.raw)));
    }
    for (k, c) in e.child_keys().into_iter().zip(e.child_elements()) {
        m.insert(k, project_element(c));
    }
    let text = e.mixed_text();
    if !text.is_empty() {
        m.insert("#text".into(), Node::String(text));
    }
    Node::Map(m)
}

fn project_element(e: &Element) -> Node {
    if e.is_leaf() {
        classify_scalar(&e.text())
    } else {
        project_map(e)
    }
}

fn set_in(e: &mut Element, segs: &[PathSeg], full: &NodePath, value: &Node) -> CodecResult<()> {
    let Some((first, rest)) = segs.split_first() else {
        if !e.is_leaf() {
            return Err(CodecError::structure(
                CODEC,
                full,
                "an element with attributes or children cannot become a scalar",
            ));
        }
        let old = e.text();
        let existing = classify_scalar(&old);
        let target = normalize_scalar(CODEC, full, &existing, value, true)?;
        let text = render_scalar(&old, &target).ok_or_else(|| {
            CodecError::unencodable(CODEC, full, format!("{} cannot be written as XML text", target.kind().name()))
        })?;
        let back = classify_scalar(&text);
        if !semantic_eq(&back, &target) {
            return Err(CodecError::unencodable(
                CODEC,
                full,
                format!("text `{text}` would be read back as {}", back.preview(40)),
            ));
        }
        e.set_text(&text);
        return Ok(());
    };
    let PathSeg::Key(k) = first else {
        return Err(CodecError::path_not_found(CODEC, full));
    };
    if e.is_leaf() {
        return Err(CodecError::path_not_found(CODEC, full));
    }
    if let Some(aname) = k.strip_prefix('@') {
        if !rest.is_empty() {
            return Err(CodecError::path_not_found(CODEC, full));
        }
        let attr =
            e.attrs.iter_mut().find(|a| a.name == aname).ok_or_else(|| CodecError::path_not_found(CODEC, full))?;
        let existing = Node::String(decode_entities(&attr.raw));
        let target = normalize_scalar(CODEC, full, &existing, value, false)?;
        let Node::String(s) = target else {
            return Err(CodecError::type_mismatch(CODEC, full, "string", target.kind().name()));
        };
        attr.raw = escape_attr(&s, attr.quote);
        return Ok(());
    }
    if k == "#text" {
        if !rest.is_empty() {
            return Err(CodecError::path_not_found(CODEC, full));
        }
        let nodes = e.mixed_text_nodes();
        let [idx] = nodes.as_slice() else {
            return Err(CodecError::structure(
                CODEC,
                full,
                "mixed text split around child elements cannot be edited as one value",
            ));
        };
        let existing = Node::String(e.mixed_text());
        let target = normalize_scalar(CODEC, full, &existing, value, false)?;
        let Node::String(s) = target else {
            return Err(CodecError::type_mismatch(CODEC, full, "string", target.kind().name()));
        };
        if s.chars().all(is_xml_ws) {
            return Err(CodecError::structure(CODEC, full, "mixed text cannot become empty or whitespace-only"));
        }
        let idx = *idx;
        let replacement: Vec<XNode> = match &e.children[idx] {
            XNode::CData(_) => cdata_sections(&s).into_iter().map(XNode::CData).collect(),
            _ => vec![XNode::Text(escape_text(&s))],
        };
        e.children.splice(idx..idx + 1, replacement);
        return Ok(());
    }
    let pos = e.child_keys().iter().position(|x| x == k).ok_or_else(|| CodecError::path_not_found(CODEC, full))?;
    let child = e.child_element_mut(pos).ok_or_else(|| CodecError::path_not_found(CODEC, full))?;
    set_in(child, rest, full, value)
}

/// A parsed generic XML document.
#[derive(Debug, Clone)]
pub struct XmlDocument {
    tree: XmlTree,
}

impl XmlDocument {
    /// Parses XML bytes (UTF-8).
    pub fn parse(bytes: &[u8]) -> CodecResult<XmlDocument> {
        Ok(XmlDocument { tree: XmlTree::parse(bytes, CODEC)? })
    }

    /// Name of the root element.
    pub fn root_name(&self) -> &str {
        &self.tree.root.name
    }
}

impl PathEditable for XmlDocument {
    fn codec(&self) -> &'static str {
        CODEC
    }

    fn set_value(&mut self, path: &NodePath, value: &Node) -> CodecResult<()> {
        if path.is_root() {
            return Err(CodecError::structure(CODEC, path, "the root element cannot be replaced"));
        }
        set_in(&mut self.tree.root, path.segments(), path, value)
    }
}

impl Document for XmlDocument {
    fn codec_id(&self) -> &'static str {
        CODEC
    }

    fn to_ast(&self) -> Node {
        project_map(&self.tree.root)
    }

    fn apply_ast(&mut self, root: &Node) -> CodecResult<()> {
        let current = self.to_ast();
        let mut work = self.clone();
        apply_via_diff(&mut work, &current, root)?;
        if !semantic_eq(&work.to_ast(), root) {
            return Err(CodecError::structure(
                CODEC,
                "$",
                "the edit cannot be represented exactly in this XML document",
            ));
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
        let mut n = vec![format!("root element <{}>", self.tree.root.name)];
        if self.tree.bom {
            n.push("UTF-8 BOM".into());
        }
        if let Some(XNode::Pi(p)) =
            self.tree.prolog.iter().find(|x| matches!(x, XNode::Pi(p) if p.starts_with("<?xml ")))
        {
            n.push(format!("declaration {p}"));
        }
        if self.tree.doctype().is_some() {
            n.push("DOCTYPE kept verbatim".into());
        }
        n
    }

    fn clone_box(&self) -> Box<dyn Document> {
        Box::new(self.clone())
    }
}

/// Byte-level check that the payload starts like XML.
pub(crate) fn starts_like_xml(bytes: &[u8]) -> bool {
    let body = bytes.strip_prefix(BOM).unwrap_or(bytes);
    let Some(start) = body.iter().position(|b| !b.is_ascii_whitespace()) else {
        return false;
    };
    let rest = &body[start..];
    rest.starts_with(b"<?xml")
        || rest.starts_with(b"<!--")
        || rest.starts_with(b"<!DOCTYPE")
        || (rest.len() > 1 && rest[0] == b'<' && (rest[1].is_ascii_alphabetic() || rest[1] == b'_'))
}

/// The generic XML codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct XmlCodec;

impl StructuralCodec for XmlCodec {
    fn id(&self) -> &'static str {
        CODEC
    }

    fn name(&self) -> &'static str {
        "XML"
    }

    fn detect(&self, bytes: &[u8], _ctx: &DetectContext) -> Confidence {
        if !starts_like_xml(bytes) {
            return 0;
        }
        match XmlTree::parse(bytes, CODEC) {
            Ok(t) if t.root.name == "unity_prefs" || t.root.name == "plist" => 50,
            Ok(_) => 80,
            Err(_) => 0,
        }
    }

    fn decode(&self, bytes: &[u8], _ctx: &DetectContext) -> CodecResult<Box<dyn Document>> {
        Ok(Box::new(XmlDocument::parse(bytes)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> NodePath {
        NodePath::parse(s).unwrap()
    }

    fn rt(src: &str) -> XmlDocument {
        let doc = XmlDocument::parse(src.as_bytes()).unwrap();
        assert_eq!(String::from_utf8(doc.encode().unwrap()).unwrap(), src);
        doc
    }

    #[test]
    fn lossless_layouts() {
        rt("<a/>");
        rt("<a></a>");
        rt("\u{feff}<?xml version='1.0' encoding=\"UTF-8\"?>\r\n<!-- c --><!DOCTYPE r [ <!ENTITY e \"x>y\"> ]>\n<r  x = 'a&amp;b'\ty=\"2\" ><?pi data?><![CDATA[<raw>]]>t&e;<b/>&#65;&#x42;</r >\n\n");
    }

    #[test]
    fn projection() {
        let doc = rt("<save v=\"3\"><gold>150</gold><ratio>0.50</ratio><ok>true</ok><name>A &amp; B</name><empty/><item>1</item><item>2</item><p id='x'>hi</p>tail</save>");
        let ast = doc.to_ast();
        let g = |s: &str| ast.get(&p(s)).cloned().unwrap();
        assert_eq!(g("[\"@v\"]"), Node::String("3".into()));
        assert_eq!(g("gold"), Node::I64(150));
        assert_eq!(g("ratio"), Node::F64(0.5));
        assert_eq!(g("ok"), Node::Bool(true));
        assert_eq!(g("name"), Node::String("A & B".into()));
        assert_eq!(g("empty"), Node::String(String::new()));
        assert_eq!(g("[\"item#2\"]"), Node::I64(2));
        assert_eq!(g("p[\"#text\"]"), Node::String("hi".into()));
        assert_eq!(g("[\"#text\"]"), Node::String("tail".into()));
    }

    #[test]
    fn edits_escape_and_keep_layout() {
        let mut doc = rt("<s a='x'>\n  <n>Bob</n>\n  <c><![CDATA[old]]></c>\n  <e/>\n  <r>1.50</r>\n</s>");
        let mut ast = doc.to_ast();
        ast.set(&p("[\"@a\"]"), Node::String("it's <&>".into()), false).unwrap();
        ast.set(&p("n"), Node::String("Tom & \"Jerry\"".into()), false).unwrap();
        ast.set(&p("c"), Node::String("a]]>b".into()), false).unwrap();
        ast.set(&p("e"), Node::String("filled".into()), false).unwrap();
        ast.set(&p("r"), Node::F64(2.5), false).unwrap();
        doc.apply_ast(&ast).unwrap();
        let out = String::from_utf8(doc.encode().unwrap()).unwrap();
        assert_eq!(
            out,
            "<s a='it&apos;s &lt;&amp;&gt;'>\n  <n>Tom &amp; \"Jerry\"</n>\n  <c><![CDATA[a]]]]><![CDATA[>b]]></c>\n  <e>filled</e>\n  <r>2.50</r>\n</s>"
        );
        let re = XmlDocument::parse(out.as_bytes()).unwrap();
        assert!(semantic_eq(&re.to_ast(), &ast));
    }

    #[test]
    fn rejected_edits() {
        let mut doc = rt("<s><n>Bob</n><k>1</k></s>");
        let mut ast = doc.to_ast();
        ast.set(&p("n"), Node::String("42".into()), false).unwrap();
        assert!(doc.apply_ast(&ast).is_err());
        let mut ast = doc.to_ast();
        ast.set(&p("z"), Node::I64(1), true).unwrap();
        assert!(matches!(doc.apply_ast(&ast), Err(CodecError::StructureUnsupported { .. })));
        let mut ast = doc.to_ast();
        ast.remove(&p("k")).unwrap();
        assert!(doc.apply_ast(&ast).is_err());
        assert_eq!(doc.encode().unwrap(), b"<s><n>Bob</n><k>1</k></s>");
    }

    #[test]
    fn malformed_rejected() {
        for bad in [
            "<a><b></a>",
            "<a>",
            "<a></a><b/>",
            "text<a/>",
            "<a x=1/>",
            "<a x='1' x='2'/>",
            "<a>&bogus</a>",
            "<a><!-- x</a>",
            "",
        ] {
            assert!(XmlDocument::parse(bad.as_bytes()).is_err(), "{bad}");
            assert_eq!(XmlCodec.detect(bad.as_bytes(), &DetectContext::default()), 0, "{bad}");
        }
    }

    #[test]
    fn detection() {
        let ctx = DetectContext::default();
        assert_eq!(XmlCodec.detect(b"<?xml version=\"1.0\"?><a/>", &ctx), 80);
        assert_eq!(XmlCodec.detect(b"  <root><x>1</x></root>\n", &ctx), 80);
        assert_eq!(XmlCodec.detect(b"<unity_prefs><pref name=\"a\" type=\"int\">1</pref></unity_prefs>", &ctx), 50);
        assert_eq!(XmlCodec.detect(b"<plist version=\"1.0\"><dict/></plist>", &ctx), 50);
        assert_eq!(XmlCodec.detect(b"{\"a\":1}", &ctx), 0);
        assert_eq!(XmlCodec.detect(b"[a]\nb=1\n", &ctx), 0);
    }

    #[test]
    fn helpers() {
        assert_eq!(decode_entities("&lt;&gt;&amp;&quot;&apos;&#65;&#x42;&custom;&"), "<>&\"'AB&custom;&");
        assert_eq!(cdata_sections("x"), vec!["x".to_string()]);
        assert_eq!(cdata_sections("a]]>b]]>c"), vec!["a]]".to_string(), ">b]]".to_string(), ">c".to_string()]);
    }
}
