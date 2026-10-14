//! Structural codec traits.
//!
//! A [`StructuralCodec`] detects and parses a payload into a codec-native
//! [`Document`] that keeps every layout detail needed to re-serialise the
//! payload bit for bit. The [`Node`] tree returned by [`Document::to_ast`] is a
//! projection of that document; edits come back through
//! [`Document::apply_ast`].

use std::fmt;

use savelens_core::diff::{diff, ChangeKind};
use savelens_core::{Endianness, Node, NodePath};

use crate::error::{CodecError, CodecResult};

/// Detection confidence, 0 (no) to 100 (certain).
pub type Confidence = u8;

/// Information available while detecting a codec.
#[derive(Debug, Clone, Default)]
pub struct DetectContext {
    /// File name (without directory), if known.
    pub file_name: Option<String>,
    /// Steam AppID of the game, if known.
    pub app_id: Option<u32>,
}

impl DetectContext {
    /// Context for a file name.
    pub fn for_file(name: impl Into<String>) -> Self {
        DetectContext {
            file_name: Some(name.into()),
            app_id: None,
        }
    }

    /// Lower-case file extension without the dot.
    pub fn extension(&self) -> Option<String> {
        let name = self.file_name.as_deref()?;
        let (_, ext) = name.rsplit_once('.')?;
        Some(ext.to_ascii_lowercase())
    }
}

/// What kinds of edits a document supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Scalar values can be changed.
    pub edit_values: bool,
    /// Map keys / array elements can be added or removed.
    pub edit_structure: bool,
}

impl Capabilities {
    pub const VALUES_ONLY: Capabilities = Capabilities {
        edit_values: true,
        edit_structure: false,
    };
    pub const FULL: Capabilities = Capabilities {
        edit_values: true,
        edit_structure: true,
    };
}

/// A parsed, lossless, editable representation of a payload.
pub trait Document: Send + Sync {
    /// Id of the codec that produced it.
    fn codec_id(&self) -> &'static str;

    /// Semantic projection of the document.
    fn to_ast(&self) -> Node;

    /// Applies an edited projection. Implementations must reject edits they
    /// cannot represent instead of silently dropping them.
    fn apply_ast(&mut self, root: &Node) -> CodecResult<()>;

    /// Serialises the document. Must reproduce the original payload exactly
    /// when no edit was applied.
    fn encode(&self) -> CodecResult<Vec<u8>>;

    /// Supported edit kinds.
    fn capabilities(&self) -> Capabilities {
        Capabilities::VALUES_ONLY
    }

    /// Dominant byte order of the payload.
    fn endianness(&self) -> Endianness {
        Endianness::Little
    }

    /// Codec-specific notes for the envelope (version info, warnings).
    fn notes(&self) -> Vec<String> {
        Vec::new()
    }

    /// Clones the document behind a box.
    fn clone_box(&self) -> Box<dyn Document>;
}

impl Clone for Box<dyn Document> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

impl fmt::Debug for dyn Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Document({})", self.codec_id())
    }
}

/// A structural codec.
pub trait StructuralCodec: Send + Sync {
    /// Stable id (`json`, `gvas`, …).
    fn id(&self) -> &'static str;

    /// Human-readable name.
    fn name(&self) -> &'static str;

    /// How confident the codec is that it can parse `bytes`.
    fn detect(&self, bytes: &[u8], ctx: &DetectContext) -> Confidence;

    /// Parses `bytes`.
    fn decode(&self, bytes: &[u8], ctx: &DetectContext) -> CodecResult<Box<dyn Document>>;
}

/// Operations used by [`apply_via_diff`] for documents whose native model is
/// addressed by path.
pub trait PathEditable {
    /// Codec id for error messages.
    fn codec(&self) -> &'static str;
    /// Replaces the value at `path`. Scalars must keep their field type.
    fn set_value(&mut self, path: &NodePath, value: &Node) -> CodecResult<()>;
    /// Inserts a new node (map key or array element).
    fn insert(&mut self, path: &NodePath, value: &Node) -> CodecResult<()> {
        let _ = value;
        Err(CodecError::structure(self.codec(), path, "this format does not support adding fields"))
    }
    /// Removes a node.
    fn remove(&mut self, path: &NodePath) -> CodecResult<()> {
        Err(CodecError::structure(self.codec(), path, "this format does not support removing fields"))
    }
}

