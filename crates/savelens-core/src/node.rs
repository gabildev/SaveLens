//! The typed save-file tree (`Node`) and the numeric value model shared by the
//! diff engine, the scanner and the editors.

use std::fmt;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};
use crate::path::{NodePath, PathSeg};
use crate::raw::RawRegion;

/// Ordered map used for `Node::Map` (key order is part of the layout).
pub type NodeMap = IndexMap<String, Node>;

/// A node of the semantic save tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum Node {
    Null,
    Bool(bool),
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
    String(String),
    Bytes(Vec<u8>),
    Array(Vec<Node>),
    Map(NodeMap),
    RawRegion(RawRegion),
}

/// Discriminant of a [`Node`], used for schema checks and display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeKind {
    Null,
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
    String,
    Bytes,
    Array,
    Map,
    RawRegion,
}

impl NodeKind {
    /// Human-readable lowercase name.
    pub fn name(self) -> &'static str {
        match self {
            NodeKind::Null => "null",
            NodeKind::Bool => "bool",
            NodeKind::I8 => "i8",
            NodeKind::I16 => "i16",
            NodeKind::I32 => "i32",
            NodeKind::I64 => "i64",
            NodeKind::U8 => "u8",
            NodeKind::U16 => "u16",
            NodeKind::U32 => "u32",
            NodeKind::U64 => "u64",
            NodeKind::F32 => "f32",
            NodeKind::F64 => "f64",
            NodeKind::String => "string",
            NodeKind::Bytes => "bytes",
            NodeKind::Array => "array",
            NodeKind::Map => "map",
            NodeKind::RawRegion => "raw-region",
        }
    }

    /// True for integer and float kinds.
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            NodeKind::I8
                | NodeKind::I16
                | NodeKind::I32
                | NodeKind::I64
                | NodeKind::U8
                | NodeKind::U16
                | NodeKind::U32
                | NodeKind::U64
                | NodeKind::F32
                | NodeKind::F64
        )
    }

    /// True for containers (array / map).
    pub fn is_container(self) -> bool {
        matches!(self, NodeKind::Array | NodeKind::Map)
    }
}

impl fmt::Display for NodeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A numeric value extracted from a node or a raw offset.
///
/// Integers are widened to `i128` so that every `i64`/`u64` fits losslessly.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value")]
pub enum ScalarValue {
    Int(i128),
    Float(f64),
}

impl ScalarValue {
    /// Value as `f64` (may lose precision for huge integers).
    pub fn as_f64(self) -> f64 {
        match self {
            ScalarValue::Int(i) => i as f64,
            ScalarValue::Float(f) => f,
        }
    }

    /// Exact integer value, if this is an integer or an integral float.
    pub fn as_i128(self) -> Option<i128> {
        match self {
            ScalarValue::Int(i) => Some(i),
            ScalarValue::Float(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 1.7e38 => {
                Some(f as i128)
            }
            ScalarValue::Float(_) => None,
        }
    }

    /// `self - other`, as an integer when both are integers.
    pub fn delta(self, other: ScalarValue) -> ScalarValue {
        match (self, other) {
            (ScalarValue::Int(a), ScalarValue::Int(b)) => ScalarValue::Int(a.wrapping_sub(b)),
            (a, b) => ScalarValue::Float(a.as_f64() - b.as_f64()),
        }
    }

    /// `self + d`.
    pub fn plus(self, d: ScalarValue) -> ScalarValue {
        match (self, d) {
            (ScalarValue::Int(a), ScalarValue::Int(b)) => ScalarValue::Int(a.wrapping_add(b)),
            (a, b) => ScalarValue::Float(a.as_f64() + b.as_f64()),
        }
    }

    /// True when the value is a float that is NaN.
    pub fn is_nan(self) -> bool {
        matches!(self, ScalarValue::Float(f) if f.is_nan())
    }

    /// Parses user input (`1500`, `-3`, `0x10`, `12.5`, `1e3`).
    pub fn parse(input: &str) -> CoreResult<ScalarValue> {
        let s = input.trim().replace('_', "");
        let err = |reason: &str| CoreError::ValueParse {
            input: input.to_string(),
            target: "number".into(),
            reason: reason.into(),
        };
        if s.is_empty() {
            return Err(err("empty input"));
        }
        let (neg, body) = match s.strip_prefix('-') {
            Some(rest) => (true, rest.to_string()),
            None => (false, s.strip_prefix('+').unwrap_or(&s).to_string()),
        };
        if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
            let v = i128::from_str_radix(hex, 16).map_err(|e| err(&e.to_string()))?;
            return Ok(ScalarValue::Int(if neg { -v } else { v }));
        }
        if let Ok(v) = body.parse::<i128>() {
            return Ok(ScalarValue::Int(if neg { -v } else { v }));
        }
        let f = body.parse::<f64>().map_err(|e| err(&e.to_string()))?;
        Ok(ScalarValue::Float(if neg { -f } else { f }))
    }
}

