//! Engine errors.

use std::path::PathBuf;

use thiserror::Error;

/// Result alias.
pub type EngineResult<T> = Result<T, EngineError>;

/// Errors of the engine, daemon and hotkeys.
#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] savelens_cas::CasError),

    #[error(transparent)]
    Codec(#[from] savelens_codecs::CodecError),

    #[error(transparent)]
    Core(#[from] savelens_core::CoreError),

    #[error("discovery: {0}")]
    Discovery(String),

    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The Safe Edit Gate refused a structural edit.
    #[error("safe edit gate: {0}")]
    GateBlocked(String),

    /// The game is writing the file right now.
    #[error("{0} is open for writing by another process (is the game saving?); try again in a moment")]
    Busy(PathBuf),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid request: {0}")]
    Invalid(String),

    #[error("configuration: {0}")]
    Config(String),

    #[error("watcher: {0}")]
    Watch(String),

    #[error("hotkeys: {0}")]
    Hotkeys(String),
}

impl EngineError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        EngineError::Io {
            path: path.into(),
            source,
        }
    }
}

impl From<notify::Error> for EngineError {
    fn from(e: notify::Error) -> Self {
        EngineError::Watch(e.to_string())
    }
}
