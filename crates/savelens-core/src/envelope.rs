//! `FormatEnvelope`: the descriptive metadata of every reversible layer that
//! wraps the structured payload of a save file, plus the `SaveAst` container.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::node::Node;
use crate::raw::Endianness;

/// Encryption / obfuscation schemes understood by the codec pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CryptoScheme {
    Aes128Cbc,
    Aes256Cbc,
    Aes128Ecb,
    Aes256Ecb,
    Aes128Ctr,
    Aes256Ctr,
    XorFixed,
    XorRolling,
    /// Unity Easy Save 3: 16-byte IV prefix, PBKDF2-HMAC-SHA1 (100 iters) → AES-128-CBC.
    Es3,
}

impl CryptoScheme {
    /// Display name.
    pub fn name(self) -> &'static str {
        match self {
            CryptoScheme::Aes128Cbc => "AES-128-CBC",
            CryptoScheme::Aes256Cbc => "AES-256-CBC",
            CryptoScheme::Aes128Ecb => "AES-128-ECB",
            CryptoScheme::Aes256Ecb => "AES-256-ECB",
            CryptoScheme::Aes128Ctr => "AES-128-CTR",
            CryptoScheme::Aes256Ctr => "AES-256-CTR",
            CryptoScheme::XorFixed => "XOR (fixed key)",
            CryptoScheme::XorRolling => "XOR (rolling key)",
            CryptoScheme::Es3 => "Easy Save 3 (PBKDF2 + AES-128-CBC)",
        }
    }
}

/// Checksum / digest algorithms the checksum hunter can detect and recompute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChecksumAlgorithm {
    Crc32,
    Crc32c,
    Adler32,
    Fnv1a32,
    Fnv1a64,
    Md5,
    Sha1,
    Sha256,
}

impl ChecksumAlgorithm {
    /// All algorithms, cheapest first.
    pub const ALL: [ChecksumAlgorithm; 8] = [
        ChecksumAlgorithm::Crc32,
        ChecksumAlgorithm::Crc32c,
        ChecksumAlgorithm::Adler32,
        ChecksumAlgorithm::Fnv1a32,
        ChecksumAlgorithm::Fnv1a64,
        ChecksumAlgorithm::Md5,
        ChecksumAlgorithm::Sha1,
        ChecksumAlgorithm::Sha256,
    ];

    /// Digest size in bytes.
    pub fn digest_len(self) -> usize {
        match self {
            ChecksumAlgorithm::Crc32
            | ChecksumAlgorithm::Crc32c
            | ChecksumAlgorithm::Adler32
            | ChecksumAlgorithm::Fnv1a32 => 4,
            ChecksumAlgorithm::Fnv1a64 => 8,
            ChecksumAlgorithm::Md5 => 16,
            ChecksumAlgorithm::Sha1 => 20,
            ChecksumAlgorithm::Sha256 => 32,
        }
    }

    /// Display name.
    pub fn name(self) -> &'static str {
        match self {
            ChecksumAlgorithm::Crc32 => "CRC32",
            ChecksumAlgorithm::Crc32c => "CRC32C",
            ChecksumAlgorithm::Adler32 => "Adler32",
            ChecksumAlgorithm::Fnv1a32 => "FNV-1a/32",
            ChecksumAlgorithm::Fnv1a64 => "FNV-1a/64",
            ChecksumAlgorithm::Md5 => "MD5",
            ChecksumAlgorithm::Sha1 => "SHA1",
            ChecksumAlgorithm::Sha256 => "SHA256",
        }
    }
}

/// How a digest is stored in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DigestEncoding {
    /// Raw digest bytes; integer checksums stored little-endian.
    RawLe,
    /// Raw digest bytes; integer checksums stored big-endian (also the natural
    /// order for MD5/SHA digests).
    RawBe,
    /// Lowercase ASCII hex.
    HexLower,
    /// Uppercase ASCII hex.
    HexUpper,
}

impl DigestEncoding {
    /// Encoded length for a digest of `digest_len` bytes.
    pub fn encoded_len(self, digest_len: usize) -> usize {
        match self {
            DigestEncoding::RawLe | DigestEncoding::RawBe => digest_len,
            DigestEncoding::HexLower | DigestEncoding::HexUpper => digest_len * 2,
        }
    }
}

/// Where the checksum field lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "anchor", content = "offset")]
pub enum FieldAnchor {
    /// Field begins `n` bytes after the start of the layer input.
    FromStart(usize),
    /// Field ends `n` bytes before the end of the layer input.
    FromEnd(usize),
}

