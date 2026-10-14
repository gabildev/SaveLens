//! Registry of structural codecs.

use std::sync::Arc;

use crate::codecs;
use crate::document::{Confidence, DetectContext, StructuralCodec};

/// Ordered collection of structural codecs.
#[derive(Clone)]
pub struct CodecRegistry {
    codecs: Vec<Arc<dyn StructuralCodec>>,
}

impl std::fmt::Debug for CodecRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.codecs.iter().map(|c| c.id())).finish()
    }
}

impl Default for CodecRegistry {
    fn default() -> Self {
        Self::with_defaults()
    }
}

impl CodecRegistry {
    /// Empty registry.
    pub fn empty() -> Self {
        CodecRegistry { codecs: Vec::new() }
    }

    /// Every built-in codec (the raw fallback last).
    pub fn with_defaults() -> Self {
        let mut r = CodecRegistry::empty();
        for c in codecs::builtin() {
            r.register(c);
        }
        r
    }

    /// Adds a codec (later registrations win ties).
    pub fn register(&mut self, codec: Arc<dyn StructuralCodec>) {
        self.codecs.retain(|c| c.id() != codec.id());
        self.codecs.push(codec);
    }

    /// Looks a codec up by id.
    pub fn get(&self, id: &str) -> Option<Arc<dyn StructuralCodec>> {
        self.codecs.iter().find(|c| c.id() == id).cloned()
    }

    /// Ids of all codecs.
    pub fn ids(&self) -> Vec<&'static str> {
        self.codecs.iter().map(|c| c.id()).collect()
    }

    /// `(id, name)` of all codecs.
    pub fn describe(&self) -> Vec<(&'static str, &'static str)> {
        self.codecs.iter().map(|c| (c.id(), c.name())).collect()
    }

    /// Codecs with a non-zero confidence, best first.
    pub fn rank(&self, bytes: &[u8], ctx: &DetectContext) -> Vec<(Confidence, Arc<dyn StructuralCodec>)> {
        let mut v: Vec<(Confidence, Arc<dyn StructuralCodec>)> = self
            .codecs
            .iter()
            .map(|c| (c.detect(bytes, ctx), c.clone()))
            .filter(|(conf, _)| *conf > 0)
            .collect();
        v.sort_by(|a, b| b.0.cmp(&a.0));
        v
    }

    /// Best non-raw confidence for `bytes`.
    pub fn best_structured_confidence(&self, bytes: &[u8], ctx: &DetectContext) -> Confidence {
        self.codecs
            .iter()
            .filter(|c| c.id() != "raw")
            .map(|c| c.detect(bytes, ctx))
            .max()
            .unwrap_or(0)
    }
}
