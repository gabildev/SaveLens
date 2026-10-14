//! Node paths: `Player.Inventory[3].Name`, `Root["key.with.dots"]`.
//!
//! Grammar:
//!
//! ```text
//! path     := ε | "$" | first rest*
//! first    := ident | quoted | index
//! rest     := "." ident | quoted | index
//! ident    := [^.\[\]"\s]+
//! quoted   := "[" '"' (escaped char)* '"' "]"
//! index    := "[" digits "]"
//! ```

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{CoreError, CoreResult};

/// A single step of a [`NodePath`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PathSeg {
    /// Map key.
    Key(String),
    /// Array index.
    Index(usize),
}

impl PathSeg {
    /// Returns the key if this is a key segment.
    pub fn as_key(&self) -> Option<&str> {
        match self {
            PathSeg::Key(k) => Some(k),
            PathSeg::Index(_) => None,
        }
    }

    /// Returns the index if this is an index segment.
    pub fn as_index(&self) -> Option<usize> {
        match self {
            PathSeg::Index(i) => Some(*i),
            PathSeg::Key(_) => None,
        }
    }
}

/// Location of a node inside a [`crate::Node`] tree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodePath(Vec<PathSeg>);

impl NodePath {
    /// The root path (empty).
    pub fn root() -> Self {
        NodePath(Vec::new())
    }

    /// Builds a path from segments.
    pub fn from_segments(segs: Vec<PathSeg>) -> Self {
        NodePath(segs)
    }

    /// Parses a textual path.
    pub fn parse(input: &str) -> CoreResult<Self> {
        input.parse()
    }

    /// Segments of the path.
    pub fn segments(&self) -> &[PathSeg] {
        &self.0
    }

    /// Number of segments.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True for the root path.
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// Same as [`NodePath::is_root`]; provided for clippy's `len_without_is_empty`.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns a new path with a key appended.
    pub fn key(&self, key: impl Into<String>) -> Self {
        let mut segs = self.0.clone();
        segs.push(PathSeg::Key(key.into()));
        NodePath(segs)
    }

    /// Returns a new path with an index appended.
    pub fn index(&self, index: usize) -> Self {
        let mut segs = self.0.clone();
        segs.push(PathSeg::Index(index));
        NodePath(segs)
    }

    /// Appends a segment in place.
    pub fn push(&mut self, seg: PathSeg) {
        self.0.push(seg);
    }

    /// Removes the last segment in place.
    pub fn pop(&mut self) -> Option<PathSeg> {
        self.0.pop()
    }

    /// Parent path (root has no parent).
    pub fn parent(&self) -> Option<NodePath> {
        if self.0.is_empty() {
            None
        } else {
            Some(NodePath(self.0[..self.0.len() - 1].to_vec()))
        }
    }

    /// Last segment.
    pub fn last(&self) -> Option<&PathSeg> {
        self.0.last()
    }

    /// The last key segment of the path, used for short human labels
    /// (`Player.Stats.Gold` → `Gold`, `Items[3]` → `Items[3]`).
    pub fn leaf_label(&self) -> String {
        let mut label = String::new();
        let mut idx_suffix = Vec::new();
        for seg in self.0.iter().rev() {
            match seg {
                PathSeg::Index(i) => idx_suffix.push(*i),
                PathSeg::Key(k) => {
                    label.push_str(k);
                    break;
                }
            }
        }
        for i in idx_suffix.iter().rev() {
            label.push_str(&format!("[{i}]"));
        }
        if label.is_empty() {
            "$".to_string()
        } else {
            label
        }
    }

    /// True if `self` equals `other` or is an ancestor of it.
    pub fn is_prefix_of(&self, other: &NodePath) -> bool {
        other.0.len() >= self.0.len() && other.0[..self.0.len()] == self.0[..]
    }

    /// Concatenates two paths.
    pub fn join(&self, other: &NodePath) -> NodePath {
        let mut segs = self.0.clone();
        segs.extend(other.0.iter().cloned());
        NodePath(segs)
    }

