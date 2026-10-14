//! # savelens-core
//!
//! Pure, I/O-free building blocks of SaveLens:
//!
//! * [`Node`] / [`SaveAst`] — the typed, order-preserving semantic tree of a save
//!   file and its [`FormatEnvelope`] (crypto, checksum, compression metadata).
//! * [`merkle`] — `blake3` Merkle hashing of every node and sub-tree.
//! * [`diff`] — semantic diff engine producing human changelogs.
//! * [`scanner`] — multi-step save-state scanner over AST leaves and raw bytes.
//! * [`cherry`] — surgical cherry-pick of sub-trees between snapshots.

pub mod cherry;
pub mod diff;
pub mod envelope;
pub mod error;
pub mod merkle;
pub mod node;
pub mod path;
pub mod raw;
pub mod scanner;

pub use cherry::{cherry_pick, CherryPickMode, CherryPickOptions, CherryPickReport};
pub use diff::{diff, diff_with, AliasResolver, Change, ChangeKind, DiffOptions, NoAliases, SemanticDiff};
pub use envelope::{
    Base64Variant, ChecksumAlgorithm, ChecksumRule, CompressionAlgorithm, CryptoScheme, DigestEncoding, EditMode,
    FieldAnchor, FormatEnvelope, LayerDescriptor, RoundTripStatus, SaveAst, SizePrefix,
};
pub use error::{CoreError, CoreResult};
pub use merkle::{bit_equal, hash_node, Hash32, MerkleTree};
pub use node::{Node, NodeKind, NodeMap, ScalarValue};
pub use path::{NodePath, PathSeg};
pub use raw::{Endianness, RawRegion, ScanType};
pub use scanner::{Alignment, CandidateView, Location, PinnedVariable, ScanConfig, ScanFilter, ScanSession};
