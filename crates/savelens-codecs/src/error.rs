//! Error type of the codec pipeline.

use savelens_core::CoreError;
use thiserror::Error;

/// Result alias for codec operations.
pub type CodecResult<T> = Result<T, CodecError>;

/// Everything that can go wrong while detecting, decoding, editing or
/// encoding a save file.
#[derive(Debug, Error)]
pub enum CodecError {
    /// The input ended before a structure was complete.
    #[error("{codec}: unexpected end of data while reading {context} at offset {offset:#x} (need {needed} more bytes)")]
    Truncated {
        codec: &'static str,
        context: String,
        offset: usize,
        needed: usize,
    },

    /// The input violates the format.
    #[error("{codec}: malformed data at offset {offset:#x}: {reason}")]
    Malformed {
        codec: &'static str,
        offset: usize,
        reason: String,
    },

    /// A valid feature of the format that SaveLens deliberately does not handle.
    #[error("{codec}: unsupported feature: {feature}")]
    Unsupported { codec: &'static str, feature: String },

    /// An AST edit has the wrong type for the underlying field.
    #[error("{codec}: type mismatch at `{path}`: field is {expected}, edit provides {found}")]
    TypeMismatch {
        codec: &'static str,
        path: String,
        expected: String,
        found: String,
    },

    /// An AST edit references a path the document does not have.
    #[error("{codec}: path `{path}` does not exist in the document")]
    PathNotFound { codec: &'static str, path: String },

    /// The codec cannot add/remove/reshape nodes at this path.
    #[error("{codec}: structural change at `{path}` is not supported: {reason}")]
    StructureUnsupported {
        codec: &'static str,
        path: String,
        reason: String,
    },

    /// A value cannot be represented by the field (range / encoding).
    #[error("{codec}: value for `{path}` cannot be encoded: {reason}")]
    Unencodable {
        codec: &'static str,
        path: String,
        reason: String,
    },

    /// Decryption / encryption failure.
    #[error("crypto ({scheme}): {reason}")]
    Crypto { scheme: String, reason: String },

    /// Compression / decompression failure.
    #[error("compression ({algorithm}): {reason}")]
    Compression { algorithm: String, reason: String },

    /// Checksum layer failure.
    #[error("checksum: {0}")]
    Checksum(String),

    /// The key database is invalid.
    #[error("game_keys.toml: {0}")]
    KeyDatabase(String),

    /// No codec could make sense of the data (should not happen: raw always works).
    #[error("no codec accepted the payload: {0}")]
    NoCodec(String),

    #[error(transparent)]
    Core(#[from] CoreError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

impl CodecError {
    /// Convenience constructor for [`CodecError::Malformed`].
    pub fn malformed(codec: &'static str, offset: usize, reason: impl Into<String>) -> Self {
        CodecError::Malformed {
            codec,
            offset,
            reason: reason.into(),
        }
    }

    /// Convenience constructor for [`CodecError::Unsupported`].
    pub fn unsupported(codec: &'static str, feature: impl Into<String>) -> Self {
        CodecError::Unsupported {
            codec,
            feature: feature.into(),
        }
    }

    /// Convenience constructor for [`CodecError::TypeMismatch`].
    pub fn type_mismatch(codec: &'static str, path: impl ToString, expected: impl Into<String>, found: impl Into<String>) -> Self {
        CodecError::TypeMismatch {
            codec,
            path: path.to_string(),
            expected: expected.into(),
            found: found.into(),
        }
    }

    /// Convenience constructor for [`CodecError::PathNotFound`].
    pub fn path_not_found(codec: &'static str, path: impl ToString) -> Self {
        CodecError::PathNotFound {
            codec,
            path: path.to_string(),
        }
    }

    /// Convenience constructor for [`CodecError::StructureUnsupported`].
    pub fn structure(codec: &'static str, path: impl ToString, reason: impl Into<String>) -> Self {
        CodecError::StructureUnsupported {
            codec,
            path: path.to_string(),
            reason: reason.into(),
        }
    }

    /// Convenience constructor for [`CodecError::Unencodable`].
    pub fn unencodable(codec: &'static str, path: impl ToString, reason: impl Into<String>) -> Self {
        CodecError::Unencodable {
            codec,
            path: path.to_string(),
            reason: reason.into(),
        }
    }
}