    /// Strips `prefix` from the start of this path.
    pub fn strip_prefix(&self, prefix: &NodePath) -> Option<NodePath> {
        if prefix.is_prefix_of(self) {
            Some(NodePath(self.0[prefix.0.len()..].to_vec()))
        } else {
            None
        }
    }
}

fn key_needs_quoting(key: &str) -> bool {
    key.is_empty()
        || key == "$"
        || key
            .chars()
            .any(|c| matches!(c, '.' | '[' | ']' | '"' | '\\') || c.is_whitespace() || c.is_control())
}

fn write_quoted(f: &mut fmt::Formatter<'_>, key: &str) -> fmt::Result {
    f.write_str("[\"")?;
    for c in key.chars() {
        match c {
            '"' => f.write_str("\\\"")?,
            '\\' => f.write_str("\\\\")?,
            '\n' => f.write_str("\\n")?,
            '\r' => f.write_str("\\r")?,
            '\t' => f.write_str("\\t")?,
            c if c.is_control() => write!(f, "\\u{{{:x}}}", c as u32)?,
            c => write!(f, "{c}")?,
        }
    }
    f.write_str("\"]")
}

impl fmt::Display for NodePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("$");
        }
        for (i, seg) in self.0.iter().enumerate() {
            match seg {
                PathSeg::Index(n) => write!(f, "[{n}]")?,
                PathSeg::Key(k) if key_needs_quoting(k) => write_quoted(f, k)?,
                PathSeg::Key(k) => {
                    if i > 0 {
                        f.write_str(".")?;
                    }
                    f.write_str(k)?;
                }
            }
        }
        Ok(())
    }
}

struct Parser<'a> {
    input: &'a str,
    chars: Vec<(usize, char)>,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn err(&self, reason: impl Into<String>) -> CoreError {
        let byte = self.chars.get(self.pos).map(|(b, _)| *b).unwrap_or(self.input.len());
        CoreError::PathParse {
            input: self.input.to_string(),
            position: byte,
            reason: reason.into(),
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).map(|(_, c)| *c)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn ident(&mut self) -> CoreResult<String> {
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if matches!(c, '.' | '[' | ']' | '"') || c.is_whitespace() {
                break;
            }
            s.push(c);
            self.pos += 1;
        }
        if s.is_empty() {
            return Err(self.err("expected a key"));
        }
        Ok(s)
    }

    fn bracket(&mut self) -> CoreResult<PathSeg> {
        // Caller consumed '['.
        match self.peek() {
            Some('"') => {
                self.pos += 1;
                let mut s = String::new();
                loop {
                    match self.bump() {
                        None => return Err(self.err("unterminated quoted key")),
                        Some('"') => break,
                        Some('\\') => match self.bump() {
                            Some('"') => s.push('"'),
                            Some('\\') => s.push('\\'),
                            Some('n') => s.push('\n'),
                            Some('r') => s.push('\r'),
                            Some('t') => s.push('\t'),
                            Some('u') => {
                                if self.bump() != Some('{') {
                                    return Err(self.err("expected `{` after \\u"));
                                }
                                let mut hex = String::new();
                                loop {
                                    match self.bump() {
                                        Some('}') => break,
                                        Some(c) if c.is_ascii_hexdigit() => hex.push(c),
                                        _ => return Err(self.err("invalid \\u{..} escape")),
                                    }
                                }
                                let code = u32::from_str_radix(&hex, 16)
                                    .map_err(|_| self.err("invalid \\u{..} escape"))?;
                                let c = char::from_u32(code)
                                    .ok_or_else(|| self.err("invalid unicode scalar"))?;
                                s.push(c);
                            }
                            _ => return Err(self.err("unknown escape")),
                        },
                        Some(c) => s.push(c),
                    }
                }
                if self.bump() != Some(']') {
                    return Err(self.err("expected `]` after quoted key"));
                }
                Ok(PathSeg::Key(s))
            }
            Some(c) if c.is_ascii_digit() => {
                let mut digits = String::new();
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() {
                        digits.push(c);
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                if self.bump() != Some(']') {
                    return Err(self.err("expected `]` after index"));
                }
                let n = digits
                    .parse::<usize>()
                    .map_err(|_| self.err("index too large"))?;
                Ok(PathSeg::Index(n))
            }
            _ => Err(self.err("expected index or quoted key after `[`")),
        }
    }
}

