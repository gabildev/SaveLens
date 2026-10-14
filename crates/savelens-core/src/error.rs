//! Error type shared by every module of `savelens-core`.

use thiserror::Error;

/// Result alias used throughout the core crate.
pub type CoreResult<T> = Result<T, CoreError>;

/// Errors produced by AST navigation, editing, scanning and cherry-picking.
#[derive(Debug, Error, Clone, PartialEq)]
pub enum CoreError {
    /// A textual path could not be parsed.
    #[error("invalid path `{input}` at byte {position}: {reason}")]
    PathParse { input: String, position: usize, reason: String },

    /// A path does not resolve to a node.
    #[error("path `{path}` does not exist")]
    PathNotFound { path: String },

    /// A path segment tried to descend into a scalar.
    #[error("cannot descend into `{path}`: node is a {kind}, not a container")]
    NotAContainer { path: String, kind: &'static str },

    /// An array index is past the end of the array.
    #[error("index {index} out of bounds at `{path}` (length {len})")]
    IndexOutOfBounds { path: String, index: usize, len: usize },

    /// A value of one type was supplied where another was required.
    #[error("type mismatch at `{path}`: expected {expected}, found {found}")]
    TypeMismatch { path: String, expected: String, found: String },

    /// A numeric value does not fit into the destination type.
    #[error("value {value} does not fit into {target} at `{path}`")]
    ValueOutOfRange { path: String, value: String, target: String },

    /// A raw-byte location points outside of the region.
    #[error("raw offset {offset:#x} (+{width} bytes) is outside region `{path}` of {len} bytes")]
    RawOffsetOutOfRange { path: String, offset: u64, width: usize, len: usize },

    /// A value could not be parsed from user input.
    #[error("cannot parse `{input}` as {target}: {reason}")]
    ValueParse { input: String, target: String, reason: String },

    /// The scanner was asked to do something invalid for its current state.
    #[error("scanner: {0}")]
    Scan(String),

    /// The scan would exceed the configured candidate budget.
    #[error(
        "scan produced more than {limit} candidates; narrow the search (exact value, fewer types or aligned scan)"
    )]
    TooManyCandidates { limit: usize },

    /// Cherry-pick refused to merge incompatible sub-trees.
    #[error("cherry-pick of `{path}` rejected: {reason}")]
    CherryPick { path: String, reason: String },
}
