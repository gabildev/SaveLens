//! Error type of the discovery crate.

use std::path::PathBuf;

/// Every failure the discovery layer can report.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// Malformed Valve KeyValues (VDF/ACF) text.
    #[error("VDF parse error at line {line}: {message}")]
    Vdf {
        /// 1-based line number where the problem was detected.
        line: usize,
        /// Human readable description.
        message: String,
    },
    /// An I/O operation on a specific path failed.
    #[error("I/O error on {}: {source}", path.display())]
    Io {
        /// Path that was being accessed.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// A save-rule database (builtin or user supplied) is invalid.
    #[error("invalid save rules in {origin}: {message}")]
    Rules {
        /// `builtin` or the path of the user rule file.
        origin: String,
        /// Description of the problem.
        message: String,
    },
    /// A path template produced an invalid glob pattern.
    #[error("invalid glob pattern `{pattern}`: {message}")]
    Glob {
        /// The offending pattern.
        pattern: String,
        /// Description from the glob crate.
        message: String,
    },
    /// The file system watcher could not be created.
    #[error("file watcher error: {0}")]
    Watch(#[from] notify::Error),
}

impl DiscoveryError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        DiscoveryError::Io { path: path.into(), source }
    }
}
