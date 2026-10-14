//! Raw binary fallback codec.
//!
//! Exposes unknown payloads as a single [`RawRegion`] (bytes + string index +
//! block hashes) so the save-state scanner can find and edit primitives at
//! any offset. Edits are in place: the payload length can never change, so a
//! raw edit can never shift the layout of a proprietary format.

use savelens_core::{Endianness, Node, RawRegion};

use crate::document::{Capabilities, Confidence, DetectContext, Document, StructuralCodec};
use crate::error::{CodecError, CodecResult};

const CODEC: &str = "raw";

/// Raw document.
#[derive(Debug, Clone)]
pub struct RawDocument {
    bytes: Vec<u8>,
    endian: Endianness,
}

impl RawDocument {
    /// Wraps bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        let endian = savelens_core::raw::guess_endianness(&bytes);
        RawDocument { bytes, endian }
    }

    /// Current bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Document for RawDocument {
    fn codec_id(&self) -> &'static str {
        CODEC
    }

    fn to_ast(&self) -> Node {
        Node::RawRegion(RawRegion::new(self.bytes.clone()))
    }

    fn apply_ast(&mut self, root: &Node) -> CodecResult<()> {
        let new = match root {
            Node::RawRegion(r) => &r.bytes,
            Node::Bytes(b) => b,
            other => {
                return Err(CodecError::type_mismatch(CODEC, "$", "raw-region", other.kind().name()));
            }
        };
        if new.len() != self.bytes.len() {
            return Err(CodecError::structure(
                CODEC,
                "$",
                format!(
                    "raw in-place edits must keep the payload size ({} bytes, edit has {})",
                    self.bytes.len(),
                    new.len()
                ),
            ));
        }
        self.bytes.copy_from_slice(new);
        Ok(())
    }

    fn encode(&self) -> CodecResult<Vec<u8>> {
        Ok(self.bytes.clone())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::VALUES_ONLY
    }

    fn endianness(&self) -> Endianness {
        self.endian
    }

    fn notes(&self) -> Vec<String> {
        vec![format!(
            "unknown format: raw byte-aligned scanner mode ({} bytes, {} endian guess)",
            self.bytes.len(),
            if self.endian == Endianness::Big { "big" } else { "little" }
        )]
    }

    fn clone_box(&self) -> Box<dyn Document> {
        Box::new(self.clone())
    }
}

/// Raw codec: accepts everything with the lowest confidence.
#[derive(Debug, Default, Clone, Copy)]
pub struct RawCodec;

impl StructuralCodec for RawCodec {
    fn id(&self) -> &'static str {
        CODEC
    }

    fn name(&self) -> &'static str {
        "Raw binary (byte-aligned scanner)"
    }

    fn detect(&self, _bytes: &[u8], _ctx: &DetectContext) -> Confidence {
        1
    }

    fn decode(&self, bytes: &[u8], _ctx: &DetectContext) -> CodecResult<Box<dyn Document>> {
        Ok(Box::new(RawDocument::new(bytes.to_vec())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use savelens_core::{Location, NodePath, ScalarValue, ScanType};

    #[test]
    fn roundtrip_and_in_place_edit() {
        let mut data = vec![0u8; 64];
        data[8..12].copy_from_slice(&1500i32.to_le_bytes());
        data[20..28].copy_from_slice(b"HeroName");
        let mut doc = RawDocument::new(data.clone());
        assert_eq!(doc.encode().unwrap(), data);
        let mut ast = doc.to_ast();
        match &ast {
            Node::RawRegion(r) => assert!(r.strings.iter().any(|s| s.text == "HeroName")),
            other => panic!("{other:?}"),
        }
        let loc = Location::Raw {
            region: NodePath::root(),
            offset: 8,
            ty: ScanType::I32,
            endian: Endianness::Little,
        };
        loc.write(&mut ast, ScalarValue::Int(99999)).unwrap();
        doc.apply_ast(&ast).unwrap();
        let out = doc.encode().unwrap();
        assert_eq!(&out[8..12], &99999i32.to_le_bytes());
        assert_eq!(out.len(), data.len());
    }

    #[test]
    fn size_change_rejected() {
        let mut doc = RawDocument::new(vec![1, 2, 3]);
        assert!(doc.apply_ast(&Node::Bytes(vec![1, 2])).is_err());
        assert!(doc.apply_ast(&Node::I32(1)).is_err());
        assert_eq!(doc.bytes(), &[1, 2, 3]);
    }
}