impl fmt::Display for ScalarValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScalarValue::Int(i) => write!(f, "{i}"),
            ScalarValue::Float(x) => f.write_str(&format_float(*x)),
        }
    }
}

/// Formats a float compactly: integral values keep one decimal, others use
/// the shortest representation that round-trips.
pub fn format_float(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x.fract() == 0.0 && x.abs() < 1e16 {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

/// Formats an `f32` with the shortest representation that round-trips as `f32`.
pub fn format_f32(x: f32) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x.fract() == 0.0 && x.abs() < 1e7 {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

/// How a numeric string is formatted, so edits preserve its style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericStringStyle {
    /// `"1500"`
    Integer,
    /// `"12.50"` – number of decimals after the point.
    Fixed(usize),
    /// `"1.5e3"` – any float syntax.
    Float,
}

/// Parses a string that contains only a number (used for numeric strings in
/// AST leaves, e.g. JSON `"gold": "1500"`).
pub fn parse_numeric_string(s: &str) -> Option<(ScalarValue, NumericStringStyle)> {
    let t = s.trim();
    if t.is_empty() || t.len() > 40 || t != s {
        return None;
    }
    let digits = t.strip_prefix('-').unwrap_or(t);
    if digits.is_empty() {
        return None;
    }
    if digits.bytes().all(|b| b.is_ascii_digit()) {
        // Reject numbers with leading zeros (they are usually identifiers like "007").
        if digits.len() > 1 && digits.starts_with('0') {
            return None;
        }
        return t
            .parse::<i128>()
            .ok()
            .map(|v| (ScalarValue::Int(v), NumericStringStyle::Integer));
    }
    if let Some((int, frac)) = digits.split_once('.') {
        if !int.is_empty()
            && !frac.is_empty()
            && int.bytes().all(|b| b.is_ascii_digit())
            && frac.bytes().all(|b| b.is_ascii_digit())
        {
            return t
                .parse::<f64>()
                .ok()
                .map(|v| (ScalarValue::Float(v), NumericStringStyle::Fixed(frac.len())));
        }
    }
    let lower = digits.to_ascii_lowercase();
    if lower.contains('e')
        && lower
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'e' | b'.' | b'-' | b'+'))
    {
        return t
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .map(|v| (ScalarValue::Float(v), NumericStringStyle::Float));
    }
    None
}

/// Formats a number in a given numeric-string style.
pub fn format_numeric_string(value: ScalarValue, style: NumericStringStyle) -> String {
    match style {
        NumericStringStyle::Integer => match value.as_i128() {
            Some(i) => i.to_string(),
            None => format!("{}", value.as_f64().round() as i128),
        },
        NumericStringStyle::Fixed(d) => format!("{:.*}", d, value.as_f64()),
        NumericStringStyle::Float => match value {
            ScalarValue::Int(i) => i.to_string(),
            ScalarValue::Float(f) => format!("{f}"),
        },
    }
}

impl Node {
    /// Kind of this node.
    pub fn kind(&self) -> NodeKind {
        match self {
            Node::Null => NodeKind::Null,
            Node::Bool(_) => NodeKind::Bool,
            Node::I8(_) => NodeKind::I8,
            Node::I16(_) => NodeKind::I16,
            Node::I32(_) => NodeKind::I32,
            Node::I64(_) => NodeKind::I64,
            Node::U8(_) => NodeKind::U8,
            Node::U16(_) => NodeKind::U16,
            Node::U32(_) => NodeKind::U32,
            Node::U64(_) => NodeKind::U64,
            Node::F32(_) => NodeKind::F32,
            Node::F64(_) => NodeKind::F64,
            Node::String(_) => NodeKind::String,
            Node::Bytes(_) => NodeKind::Bytes,
            Node::Array(_) => NodeKind::Array,
            Node::Map(_) => NodeKind::Map,
            Node::RawRegion(_) => NodeKind::RawRegion,
        }
    }

