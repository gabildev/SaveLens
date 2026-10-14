//! # savelens-codecs
//!
//! The reversible multi-layer pipeline that turns save files into editable
//! [`savelens_core::SaveAst`]s and back, bit for bit:
//!
//! 1. **Auto-Crypt** ([`layers::crypto`]): AES-CBC/ECB/CTR, XOR (fixed /
//!    rolling / brute-forced single byte) and Unity Easy Save 3, driven by
//!    `game_keys.toml`, plus a Shannon-entropy detector ([`entropy`]).
//! 2. **Smart Checksum Hunter** ([`layers::checksum`]).
//! 3. **Compression wrapper** ([`layers::compression`], [`layers::base64_layer`]).
//! 4. **Structural codecs** ([`codecs`]).
//!
//! Use [`EnvelopePipeline`] to open a file and [`OpenedSave`] to edit and
//! re-encode it.

pub mod codecs;
pub mod document;
pub mod entropy;
pub mod error;
pub mod io;
pub mod layers;
pub mod pipeline;
pub mod registry;

pub use document::{Capabilities, Confidence, DetectContext, Document, StructuralCodec};
pub use error::{CodecError, CodecResult};
pub use layers::crypto::keys::{KeyDatabase, KeyProfile};
pub use pipeline::{EnvelopePipeline, OpenedSave, PipelineOptions};
pub use registry::CodecRegistry;