/// Generic `apply_ast` implementation: diffs the current projection against
/// the edited one and replays each change through [`PathEditable`].
///
/// Array length changes are replayed as a single `set_value` on the array so
/// that index shifts never misplace elements.
pub fn apply_via_diff<D: PathEditable>(doc: &mut D, current: &Node, edited: &Node) -> CodecResult<()> {
    let d = diff(current, edited);
    // Arrays whose length changed are replaced wholesale.
    let mut replaced_arrays: Vec<NodePath> = Vec::new();
    for c in &d.changes {
        if matches!(c.kind, ChangeKind::Added { .. } | ChangeKind::Removed { .. }) {
            if let Some(parent) = c.path.parent() {
                if matches!(current.get(&parent), Some(Node::Array(_))) && !replaced_arrays.iter().any(|p| p.is_prefix_of(&parent)) {
                    replaced_arrays.retain(|p| !parent.is_prefix_of(p));
                    replaced_arrays.push(parent);
                }
            }
        }
    }
    for p in &replaced_arrays {
        let new = edited.get(p).ok_or_else(|| CodecError::path_not_found(doc.codec(), p))?;
        doc.set_value(p, new)?;
    }
    for c in &d.changes {
        if replaced_arrays.iter().any(|p| p.is_prefix_of(&c.path)) {
            continue;
        }
        match &c.kind {
            ChangeKind::Modified { new, .. } | ChangeKind::TypeChanged { new, .. } => doc.set_value(&c.path, new)?,
            ChangeKind::BytesChanged { .. } => {
                let new = edited.get(&c.path).ok_or_else(|| CodecError::path_not_found(doc.codec(), &c.path))?;
                doc.set_value(&c.path, new)?;
            }
            ChangeKind::Added { new } => doc.insert(&c.path, new)?,
            ChangeKind::Removed { .. } => doc.remove(&c.path)?,
        }
    }
    Ok(())
}

/// Converts a scalar edit into the exact type of the existing field, accepting
/// numerically-compatible nodes (e.g. an `I64` edit for an `I32` field) with
/// range checking.
pub fn coerce_like(codec: &'static str, path: &NodePath, existing: &Node, edit: &Node) -> CodecResult<Node> {
    if existing.kind() == edit.kind() {
        return Ok(edit.clone());
    }
    if existing.kind().is_numeric() || matches!(existing, Node::Bool(_)) {
        if let Some(v) = edit.as_scalar_lenient() {
            return existing.with_scalar(v, path).map_err(CodecError::from);
        }
    }
    if let (Node::Bytes(_), Node::RawRegion(r)) = (existing, edit) {
        return Ok(Node::Bytes(r.bytes.clone()));
    }
    Err(CodecError::type_mismatch(codec, path, existing.kind().name(), edit.kind().name()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use savelens_core::NodeMap;

    /// A trivial map-of-scalars document used to exercise `apply_via_diff`.
    struct Flat {
        root: Node,
    }

    impl PathEditable for Flat {
        fn codec(&self) -> &'static str {
            "flat"
        }
        fn set_value(&mut self, path: &NodePath, value: &Node) -> CodecResult<()> {
            let existing = self.root.resolve(path)?.clone();
            let v = if existing.is_container() { value.clone() } else { coerce_like("flat", path, &existing, value)? };
            self.root.set(path, v, false)?;
            Ok(())
        }
        fn insert(&mut self, path: &NodePath, value: &Node) -> CodecResult<()> {
            self.root.set(path, value.clone(), true)?;
            Ok(())
        }
        fn remove(&mut self, path: &NodePath) -> CodecResult<()> {
            self.root.remove(path)?;
            Ok(())
        }
    }

    #[test]
    fn replays_changes() {
        let current = Node::map_from([
            ("a", Node::I32(1)),
            ("b", Node::String("x".into())),
            ("arr", Node::Array(vec![Node::I32(1), Node::I32(2)])),
        ]);
        let mut doc = Flat { root: current.clone() };
        let edited = Node::map_from([
            ("a", Node::I32(5)),
            ("arr", Node::Array(vec![Node::I32(9), Node::I32(1), Node::I32(2)])),
            ("c", Node::Bool(true)),
        ]);
        apply_via_diff(&mut doc, &current, &edited).unwrap();
        assert_eq!(doc.root, edited);
    }

    #[test]
    fn coerce_checks_range() {
        let p = NodePath::root();
        assert_eq!(coerce_like("t", &p, &Node::I32(0), &Node::I64(5)).unwrap(), Node::I32(5));
        assert!(coerce_like("t", &p, &Node::U8(0), &Node::I64(500)).is_err());
        assert!(coerce_like("t", &p, &Node::I32(0), &Node::String("x".into())).is_err());
        assert!(coerce_like("t", &p, &Node::Map(NodeMap::new()), &Node::I32(1)).is_err());
    }
}
