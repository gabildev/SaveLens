//! Building blocks for container formats (ZIP, BND4, Anvil regions, …).
//!
//! A container stores several payloads; each payload may itself be
//! compressed, encrypted, signed or structured. [`NestedEntry`] opens a
//! payload through the full [`EnvelopePipeline`](crate::pipeline::EnvelopePipeline)
//! (taken from the [`DetectContext`]) and guarantees that an entry that was
//! not edited is written back with its *original bytes*, whatever its codec.

use savelens_core::{bit_equal, EditMode, Node};

use crate::document::DetectContext;
use crate::error::CodecResult;
use crate::pipeline::OpenedSave;

/// One payload of a container, opened through the pipeline.
#[derive(Debug, Clone)]
pub struct NestedEntry {
    /// Entry name inside the container.
    pub name: String,
    original: Vec<u8>,
    opened: Option<OpenedSave>,
    dirty: bool,
}

impl NestedEntry {
    /// Opens `bytes` as entry `name`. Entries whose round trip is not
    /// bit-exact are switched to raw in-place mode so they can still be
    /// edited safely. Beyond the nesting limit entries stay opaque bytes.
    pub fn open(ctx: &DetectContext, name: &str, bytes: &[u8]) -> NestedEntry {
        let opened = if ctx.can_nest() {
            let nested = ctx.nested(name);
            let pipeline = nested.pipeline();
            let opened = match pipeline.open(bytes, &nested) {
                Ok(o) => o,
                Err(_) => pipeline.open_raw(bytes, &nested),
            };
            Some(if opened.edit_mode() == EditMode::Structural { opened } else { opened.into_raw_mode() })
        } else {
            None
        };
        NestedEntry { name: name.to_string(), original: bytes.to_vec(), opened, dirty: false }
    }

    /// Original bytes of the entry.
    pub fn original(&self) -> &[u8] {
        &self.original
    }

    /// Codec id of the entry (`bytes` when it was not opened).
    pub fn codec_id(&self) -> &'static str {
        self.opened.as_ref().map(OpenedSave::codec_id).unwrap_or("bytes")
    }

    /// The opened entry, if any.
    pub fn opened(&self) -> Option<&OpenedSave> {
        self.opened.as_ref()
    }

    /// True once an edit changed the entry.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Semantic tree of the entry (`Bytes` for opaque entries).
    pub fn to_ast(&self) -> Node {
        match &self.opened {
            Some(o) => o.root(),
            None => Node::Bytes(self.original.clone()),
        }
    }

    /// Applies an edited tree; unchanged trees leave the entry untouched.
    pub fn apply_ast(&mut self, node: &Node) -> CodecResult<()> {
        if bit_equal(&self.to_ast(), node) {
            return Ok(());
        }
        match &mut self.opened {
            Some(o) => {
                let mut work = o.clone();
                work.apply_ast(node)?;
                *o = work;
            }
            None => match node {
                Node::Bytes(b) => self.original = b.clone(),
                other => {
                    return Err(crate::error::CodecError::type_mismatch(
                        "container",
                        &self.name,
                        "bytes",
                        other.kind().name(),
                    ))
                }
            },
        }
        self.dirty = true;
        Ok(())
    }

    /// Bytes to store in the container: the original bytes when the entry was
    /// not edited, otherwise the re-encoded entry (all layers re-applied).
    pub fn encode(&self) -> CodecResult<Vec<u8>> {
        match (&self.opened, self.dirty) {
            (Some(o), true) => o.encode(),
            _ => Ok(self.original.clone()),
        }
    }

    /// Short description for document notes (`save.json: json, Zlib`).
    pub fn describe(&self) -> String {
        match &self.opened {
            Some(o) => {
                let layers: Vec<String> = o.layers().iter().map(|l| l.to_string()).collect();
                if layers.is_empty() {
                    format!("{}: {}", self.name, o.codec_id())
                } else {
                    format!("{}: {} via {}", self.name, o.codec_id(), layers.join(" → "))
                }
            }
            None => format!("{}: opaque ({} bytes)", self.name, self.original.len()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use savelens_core::NodePath;

    #[test]
    fn unedited_entries_keep_their_bytes_and_edits_reencode_layers() {
        let json = br#"{"gold":10,"name":"x"}"#;
        let z = crate::layers::compression::deflate(json, 6, true).unwrap();
        let ctx = DetectContext::for_file("container.bin");
        let mut e = NestedEntry::open(&ctx, "dir/save.json", &z);
        assert_eq!(e.codec_id(), "json");
        assert!(e.describe().contains("Zlib"));
        assert_eq!(e.encode().unwrap(), z);
        let mut ast = e.to_ast();
        e.apply_ast(&ast).unwrap();
        assert!(!e.is_dirty(), "identical tree is not an edit");
        ast.set(&NodePath::parse("gold").unwrap(), Node::I64(999), false).unwrap();
        e.apply_ast(&ast).unwrap();
        assert!(e.is_dirty());
        let out = e.encode().unwrap();
        let back = crate::layers::compression::CompressionLayer::detect(&out).unwrap().1;
        assert_eq!(back, br#"{"gold":999,"name":"x"}"#);
    }

    #[test]
    fn nesting_limit_keeps_entries_opaque() {
        let ctx = DetectContext { depth: crate::document::MAX_NESTING, ..DetectContext::default() };
        let mut e = NestedEntry::open(&ctx, "a", b"{\"x\":1}");
        assert_eq!(e.codec_id(), "bytes");
        assert_eq!(e.to_ast(), Node::Bytes(b"{\"x\":1}".to_vec()));
        e.apply_ast(&Node::Bytes(b"{\"x\":2}".to_vec())).unwrap();
        assert_eq!(e.encode().unwrap(), b"{\"x\":2}");
        assert!(e.apply_ast(&Node::I32(1)).is_err());
    }
}