/// A detected checksum rule. Offsets of the covered range refer to the layer
/// input *with the checksum field removed* (the "stripped payload").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChecksumRule {
    pub algorithm: ChecksumAlgorithm,
    pub encoding: DigestEncoding,
    pub field: FieldAnchor,
    /// Bytes skipped at the start of the stripped payload before hashing.
    pub covered_skip_start: usize,
    /// Bytes skipped at the end of the stripped payload.
    pub covered_skip_end: usize,
}

impl fmt::Display for ChecksumRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let place = match self.field {
            FieldAnchor::FromStart(0) => "header".to_string(),
            FieldAnchor::FromStart(n) => format!("header+{n}"),
            FieldAnchor::FromEnd(0) => "trailer".to_string(),
            FieldAnchor::FromEnd(n) => format!("trailer-{n}"),
        };
        write!(
            f,
            "{} ({:?}) in {place}, covering payload[{}..len-{}]",
            self.algorithm.name(),
            self.encoding,
            self.covered_skip_start,
            self.covered_skip_end
        )
    }
}

/// Compression algorithms of the compression wrapper layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompressionAlgorithm {
    Zlib,
    Gzip,
    Deflate,
    Lz4Frame,
    Lz4Block,
    Zstd,
}

impl CompressionAlgorithm {
    /// Display name.
    pub fn name(self) -> &'static str {
        match self {
            CompressionAlgorithm::Zlib => "Zlib",
            CompressionAlgorithm::Gzip => "Gzip",
            CompressionAlgorithm::Deflate => "Deflate",
            CompressionAlgorithm::Lz4Frame => "LZ4 Frame",
            CompressionAlgorithm::Lz4Block => "LZ4 Block",
            CompressionAlgorithm::Zstd => "Zstd",
        }
    }
}

/// Uncompressed-size prefix placed before a compressed stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SizePrefix {
    U32Le,
    U32Be,
    U64Le,
    U64Be,
}

impl SizePrefix {
    /// Width of the prefix in bytes.
    pub fn width(self) -> usize {
        match self {
            SizePrefix::U32Le | SizePrefix::U32Be => 4,
            SizePrefix::U64Le | SizePrefix::U64Be => 8,
        }
    }
}

/// Base64 alphabet / padding variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Base64Variant {
    Standard,
    StandardNoPad,
    UrlSafe,
    UrlSafeNoPad,
}

/// One reversible transformation wrapping the structured payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "layer", rename_all = "kebab-case")]
pub enum LayerDescriptor {
    Crypto {
        scheme: CryptoScheme,
        /// Name of the `game_keys.toml` profile used.
        profile: String,
    },
    Checksum {
        rule: ChecksumRule,
    },
    Compression {
        algorithm: CompressionAlgorithm,
        size_prefix: Option<SizePrefix>,
        /// Compression level that reproduces the original stream, if found.
        level: Option<i32>,
        /// True when re-compressing the original payload reproduces the original bytes.
        exact: bool,
    },
    Base64 {
        variant: Base64Variant,
        /// Line wrap width used by the original text (0 = none).
        line_width: usize,
    },
    /// Bytes preceding/following the payload that are kept verbatim.
    Framing {
        prefix_len: usize,
        suffix_len: usize,
    },
}

impl fmt::Display for LayerDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayerDescriptor::Crypto { scheme, profile } => write!(f, "Crypto: {} [{profile}]", scheme.name()),
            LayerDescriptor::Checksum { rule } => write!(f, "Checksum Auto-Matched: {}", rule.algorithm.name()),
            LayerDescriptor::Compression { algorithm, size_prefix, level, exact } => {
                write!(f, "Compression: {}", algorithm.name())?;
                if let Some(p) = size_prefix {
                    write!(f, " (+{:?} size prefix)", p)?;
                }
                if let Some(l) = level {
                    write!(f, " level {l}")?;
                }
                if !exact {
                    write!(f, " [re-encode not bit-exact]")?;
                }
                Ok(())
            }
            LayerDescriptor::Base64 { variant, .. } => write!(f, "Base64 ({variant:?})"),
            LayerDescriptor::Framing { prefix_len, suffix_len } => {
                write!(f, "Framing: {prefix_len} prefix / {suffix_len} suffix bytes")
            }
        }
    }
}

/// Result of the in-memory `serialize(deserialize(bytes)) == bytes` check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoundTripStatus {
    /// Bit-for-bit identical.
    Lossless,
    /// Structural serializer differs from the original at `first_diff_offset`.
    Lossy { first_diff_offset: usize, original_len: usize, reencoded_len: usize },
    /// Re-encoding failed with an error.
    Failed(String),
}

impl RoundTripStatus {
    /// True when structural edits are allowed.
    pub fn is_lossless(&self) -> bool {
        matches!(self, RoundTripStatus::Lossless)
    }
}

/// Which editing modes the Safe Edit Gate allows for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditMode {
    /// Full structural editing through the AST.
    Structural,
    /// Only raw in-place edits (fixed-width writes) with checksum recalculation.
    RawInPlace,
    /// Only whole-file backup / restore.
    BackupRestoreOnly,
}