    /// Creates an empty map node.
    pub fn map() -> Node {
        Node::Map(NodeMap::new())
    }

    /// Builds a map node from `(key, value)` pairs, preserving order.
    pub fn map_from<K: Into<String>>(pairs: impl IntoIterator<Item = (K, Node)>) -> Node {
        Node::Map(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// True for arrays and maps.
    pub fn is_container(&self) -> bool {
        self.kind().is_container()
    }

    /// True for anything that is not a container.
    pub fn is_leaf(&self) -> bool {
        !self.is_container()
    }

    /// Numeric value of integer/float nodes (and booleans as 0/1).
    pub fn as_scalar(&self) -> Option<ScalarValue> {
        Some(match self {
            Node::I8(v) => ScalarValue::Int(*v as i128),
            Node::I16(v) => ScalarValue::Int(*v as i128),
            Node::I32(v) => ScalarValue::Int(*v as i128),
            Node::I64(v) => ScalarValue::Int(*v as i128),
            Node::U8(v) => ScalarValue::Int(*v as i128),
            Node::U16(v) => ScalarValue::Int(*v as i128),
            Node::U32(v) => ScalarValue::Int(*v as i128),
            Node::U64(v) => ScalarValue::Int(*v as i128),
            Node::F32(v) => ScalarValue::Float(*v as f64),
            Node::F64(v) => ScalarValue::Float(*v),
            _ => return None,
        })
    }

    /// Numeric value, also accepting numeric strings and booleans.
    pub fn as_scalar_lenient(&self) -> Option<ScalarValue> {
        match self {
            Node::String(s) => parse_numeric_string(s).map(|(v, _)| v),
            Node::Bool(b) => Some(ScalarValue::Int(*b as i128)),
            other => other.as_scalar(),
        }
    }

    /// Returns a node of the *same type* holding `value`, with range checking.
    ///
    /// Numeric strings keep their formatting style; booleans accept 0/1.
    pub fn with_scalar(&self, value: ScalarValue, path: &NodePath) -> CoreResult<Node> {
        let out_of_range = |target: &str| CoreError::ValueOutOfRange {
            path: path.to_string(),
            value: value.to_string(),
            target: target.to_string(),
        };
        let int = |target: &str| -> CoreResult<i128> {
            match value {
                ScalarValue::Int(i) => Ok(i),
                ScalarValue::Float(f) if f.is_finite() && f.fract() == 0.0 => Ok(f as i128),
                ScalarValue::Float(_) => Err(CoreError::TypeMismatch {
                    path: path.to_string(),
                    expected: target.to_string(),
                    found: "non-integral float".into(),
                }),
            }
        };
        macro_rules! int_node {
            ($variant:ident, $ty:ty, $name:literal) => {{
                let i = int($name)?;
                <$ty>::try_from(i)
                    .map(Node::$variant)
                    .map_err(|_| out_of_range($name))
            }};
        }
        match self {
            Node::I8(_) => int_node!(I8, i8, "i8"),
            Node::I16(_) => int_node!(I16, i16, "i16"),
            Node::I32(_) => int_node!(I32, i32, "i32"),
            Node::I64(_) => int_node!(I64, i64, "i64"),
            Node::U8(_) => int_node!(U8, u8, "u8"),
            Node::U16(_) => int_node!(U16, u16, "u16"),
            Node::U32(_) => int_node!(U32, u32, "u32"),
            Node::U64(_) => int_node!(U64, u64, "u64"),
            Node::F32(_) => {
                let f = value.as_f64();
                if f.is_finite() && f.abs() > f32::MAX as f64 {
                    return Err(out_of_range("f32"));
                }
                Ok(Node::F32(f as f32))
            }
            Node::F64(_) => Ok(Node::F64(value.as_f64())),
            Node::Bool(_) => match value.as_i128() {
                Some(0) => Ok(Node::Bool(false)),
                Some(1) => Ok(Node::Bool(true)),
                _ => Err(out_of_range("bool (0 or 1)")),
            },
            Node::String(s) => {
                let style = parse_numeric_string(s)
                    .map(|(_, st)| st)
                    .unwrap_or(match value {
                        ScalarValue::Int(_) => NumericStringStyle::Integer,
                        ScalarValue::Float(_) => NumericStringStyle::Float,
                    });
                Ok(Node::String(format_numeric_string(value, style)))
            }
            other => Err(CoreError::TypeMismatch {
                path: path.to_string(),
                expected: "numeric node".into(),
                found: other.kind().to_string(),
            }),
        }
    }

    /// Parses `input` into a node with the same type as `self`.
    pub fn parse_like(&self, input: &str, path: &NodePath) -> CoreResult<Node> {
        match self {
            Node::String(s) if parse_numeric_string(s).is_none() => Ok(Node::String(input.to_string())),
            Node::String(_) => {
                // Numeric strings accept either a number (style preserved) or free text.
                match ScalarValue::parse(input) {
                    Ok(v) => self.with_scalar(v, path),
                    Err(_) => Ok(Node::String(input.to_string())),
                }
            }
            Node::Bool(_) => match input.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => Ok(Node::Bool(true)),
                "false" | "0" | "no" | "off" => Ok(Node::Bool(false)),
                _ => Err(CoreError::ValueParse {
                    input: input.to_string(),
                    target: "bool".into(),
                    reason: "expected true/false".into(),
                }),
            },
            Node::Null => match input.trim() {
                "null" | "" => Ok(Node::Null),
                _ => Err(CoreError::ValueParse {
                    input: input.to_string(),
                    target: "null".into(),
                    reason: "null nodes only accept `null`".into(),
                }),
            },
            Node::Bytes(_) => parse_hex_bytes(input).map(Node::Bytes),
            n if n.kind().is_numeric() => self.with_scalar(ScalarValue::parse(input)?, path),
            other => Err(CoreError::TypeMismatch {
                path: path.to_string(),
                expected: "scalar node".into(),
                found: other.kind().to_string(),
            }),
        }
    }

    /// Number of direct children.
    pub fn child_count(&self) -> usize {
        match self {
            Node::Array(a) => a.len(),
            Node::Map(m) => m.len(),
            _ => 0,
        }
    }

    /// Total number of nodes in the subtree (including `self`).
    pub fn node_count(&self) -> usize {
        1 + match self {
            Node::Array(a) => a.iter().map(Node::node_count).sum(),
            Node::Map(m) => m.values().map(Node::node_count).sum(),
            _ => 0,
        }
    }

    /// Resolves a single child segment.
    pub fn child(&self, seg: &PathSeg) -> Option<&Node> {
        match (self, seg) {
            (Node::Array(a), PathSeg::Index(i)) => a.get(*i),
            (Node::Map(m), PathSeg::Key(k)) => m.get(k),
            _ => None,
        }
    }

    /// Mutable variant of [`Node::child`].
    pub fn child_mut(&mut self, seg: &PathSeg) -> Option<&mut Node> {
        match (self, seg) {
            (Node::Array(a), PathSeg::Index(i)) => a.get_mut(*i),
            (Node::Map(m), PathSeg::Key(k)) => m.get_mut(k),
            _ => None,
        }
    }

    /// Looks up a node by path.
    pub fn get(&self, path: &NodePath) -> Option<&Node> {
        let mut cur = self;
        for seg in path.segments() {
            cur = cur.child(seg)?;
        }
        Some(cur)
    }

    /// Mutable lookup by path.
    pub fn get_mut(&mut self, path: &NodePath) -> Option<&mut Node> {
        let mut cur = self;
        for seg in path.segments() {
            cur = cur.child_mut(seg)?;
        }
        Some(cur)
    }

    /// Looks up a node by path, producing a descriptive error.
    pub fn resolve(&self, path: &NodePath) -> CoreResult<&Node> {
        let mut cur = self;
        let mut walked = NodePath::root();
        for seg in path.segments() {
            cur = step(cur, seg, &walked)?;
            walked.push(seg.clone());
        }
        Ok(cur)
    }

    /// Mutable variant of [`Node::resolve`].
    pub fn resolve_mut(&mut self, path: &NodePath) -> CoreResult<&mut Node> {
        let mut cur = self;
        let mut walked = NodePath::root();
        for seg in path.segments() {
            // Validate first with an immutable borrow for the error message.
            step(cur, seg, &walked)?;
            cur = cur.child_mut(seg).ok_or_else(|| CoreError::PathNotFound {
                path: walked.to_string(),
            })?;
            walked.push(seg.clone());
        }
        Ok(cur)
    }

    /// Replaces the node at `path`. With `create`, a missing last map key is inserted.
    pub fn set(&mut self, path: &NodePath, value: Node, create: bool) -> CoreResult<Option<Node>> {
        let Some(last) = path.last() else {
            return Ok(Some(std::mem::replace(self, value)));
        };
        let parent_path = path.parent().unwrap_or_default();
        let parent = self.resolve_mut(&parent_path)?;
        match (parent, last) {
            (Node::Map(m), PathSeg::Key(k)) => {
                if let Some(slot) = m.get_mut(k) {
                    Ok(Some(std::mem::replace(slot, value)))
                } else if create {
                    m.insert(k.clone(), value);
                    Ok(None)
                } else {
                    Err(CoreError::PathNotFound {
                        path: path.to_string(),
                    })
                }
            }
            (Node::Array(a), PathSeg::Index(i)) => {
                let len = a.len();
                if let Some(slot) = a.get_mut(*i) {
                    Ok(Some(std::mem::replace(slot, value)))
                } else if create && *i == len {
                    a.push(value);
                    Ok(None)
                } else {
                    Err(CoreError::IndexOutOfBounds {
                        path: parent_path.to_string(),
                        index: *i,
                        len,
                    })
                }
            }
            (other, _) => Err(CoreError::NotAContainer {
                path: parent_path.to_string(),
                kind: other.kind().name(),
            }),
        }
    }

    /// Removes the node at `path` and returns it.
    pub fn remove(&mut self, path: &NodePath) -> CoreResult<Node> {
        let Some(last) = path.last() else {
            return Err(CoreError::PathNotFound { path: "$".into() });
        };
        let parent_path = path.parent().unwrap_or_default();
        let parent = self.resolve_mut(&parent_path)?;
        match (parent, last) {
            (Node::Map(m), PathSeg::Key(k)) => m.shift_remove(k).ok_or_else(|| CoreError::PathNotFound {
                path: path.to_string(),
            }),
            (Node::Array(a), PathSeg::Index(i)) => {
                if *i < a.len() {
                    Ok(a.remove(*i))
                } else {
                    Err(CoreError::IndexOutOfBounds {
                        path: parent_path.to_string(),
                        index: *i,
                        len: a.len(),
                    })
                }
            }
            (other, _) => Err(CoreError::NotAContainer {
                path: parent_path.to_string(),
                kind: other.kind().name(),
            }),
        }
    }

    /// Depth-first pre-order traversal with paths.
    pub fn walk<F: FnMut(&NodePath, &Node)>(&self, mut f: F) {
        let mut path = NodePath::root();
        walk_inner(self, &mut path, &mut f);
    }

    /// Collects all `(path, leaf)` pairs.
    pub fn leaves(&self) -> Vec<(NodePath, &Node)> {
        let mut out = Vec::new();
        let mut path = NodePath::root();
        collect_leaves(self, &mut path, &mut out);
        out
    }

    /// Short single-line rendering of a value (containers are summarised).
    pub fn preview(&self, max_len: usize) -> String {
        let s = match self {
            Node::Null => "null".to_string(),
            Node::Bool(b) => b.to_string(),
            Node::I8(v) => v.to_string(),
            Node::I16(v) => v.to_string(),
            Node::I32(v) => v.to_string(),
            Node::I64(v) => v.to_string(),
            Node::U8(v) => v.to_string(),
            Node::U16(v) => v.to_string(),
            Node::U32(v) => v.to_string(),
            Node::U64(v) => v.to_string(),
            Node::F32(v) => format_f32(*v),
            Node::F64(v) => format_float(*v),
            Node::String(s) => format!("{s:?}"),
            Node::Bytes(b) => {
                let shown: String = b.iter().take(16).map(|x| format!("{x:02x}")).collect();
                if b.len() > 16 {
                    format!("<{} bytes: {shown}…>", b.len())
                } else {
                    format!("<{} bytes: {shown}>", b.len())
                }
            }
            Node::Array(a) => format!("[{} items]", a.len()),
            Node::Map(m) => format!("{{{} fields}}", m.len()),
            Node::RawRegion(r) => format!("<raw region: {} bytes>", r.len()),
        };
        truncate_chars(&s, max_len)
    }
}

fn step<'a>(cur: &'a Node, seg: &PathSeg, walked: &NodePath) -> CoreResult<&'a Node> {
    match (cur, seg) {
        (Node::Array(a), PathSeg::Index(i)) => a.get(*i).ok_or_else(|| CoreError::IndexOutOfBounds {
            path: walked.to_string(),
            index: *i,
            len: a.len(),
        }),
        (Node::Map(m), PathSeg::Key(k)) => m.get(k).ok_or_else(|| CoreError::PathNotFound {
            path: walked.key(k.clone()).to_string(),
        }),
        (Node::Array(_), PathSeg::Key(k)) => Err(CoreError::TypeMismatch {
            path: walked.to_string(),
            expected: format!("map (for key `{k}`)"),
            found: "array".into(),
        }),
        (Node::Map(_), PathSeg::Index(i)) => Err(CoreError::TypeMismatch {
            path: walked.to_string(),
            expected: format!("array (for index {i})"),
            found: "map".into(),
        }),
        (other, _) => Err(CoreError::NotAContainer {
            path: walked.to_string(),
            kind: other.kind().name(),
        }),
    }
}