impl FromStr for NodePath {
    type Err = CoreError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let trimmed = input.trim();
        if trimmed.is_empty() || trimmed == "$" {
            return Ok(NodePath::root());
        }
        let body = trimmed.strip_prefix("$.").unwrap_or(trimmed);
        let body = if body.starts_with("$[") { &body[1..] } else { body };
        let mut p = Parser {
            input: body,
            chars: body.char_indices().collect(),
            pos: 0,
        };
        let mut segs = Vec::new();
        let mut first = true;
        while let Some(c) = p.peek() {
            match c {
                '[' => {
                    p.pos += 1;
                    segs.push(p.bracket()?);
                }
                '.' => {
                    if first {
                        return Err(p.err("path cannot start with `.`"));
                    }
                    p.pos += 1;
                    segs.push(PathSeg::Key(p.ident()?));
                }
                _ => {
                    if !first {
                        return Err(p.err("expected `.` or `[`"));
                    }
                    segs.push(PathSeg::Key(p.ident()?));
                }
            }
            first = false;
        }
        Ok(NodePath(segs))
    }
}

impl Serialize for NodePath {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for NodePath {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl From<Vec<PathSeg>> for NodePath {
    fn from(v: Vec<PathSeg>) -> Self {
        NodePath(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_parse_simple() {
        let p = NodePath::root().key("Player").key("Inventory").index(3).key("Name");
        assert_eq!(p.to_string(), "Player.Inventory[3].Name");
        assert_eq!(NodePath::parse("Player.Inventory[3].Name").unwrap(), p);
    }

    #[test]
    fn quoted_keys() {
        let p = NodePath::root().key("a.b").key("c d").key("q\"x");
        let s = p.to_string();
        assert_eq!(s, "[\"a.b\"][\"c d\"][\"q\\\"x\"]");
        assert_eq!(NodePath::parse(&s).unwrap(), p);
    }

    #[test]
    fn root_forms() {
        assert!(NodePath::parse("").unwrap().is_root());
        assert!(NodePath::parse("$").unwrap().is_root());
        assert_eq!(NodePath::root().to_string(), "$");
        assert_eq!(NodePath::parse("$.a[1]").unwrap(), NodePath::root().key("a").index(1));
        assert_eq!(NodePath::parse("[2].x").unwrap(), NodePath::root().index(2).key("x"));
    }

    #[test]
    fn errors() {
        assert!(NodePath::parse(".a").is_err());
        assert!(NodePath::parse("a[").is_err());
        assert!(NodePath::parse("a[x]").is_err());
        assert!(NodePath::parse("a[\"x").is_err());
        assert!(NodePath::parse("a..b").is_err());
    }

    #[test]
    fn prefix_ops() {
        let a = NodePath::parse("Player.Inventory").unwrap();
        let b = NodePath::parse("Player.Inventory[2].Count").unwrap();
        assert!(a.is_prefix_of(&b));
        assert!(!b.is_prefix_of(&a));
        assert_eq!(b.strip_prefix(&a).unwrap().to_string(), "[2].Count");
        assert_eq!(b.leaf_label(), "Count");
        assert_eq!(NodePath::parse("Items[3]").unwrap().leaf_label(), "Items[3]");
        assert_eq!(b.parent().unwrap().to_string(), "Player.Inventory[2]");
    }

    #[test]
    fn unicode_escape_roundtrip() {
        let p = NodePath::root().key("tab\tkey\u{1}");
        let s = p.to_string();
        assert_eq!(NodePath::parse(&s).unwrap(), p);
    }
}