/// Metadata needed to describe (and reconstruct) the encoding of a save.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormatEnvelope {
    /// Structural codec id (`json`, `gvas`, `nrbf`, `raw`, …).
    pub codec: String,
    /// Layers from outermost (applied last when writing) to innermost.
    pub layers: Vec<LayerDescriptor>,
    /// Dominant byte order of the structured payload.
    pub endianness: Endianness,
    /// Result of the round-trip gate, if it ran.
    pub round_trip: Option<RoundTripStatus>,
    /// Shannon entropy (bits/byte) of the raw file.
    pub entropy: f64,
    /// Free-form notes from detectors (warnings, heuristics used).
    pub notes: Vec<String>,
}

impl FormatEnvelope {
    /// Envelope for a payload with no layers.
    pub fn plain(codec: impl Into<String>) -> Self {
        FormatEnvelope {
            codec: codec.into(),
            layers: Vec::new(),
            endianness: Endianness::Little,
            round_trip: None,
            entropy: 0.0,
            notes: Vec::new(),
        }
    }

    /// Edit modes allowed by the safe edit gate.
    pub fn allowed_edit_mode(&self) -> EditMode {
        match &self.round_trip {
            Some(RoundTripStatus::Lossless) => EditMode::Structural,
            Some(RoundTripStatus::Lossy { .. }) => EditMode::RawInPlace,
            Some(RoundTripStatus::Failed(_)) => EditMode::BackupRestoreOnly,
            None => EditMode::BackupRestoreOnly,
        }
    }

    /// Integrity badges for UIs (`Lossless Verified`, `Checksum Auto-Matched: CRC32`, …).
    pub fn badges(&self) -> Vec<String> {
        let mut out = Vec::new();
        match &self.round_trip {
            Some(RoundTripStatus::Lossless) => out.push("Lossless Verified".to_string()),
            Some(RoundTripStatus::Lossy { first_diff_offset, .. }) => {
                out.push(format!("Lossy round-trip (first diff @ {first_diff_offset:#x})"))
            }
            Some(RoundTripStatus::Failed(e)) => out.push(format!("Round-trip failed: {e}")),
            None => out.push("Round-trip not verified".to_string()),
        }
        for l in &self.layers {
            out.push(l.to_string());
        }
        out.push(format!("Codec: {}", self.codec));
        out
    }
}

/// The semantic tree of a save file plus its envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SaveAst {
    pub root: Node,
    pub envelope: FormatEnvelope,
}

impl SaveAst {
    /// Creates a new AST.
    pub fn new(root: Node, envelope: FormatEnvelope) -> Self {
        SaveAst { root, envelope }
    }

    /// Merkle root hash of the tree.
    pub fn merkle_root(&self) -> crate::merkle::Hash32 {
        crate::merkle::hash_node(&self.root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_mode_from_round_trip() {
        let mut e = FormatEnvelope::plain("json");
        assert_eq!(e.allowed_edit_mode(), EditMode::BackupRestoreOnly);
        e.round_trip = Some(RoundTripStatus::Lossless);
        assert_eq!(e.allowed_edit_mode(), EditMode::Structural);
        e.round_trip = Some(RoundTripStatus::Lossy { first_diff_offset: 3, original_len: 10, reencoded_len: 10 });
        assert_eq!(e.allowed_edit_mode(), EditMode::RawInPlace);
    }

    #[test]
    fn badges_list_layers() {
        let mut e = FormatEnvelope::plain("gvas");
        e.round_trip = Some(RoundTripStatus::Lossless);
        e.layers.push(LayerDescriptor::Compression {
            algorithm: CompressionAlgorithm::Zlib,
            size_prefix: None,
            level: Some(6),
            exact: true,
        });
        e.layers.push(LayerDescriptor::Checksum {
            rule: ChecksumRule {
                algorithm: ChecksumAlgorithm::Crc32,
                encoding: DigestEncoding::RawLe,
                field: FieldAnchor::FromEnd(0),
                covered_skip_start: 0,
                covered_skip_end: 0,
            },
        });
        let b = e.badges();
        assert_eq!(b[0], "Lossless Verified");
        assert!(b.iter().any(|s| s == "Compression: Zlib level 6"));
        assert!(b.iter().any(|s| s == "Checksum Auto-Matched: CRC32"));
    }

    #[test]
    fn envelope_serde_roundtrip() {
        let mut e = FormatEnvelope::plain("raw");
        e.layers.push(LayerDescriptor::Crypto { scheme: CryptoScheme::Es3, profile: "es3-default".into() });
        let s = serde_json::to_string(&e).unwrap();
        let back: FormatEnvelope = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
    }
}