fn walk_inner<F: FnMut(&NodePath, &Node)>(node: &Node, path: &mut NodePath, f: &mut F) {
    f(path, node);
    match node {
        Node::Array(a) => {
            for (i, child) in a.iter().enumerate() {
                path.push(PathSeg::Index(i));
                walk_inner(child, path, f);
                path.pop();
            }
        }
        Node::Map(m) => {
            for (k, child) in m {
                path.push(PathSeg::Key(k.clone()));
                walk_inner(child, path, f);
                path.pop();
            }
        }
        _ => {}
    }
}

fn collect_leaves<'a>(node: &'a Node, path: &mut NodePath, out: &mut Vec<(NodePath, &'a Node)>) {
    match node {
        Node::Array(a) => {
            for (i, child) in a.iter().enumerate() {
                path.push(PathSeg::Index(i));
                collect_leaves(child, path, out);
                path.pop();
            }
        }
        Node::Map(m) => {
            for (k, child) in m {
                path.push(PathSeg::Key(k.clone()));
                collect_leaves(child, path, out);
                path.pop();
            }
        }
        leaf => out.push((path.clone(), leaf)),
    }
}

/// Truncates on a char boundary, appending `…` when shortened.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Parses `de ad be ef`, `deadbeef` or `0xDEADBEEF` into bytes.
pub fn parse_hex_bytes(input: &str) -> CoreResult<Vec<u8>> {
    let cleaned: String = input
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X")
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ':' && *c != '-')
        .collect();
    hex::decode(&cleaned).map_err(|e| CoreError::ValueParse {
        input: input.to_string(),
        target: "hex bytes".into(),
        reason: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Node {
        Node::map_from([
            (
                "Player",
                Node::map_from([
                    ("Gold", Node::I32(1200)),
                    ("Name", Node::String("Hero".into())),
                    (
                        "Inventory",
                        Node::Array(vec![Node::String("Sword".into()), Node::String("Shield".into())]),
                    ),
                ]),
            ),
            ("Version", Node::U16(3)),
        ])
    }

    #[test]
    fn get_set_remove() {
        let mut n = sample();
        let p = NodePath::parse("Player.Gold").unwrap();
        assert_eq!(n.get(&p), Some(&Node::I32(1200)));
        let old = n.set(&p, Node::I32(1500), false).unwrap();
        assert_eq!(old, Some(Node::I32(1200)));
        assert_eq!(n.get(&p), Some(&Node::I32(1500)));
        let inv = NodePath::parse("Player.Inventory[1]").unwrap();
        assert_eq!(n.remove(&inv).unwrap(), Node::String("Shield".into()));
        assert!(n.get(&inv).is_none());
        let newkey = NodePath::parse("Player.Level").unwrap();
        assert!(n.set(&newkey, Node::I32(1), false).is_err());
        n.set(&newkey, Node::I32(1), true).unwrap();
        assert_eq!(n.get(&newkey), Some(&Node::I32(1)));
    }

    #[test]
    fn resolve_errors_are_descriptive() {
        let n = sample();
        let err = n.resolve(&NodePath::parse("Player.Gold.X").unwrap()).unwrap_err();
        assert!(matches!(err, CoreError::NotAContainer { .. }));
        let err = n.resolve(&NodePath::parse("Player.Inventory[9]").unwrap()).unwrap_err();
        assert!(matches!(err, CoreError::IndexOutOfBounds { index: 9, len: 2, .. }));
        let err = n.resolve(&NodePath::parse("Nope").unwrap()).unwrap_err();
        assert!(matches!(err, CoreError::PathNotFound { .. }));
    }

    #[test]
    fn with_scalar_range_checks() {
        let p = NodePath::root();
        assert_eq!(Node::U8(0).with_scalar(ScalarValue::Int(255), &p).unwrap(), Node::U8(255));
        assert!(Node::U8(0).with_scalar(ScalarValue::Int(256), &p).is_err());
        assert!(Node::U8(0).with_scalar(ScalarValue::Int(-1), &p).is_err());
        assert!(Node::I32(0).with_scalar(ScalarValue::Float(1.5), &p).is_err());
        assert_eq!(Node::I32(0).with_scalar(ScalarValue::Float(2.0), &p).unwrap(), Node::I32(2));
        assert_eq!(Node::F32(0.0).with_scalar(ScalarValue::Int(3), &p).unwrap(), Node::F32(3.0));
        assert_eq!(
            Node::String("12.50".into()).with_scalar(ScalarValue::Float(3.0), &p).unwrap(),
            Node::String("3.00".into())
        );
        assert_eq!(
            Node::String("100".into()).with_scalar(ScalarValue::Int(99999), &p).unwrap(),
            Node::String("99999".into())
        );
        assert!(Node::Map(NodeMap::new()).with_scalar(ScalarValue::Int(1), &p).is_err());
    }

    #[test]
    fn scalar_parse() {
        assert_eq!(ScalarValue::parse("1500").unwrap(), ScalarValue::Int(1500));
        assert_eq!(ScalarValue::parse("-3").unwrap(), ScalarValue::Int(-3));
        assert_eq!(ScalarValue::parse("0x10").unwrap(), ScalarValue::Int(16));
        assert_eq!(ScalarValue::parse("1_000").unwrap(), ScalarValue::Int(1000));
        assert_eq!(ScalarValue::parse("12.5").unwrap(), ScalarValue::Float(12.5));
        assert!(ScalarValue::parse("abc").is_err());
        assert!(ScalarValue::parse("").is_err());
    }

    #[test]
    fn numeric_strings() {
        assert_eq!(
            parse_numeric_string("1500"),
            Some((ScalarValue::Int(1500), NumericStringStyle::Integer))
        );
        assert_eq!(parse_numeric_string("007"), None);
        assert_eq!(parse_numeric_string(" 5"), None);
        assert_eq!(
            parse_numeric_string("1.25"),
            Some((ScalarValue::Float(1.25), NumericStringStyle::Fixed(2)))
        );
        assert!(parse_numeric_string("1e3").is_some());
        assert!(parse_numeric_string("e").is_none());
        assert!(parse_numeric_string("hello").is_none());
    }

    #[test]
    fn parse_like_types() {
        let p = NodePath::root();
        assert_eq!(Node::Bool(false).parse_like("true", &p).unwrap(), Node::Bool(true));
        assert_eq!(Node::I16(0).parse_like("-7", &p).unwrap(), Node::I16(-7));
        assert_eq!(Node::String("x".into()).parse_like("hello", &p).unwrap(), Node::String("hello".into()));
        assert_eq!(Node::Bytes(vec![]).parse_like("de ad", &p).unwrap(), Node::Bytes(vec![0xde, 0xad]));
        assert!(Node::Array(vec![]).parse_like("1", &p).is_err());
    }

    #[test]
    fn walk_and_leaves() {
        let n = sample();
        let leaves = n.leaves();
        let paths: Vec<String> = leaves.iter().map(|(p, _)| p.to_string()).collect();
        assert_eq!(
            paths,
            vec![
                "Player.Gold",
                "Player.Name",
                "Player.Inventory[0]",
                "Player.Inventory[1]",
                "Version"
            ]
        );
        let mut count = 0;
        n.walk(|_, _| count += 1);
        assert_eq!(count, n.node_count());
    }

    #[test]
    fn preview_truncates() {
        assert_eq!(Node::String("abcdefghij".into()).preview(5), "\"abc…");
        assert_eq!(Node::Bytes(vec![1, 2]).preview(100), "<2 bytes: 0102>");
    }
}
