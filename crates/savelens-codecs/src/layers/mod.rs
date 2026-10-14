//! Reversible envelope layers wrapping the structured payload.
//!
//! Each layer is created by peeling it off the outer bytes and records every
//! parameter needed to wrap a (possibly edited) inner payload again.

pub mod base64_layer;
pub mod checksum;
pub mod compression;
pub mod crypto;

use std::fmt;

use savelens_core::{Hash32, LayerDescriptor};

use crate::error::CodecResult;

/// A reversible transformation between an outer and an inner payload.
pub trait Layer: Send + Sync {
    /// Descriptive metadata for the envelope.
    fn descriptor(&self) -> LayerDescriptor;

    /// Wraps an inner payload into outer bytes.
    fn wrap(&self, inner: &[u8]) -> CodecResult<Vec<u8>>;

    /// True when wrapping a *modified* payload is bit-exact with what the game
    /// itself would produce (deterministic layer with recovered parameters).
    fn exact_reencode(&self) -> bool;

    /// Clones the layer behind a box.
    fn clone_box(&self) -> Box<dyn Layer>;
}

impl Clone for Box<dyn Layer> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

impl fmt::Debug for dyn Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Layer({})", self.descriptor())
    }
}

/// Remembers the original outer bytes and the hash of the original inner
/// payload so that an unmodified payload is re-wrapped to the identical
/// original bytes even for non-deterministic encoders (e.g. compressors whose
/// exact parameters could not be recovered).
#[derive(Debug, Clone)]
pub struct Identity {
    inner_hash: Hash32,
    inner_len: usize,
    outer: Vec<u8>,
}

impl Identity {
    /// Records an `(outer, inner)` pair.
    pub fn new(outer: &[u8], inner: &[u8]) -> Self {
        Identity {
            inner_hash: Hash32::of_bytes(inner),
            inner_len: inner.len(),
            outer: outer.to_vec(),
        }
    }

    /// Returns the original outer bytes when `inner` is the original payload.
    pub fn lookup(&self, inner: &[u8]) -> Option<Vec<u8>> {
        if inner.len() == self.inner_len && Hash32::of_bytes(inner) == self.inner_hash {
            Some(self.outer.clone())
        } else {
            None
        }
    }
}

/// Bytes kept verbatim before and after a wrapped payload (e.g. data trailing
/// a compressed stream).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Framing {
    pub prefix: Vec<u8>,
    pub suffix: Vec<u8>,
}

impl Framing {
    /// True when there is nothing to keep.
    pub fn is_empty(&self) -> bool {
        self.prefix.is_empty() && self.suffix.is_empty()
    }

    /// Surrounds `body` with the framing bytes.
    pub fn apply(&self, body: Vec<u8>) -> Vec<u8> {
        if self.is_empty() {
            return body;
        }
        let mut out = Vec::with_capacity(self.prefix.len() + body.len() + self.suffix.len());
        out.extend_from_slice(&self.prefix);
        out.extend_from_slice(&body);
        out.extend_from_slice(&self.suffix);
        out
    }
}
