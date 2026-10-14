//! Structural codecs.

pub mod json;
pub mod raw;

use std::sync::Arc;

use crate::document::StructuralCodec;

/// Every built-in structural codec, raw fallback last.
pub fn builtin() -> Vec<Arc<dyn StructuralCodec>> {
    vec![Arc::new(json::JsonCodec), Arc::new(raw::RawCodec)]
}
