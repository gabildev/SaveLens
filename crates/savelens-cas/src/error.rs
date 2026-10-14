//! Errors of the content-addressable store.

use std::path::PathBuf;

use thiserror::Error;

/// Result alias.
pub type CasResult<T> = Result<T, CasError>;

/// Store errors.
#[derive(Debug, Error)]
pub enum CasError {
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),

    /// A chunk or blob does not match its content hash.
    #[error("integrity violation: {0}")]
    Integrity(String),

    /// A referenced object does not exist.
    #[error("{kind} `{id}` not found")]
    NotFound { kind: &'static str, id: String },

    /// The operation conflicts with existing state.
    #[error("conflict: {0}")]
    Conflict(String),

    /// Invalid argument (bad branch name, path outside the save root, …).
    #[error("invalid argument: {0}")]
    Invalid(String),

    /// The pre-mutation safety backup could not be verified; the mutation was aborted.
    #[error("safety backup of {path} failed verification: {reason}; mutation aborted, file untouched")]
    SafetyBackupFailed { path: PathBuf, reason: String },

    /// The written file did not read back identically; the original was restored.
    #[error("write verification of {path} failed ({reason}); original content restored from {tag}")]
    WriteVerificationFailed { path: PathBuf, reason: String, tag: String },

    /// Nothing to restore.
    #[error("panic restore: {0}")]
    NothingToRestore(String),
}

impl CasError {
    /// Wraps an I/O error with its path.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        CasError::Io {
            path: path.into(),
            source,
        }
    }
}
