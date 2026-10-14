//! # savelens-cas
//!
//! SaveLens' own content-addressable store (no Git): files are split with
//! FastCDC, chunks are compressed with zstd and addressed by blake3, and the
//! commit graph, branches, tags, semantic changelogs, cheat table and
//! safety backups live in SQLite. Everything is stored under the user data
//! directory, never inside a game's folder.
//!
//! The [`safety`] module implements the zero-corruption mutation protocol
//! (`safety/pre-mutation-<timestamp>` shadow backups, atomic staged writes,
//! read-back verification and Panic Restore).

pub mod chunks;
pub mod error;
pub mod model;
pub mod safety;
pub mod store;
pub mod watch;

pub use error::{CasError, CasResult};
pub use model::*;
pub use safety::{MutationSummary, PanicReport, PanicScope, WriteMethod};
pub use store::{default_data_dir, Store};
pub use watch::{AskOutcome, QuestionStatus, WatchMute, WatchQuestion};
