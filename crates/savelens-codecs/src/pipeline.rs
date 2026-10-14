//! The reversible `EnvelopePipeline`:
//! `Decrypt -> Strip checksum -> Decompress / Base64 -> Parse AST`.
//!
//! Layers are peeled iteratively until nothing more is recognised, then the
//! best structural codec parses the innermost payload. Writing applies the
//! layers in reverse order. Opening a file always runs the in-memory
//! round-trip gate (`serialize(deserialize(bytes)) == bytes`).

use std::sync::Arc;

use savelens_core::{
    EditMode, FormatEnvelope, LayerDescriptor, Location, Node, NodePath, RoundTripStatus, SaveAst, ScalarValue,
};

use crate::codecs::raw::RawDocument;
use crate::document::{DetectContext, Document};
use crate::entropy;
use crate::error::{CodecError, CodecResult};
use crate::layers::base64_layer::Base64Layer;
use crate::layers::checksum::ChecksumLayer;
use crate::layers::compression::CompressionLayer;
use crate::layers::crypto::keys::KeyDatabase;
use crate::layers::crypto::CryptoLayer;
use crate::layers::lzstring::LzStringLayer;
use crate::layers::plz::PlzLayer;
use crate::layers::Layer;
use crate::registry::CodecRegistry;

/// Pipeline options.
#[derive(Debug, Clone)]
pub struct PipelineOptions {
    /// Maximum number of envelope layers peeled.
    pub max_layers: usize,
    /// Brute-force single-byte XOR obfuscation.
    pub xor_bruteforce: bool,
    /// Run the checksum hunter.
    pub hunt_checksums: bool,
}

impl Default for PipelineOptions {
    fn default() -> Self {
        PipelineOptions { max_layers: 8, xor_bruteforce: true, hunt_checksums: true }
    }
}

/// Detects, decodes and re-encodes save files.
#[derive(Clone)]
pub struct EnvelopePipeline {
    registry: Arc<CodecRegistry>,
    keys: Arc<KeyDatabase>,
    options: PipelineOptions,
}

impl std::fmt::Debug for EnvelopePipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvelopePipeline")
            .field("codecs", &self.registry)
            .field("profiles", &self.keys.profiles().len())
            .field("options", &self.options)
            .finish()
    }
}

impl EnvelopePipeline {
    /// Pipeline with explicit registry and keys.
    pub fn new(registry: CodecRegistry, keys: KeyDatabase, options: PipelineOptions) -> Self {
        EnvelopePipeline { registry: Arc::new(registry), keys: Arc::new(keys), options }
    }

    /// Built-in codecs, built-in + user key profiles.
    pub fn with_defaults() -> CodecResult<Self> {
        Ok(Self::new(CodecRegistry::with_defaults(), KeyDatabase::load_default()?, PipelineOptions::default()))
    }

    /// Built-in codecs and only built-in keys (no filesystem access).
    pub fn builtin() -> Self {
        Self::new(CodecRegistry::with_defaults(), KeyDatabase::builtin(), PipelineOptions::default())
    }

    /// The codec registry.
    pub fn registry(&self) -> &CodecRegistry {
        &self.registry
    }

    /// The key database.
    pub fn keys(&self) -> &KeyDatabase {
        &self.keys
    }

    /// Cheap "is this a meaningful payload?" probe used to accept speculative
    /// layers (base64, crypto, XOR).
    fn recognized(&self, data: &[u8], ctx: &DetectContext, depth: usize) -> bool {
        if depth > self.options.max_layers || data.is_empty() {
            return false;
        }
        if self.registry.best_structured_confidence(data, ctx) >= 50 {
            return true;
        }
        if let Some((_, inner)) = CompressionLayer::detect(data) {
            return self.recognized(&inner, ctx, depth + 1) || !entropy::looks_random(&inner);
        }
        if self.options.hunt_checksums {
            if let Some((_, inner)) = ChecksumLayer::detect(data) {
                return self.recognized(&inner, ctx, depth + 1);
            }
        }
        if PlzLayer::detect(data).is_some() {
            return true;
        }
        self.try_specific_crypto(data, ctx, depth + 1).is_some()
    }

    /// Profiles selected by AppID or file name only (no generic / brute-force
    /// attempts): cheap enough to be used while probing speculative layers.
    fn try_specific_crypto(&self, data: &[u8], ctx: &DetectContext, depth: usize) -> Option<(Box<dyn Layer>, Vec<u8>)> {
        if depth > self.options.max_layers {
            return None;
        }
        for profile in self.keys.candidates(ctx) {
            let specific = ctx.app_id.is_some_and(|id| profile.app_ids.contains(&id))
                || ctx.file_name.as_deref().is_some_and(|n| profile.matches_file(n));
            if !specific {
                continue;
            }
            let Ok((layer, inner)) = CryptoLayer::open(profile, data) else { continue };
            if self.recognized(&inner, ctx, depth + 1) || entropy::shannon(&inner) < 6.0 {
                return Some((Box::new(layer), inner));
            }
        }
        None
    }

    fn quick_text_like(prefix: &[u8]) -> bool {
        let first = prefix.iter().find(|b| !b.is_ascii_whitespace());
        matches!(first, Some(b'{') | Some(b'[') | Some(b'<')) && entropy::printable_ratio(prefix) > 0.97
    }

    fn try_crypto(&self, data: &[u8], ctx: &DetectContext, depth: usize) -> Option<(Box<dyn Layer>, Vec<u8>)> {
        let random = entropy::looks_random(data);
        for profile in self.keys.candidates(ctx) {
            let specific = ctx.app_id.is_some_and(|id| profile.app_ids.contains(&id))
                || ctx.file_name.as_deref().is_some_and(|n| profile.matches_file(n));
            if profile.generic && !specific && !random {
                continue;
            }
            let Ok((layer, inner)) = CryptoLayer::open(profile, data) else { continue };
            if self.recognized(&inner, ctx, depth + 1) || (specific && entropy::shannon(&inner) < 6.0) {
                return Some((Box::new(layer), inner));
            }
        }
        if self.options.xor_bruteforce && data.len() >= 8 && !random {
            let prefix = &data[..data.len().min(256)];
            for key in 1..=255u8 {
                let probe: Vec<u8> = prefix.iter().map(|b| b ^ key).collect();
                if !Self::quick_text_like(&probe) {
                    continue;
                }
                let (layer, inner) = CryptoLayer::open_single_byte_xor(key, data);
                if self.recognized(&inner, ctx, depth + 1) {
                    return Some((Box::new(layer), inner));
                }
            }
        }
        None
    }

    /// Peels envelope layers. Returns layers (outermost first) and the payload.
    fn peel(&self, data: &[u8], ctx: &DetectContext) -> (Vec<Box<dyn Layer>>, Vec<u8>) {
        let mut layers: Vec<Box<dyn Layer>> = Vec::new();
        let mut cur = data.to_vec();
        while layers.len() < self.options.max_layers {
            let depth = layers.len();
            // A payload that a structural codec parses with high confidence
            // is never peeled further (avoids decoding e.g. base64 *values*).
            let structured = self.registry.best_structured_confidence(&cur, ctx) >= 70;
            if self.options.hunt_checksums && !structured {
                if let Some((l, inner)) = ChecksumLayer::detect(&cur) {
                    layers.push(Box::new(l));
                    cur = inner;
                    continue;
                }
            }
            if let Some((l, inner)) = CompressionLayer::detect(&cur) {
                layers.push(Box::new(l));
                cur = inner;
                continue;
            }
            if let Some((l, inner)) = PlzLayer::detect(&cur) {
                layers.push(Box::new(l));
                cur = inner;
                continue;
            }
            if structured {
                break;
            }
            if let Some((l, inner)) = Base64Layer::detect(&cur) {
                if self.recognized(&inner, ctx, depth + 1) {
                    layers.push(Box::new(l));
                    cur = inner;
                    continue;
                }
            }
            if let Some((l, inner)) = LzStringLayer::detect(&cur) {
                if self.recognized(&inner, ctx, depth + 1) {
                    layers.push(Box::new(l));
                    cur = inner;
                    continue;
                }
            }
            if let Some((l, inner)) = self.try_crypto(&cur, ctx, depth) {
                layers.push(l);
                cur = inner;
                continue;
            }
            break;
        }
        (layers, cur)
    }

    fn decode_payload(&self, payload: &[u8], ctx: &DetectContext, notes: &mut Vec<String>) -> Box<dyn Document> {
        for (conf, codec) in self.registry.rank(payload, ctx) {
            if codec.id() == "raw" {
                continue;
            }
            match codec.decode(payload, ctx) {
                Ok(doc) => return doc,
                Err(e) => notes.push(format!("{} (confidence {conf}) rejected the payload: {e}", codec.id())),
            }
        }
        Box::new(RawDocument::new(payload.to_vec()))
    }

    /// Copy of `ctx` with this pipeline attached (for container codecs).
    fn attach(&self, ctx: &DetectContext) -> DetectContext {
        let mut c = ctx.clone();
        if c.pipeline.is_none() {
            c.pipeline = Some(self.clone());
        }
        c
    }

    /// Opens a save file: peels layers, parses the payload and runs the
    /// round-trip gate.
    pub fn open(&self, data: &[u8], ctx: &DetectContext) -> CodecResult<OpenedSave> {
        let ctx = &self.attach(ctx);
        let (layers, payload) = self.peel(data, ctx);
        let mut notes = Vec::new();
        let doc = self.decode_payload(&payload, ctx, &mut notes);
        Ok(OpenedSave::assemble(data, layers, payload, doc, notes))
    }

    /// Opens with a forced structural codec (layers are still peeled).
    pub fn open_with_codec(&self, data: &[u8], ctx: &DetectContext, codec_id: &str) -> CodecResult<OpenedSave> {
        let ctx = &self.attach(ctx);
        let codec = self.registry.get(codec_id).ok_or_else(|| {
            CodecError::NoCodec(format!("unknown codec `{codec_id}`; available: {:?}", self.registry.ids()))
        })?;
        let (layers, payload) = self.peel(data, ctx);
        let doc = codec.decode(&payload, ctx)?;
        Ok(OpenedSave::assemble(data, layers, payload, doc, Vec::new()))
    }

    /// Opens with the raw codec (layers are still peeled).
    pub fn open_raw(&self, data: &[u8], ctx: &DetectContext) -> OpenedSave {
        let ctx = &self.attach(ctx);
        let (layers, payload) = self.peel(data, ctx);
        let doc: Box<dyn Document> = Box::new(RawDocument::new(payload.clone()));
        OpenedSave::assemble(data, layers, payload, doc, Vec::new())
    }
}

/// An opened save: layers + parsed document + envelope.
#[derive(Debug, Clone)]
pub struct OpenedSave {
    original: Arc<Vec<u8>>,
    layers: Vec<Box<dyn Layer>>,
    payload: Vec<u8>,
    doc: Box<dyn Document>,
    envelope: FormatEnvelope,
}

/// Wraps `payload` through `layers` (outermost first).
fn wrap_layers(layers: &[Box<dyn Layer>], payload: Vec<u8>) -> CodecResult<Vec<u8>> {
    let mut cur = payload;
    for layer in layers.iter().rev() {
        cur = layer.wrap(&cur)?;
    }
    Ok(cur)
}

fn first_difference(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()))
}

impl OpenedSave {
    fn assemble(
        data: &[u8],
        layers: Vec<Box<dyn Layer>>,
        payload: Vec<u8>,
        doc: Box<dyn Document>,
        mut notes: Vec<String>,
    ) -> OpenedSave {
        let ent = entropy::shannon(data);
        notes.push(format!("entropy {ent:.2} bits/byte: {}", entropy::describe(ent)));
        for l in &layers {
            if !l.exact_reencode() {
                notes.push(format!("{}: edits will produce a valid but not bit-identical stream", l.descriptor()));
            }
        }
        notes.extend(doc.notes());
        let envelope = FormatEnvelope {
            codec: doc.codec_id().to_string(),
            layers: layers.iter().map(|l| l.descriptor()).collect(),
            endianness: doc.endianness(),
            round_trip: None,
            entropy: ent,
            notes,
        };
        let mut s = OpenedSave { original: Arc::new(data.to_vec()), layers, payload, doc, envelope };
        let status = s.verify_round_trip();
        if !status.is_lossless() {
            s.envelope
                .notes
                .push("Safe Edit Gate: structural round-trip is not bit-exact; only raw in-place edits or full backup/restore are allowed".into());
        }
        s.envelope.round_trip = Some(status);
        s
    }

    /// The original file bytes.
    pub fn original(&self) -> &[u8] {
        &self.original
    }

    /// The innermost payload as it was in the original file.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Envelope metadata.
    pub fn envelope(&self) -> &FormatEnvelope {
        &self.envelope
    }

    /// Structural codec id.
    pub fn codec_id(&self) -> &'static str {
        self.doc.codec_id()
    }

    /// Layer descriptors, outermost first.
    pub fn layers(&self) -> Vec<LayerDescriptor> {
        self.layers.iter().map(|l| l.descriptor()).collect()
    }

    /// Semantic tree.
    pub fn root(&self) -> Node {
        self.doc.to_ast()
    }

    /// Semantic tree + envelope.
    pub fn ast(&self) -> SaveAst {
        SaveAst::new(self.root(), self.envelope.clone())
    }

    /// Edit capabilities of the document.
    pub fn capabilities(&self) -> crate::document::Capabilities {
        self.doc.capabilities()
    }

    /// Edit mode allowed by the safe edit gate.
    pub fn edit_mode(&self) -> EditMode {
        self.envelope.allowed_edit_mode()
    }

    /// Runs `encode() == original`.
    pub fn verify_round_trip(&self) -> RoundTripStatus {
        // The structural serializer must reproduce the payload by itself.
        let structural = match self.doc.encode() {
            Ok(p) => p,
            Err(e) => return RoundTripStatus::Failed(e.to_string()),
        };
        if structural != self.payload {
            return RoundTripStatus::Lossy {
                first_diff_offset: first_difference(&structural, &self.payload),
                original_len: self.payload.len(),
                reencoded_len: structural.len(),
            };
        }
        match wrap_layers(&self.layers, structural) {
            Ok(out) if out == *self.original => RoundTripStatus::Lossless,
            Ok(out) => RoundTripStatus::Lossy {
                first_diff_offset: first_difference(&out, &self.original),
                original_len: self.original.len(),
                reencoded_len: out.len(),
            },
            Err(e) => RoundTripStatus::Failed(e.to_string()),
        }
    }

    /// Switches to raw in-place editing of the innermost payload (keeps every
    /// envelope layer, so checksums / compression / encryption are still
    /// recomputed on save).
    pub fn into_raw_mode(self) -> OpenedSave {
        let OpenedSave { original, layers, payload, mut envelope, .. } = self;
        let doc: Box<dyn Document> = Box::new(RawDocument::new(payload.clone()));
        envelope.codec = "raw".into();
        envelope.notes.push("raw in-place edit mode".into());
        let mut s = OpenedSave { original, layers, payload, doc, envelope };
        let status = s.verify_round_trip();
        s.envelope.round_trip = Some(status);
        s
    }

    fn check_gate(&self) -> CodecResult<()> {
        match self.edit_mode() {
            EditMode::Structural => Ok(()),
            EditMode::RawInPlace if self.doc.codec_id() == "raw" => Ok(()),
            EditMode::RawInPlace => Err(CodecError::unsupported(
                self.doc.codec_id(),
                "structural edits are blocked by the Safe Edit Gate (round-trip not bit-exact); switch to raw in-place mode",
            )),
            EditMode::BackupRestoreOnly => Err(CodecError::unsupported(
                self.doc.codec_id(),
                "this file can only be backed up / restored as a whole (round-trip failed)",
            )),
        }
    }

    /// Applies an edited semantic tree.
    pub fn apply_ast(&mut self, root: &Node) -> CodecResult<()> {
        self.check_gate()?;
        let mut work = self.doc.clone();
        work.apply_ast(root)?;
        self.doc = work;
        Ok(())
    }

    /// Replaces one node.
    pub fn set(&mut self, path: &NodePath, value: Node) -> CodecResult<()> {
        let mut root = self.root();
        root.set(path, value, false)?;
        self.apply_ast(&root)
    }

    /// Writes a scalar at a scanner location.
    pub fn write_location(&mut self, loc: &Location, value: ScalarValue) -> CodecResult<()> {
        let mut root = self.root();
        loc.write(&mut root, value)?;
        self.apply_ast(&root)
    }

    /// Serialises the (edited) save through every layer.
    pub fn encode(&self) -> CodecResult<Vec<u8>> {
        let payload = self.doc.encode()?;
        wrap_layers(&self.layers, payload)
    }

    /// Encodes and verifies that re-opening the output yields `expected_root`.
    /// Used before any write to disk.
    pub fn encode_verified(&self, pipeline: &EnvelopePipeline, ctx: &DetectContext) -> CodecResult<Vec<u8>> {
        let bytes = self.encode()?;
        let reopened = if self.codec_id() == "raw" {
            pipeline.open_raw(&bytes, ctx)
        } else {
            pipeline.open_with_codec(&bytes, ctx, self.codec_id())?
        };
        let expected = self.root();
        let got = reopened.root();
        if !savelens_core::bit_equal(&expected, &got) {
            let d = savelens_core::diff(&expected, &got);
            return Err(CodecError::malformed(
                self.codec_id(),
                0,
                format!(
                    "verification failed: re-reading the encoded file differs ({} change(s), first: {})",
                    d.len(),
                    d.changes.first().map(|c| c.describe()).unwrap_or_default()
                ),
            ));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::compression::StreamParams;
    use crate::layers::crypto::cipher::{es3_encrypt, xor_encrypt, RollingMode};
    use savelens_core::{ChecksumAlgorithm, ChecksumRule, CompressionAlgorithm, DigestEncoding, FieldAnchor};

    fn json() -> Vec<u8> {
        br#"{"Player":{"Gold":1200,"HP":87.5,"Name":"Hero"},"Quests":{"Main":"Active"}}"#.to_vec()
    }

    fn pipeline() -> EnvelopePipeline {
        EnvelopePipeline::builtin()
    }

    fn gold(s: &OpenedSave) -> Node {
        s.root().get(&NodePath::parse("Player.Gold").unwrap()).cloned().unwrap()
    }

    #[test]
    fn plain_json() {
        let s = pipeline().open(&json(), &DetectContext::default()).unwrap();
        assert_eq!(s.codec_id(), "json");
        assert!(s.layers().is_empty());
        assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
        assert_eq!(s.encode().unwrap(), json());
    }

    #[test]
    fn es3_gzip_json_with_crc_trailer() {
        // JSON -> gzip -> ES3 encryption -> CRC32 trailer.
        let gz = StreamParams::Gzip { header: vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff], level: 6 }
            .compress(&json())
            .unwrap();
        let enc = es3_encrypt(b"password", &[7; 16], &gz).unwrap();
        let rule = ChecksumRule {
            algorithm: ChecksumAlgorithm::Crc32,
            encoding: DigestEncoding::RawLe,
            field: FieldAnchor::FromEnd(0),
            covered_skip_start: 0,
            covered_skip_end: 0,
        };
        let file = crate::layers::checksum::sign(&rule, &enc).unwrap();
        let ctx = DetectContext::for_file("slot0.es3");
        let mut s = pipeline().open(&file, &ctx).unwrap();
        assert_eq!(s.codec_id(), "json");
        let kinds: Vec<String> = s.layers().iter().map(|l| l.to_string()).collect();
        assert!(kinds[0].starts_with("Checksum"), "{kinds:?}");
        assert!(kinds[1].starts_with("Crypto"), "{kinds:?}");
        assert!(kinds[2].starts_with("Compression: Gzip"), "{kinds:?}");
        assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
        assert_eq!(s.encode().unwrap(), file);

        // Edit gold, re-sign, re-encrypt, re-compress.
        s.set(&NodePath::parse("Player.Gold").unwrap(), Node::I64(999_999)).unwrap();
        let out = s.encode_verified(&pipeline(), &ctx).unwrap();
        assert_ne!(out, file);
        assert!(crate::layers::checksum::verify(&rule, &out));
        let re = pipeline().open(&out, &ctx).unwrap();
        assert_eq!(gold(&re), Node::I64(999_999));
        assert_eq!(re.layers().len(), 3);
    }

    #[test]
    fn base64_zlib_json() {
        use base64::Engine;
        let z = crate::layers::compression::deflate(&json(), 9, true).unwrap();
        let file = base64::engine::general_purpose::STANDARD.encode(z).into_bytes();
        let mut s = pipeline().open(&file, &DetectContext::default()).unwrap();
        assert_eq!(s.codec_id(), "json");
        assert!(matches!(s.layers()[0], LayerDescriptor::Base64 { .. }));
        assert!(matches!(
            s.layers()[1],
            LayerDescriptor::Compression { algorithm: CompressionAlgorithm::Zlib, exact: true, .. }
        ));
        s.set(&NodePath::parse("Quests.Main").unwrap(), Node::String("Completed".into())).unwrap();
        let out = s.encode_verified(&pipeline(), &DetectContext::default()).unwrap();
        let re = pipeline().open(&out, &DetectContext::default()).unwrap();
        assert_eq!(re.root().get(&NodePath::parse("Quests.Main").unwrap()), Some(&Node::String("Completed".into())));
    }

    #[test]
    fn single_byte_xor_is_brute_forced() {
        let file = xor_encrypt(&[0x3c], RollingMode::Fixed, &json()).unwrap();
        let s = pipeline().open(&file, &DetectContext::default()).unwrap();
        assert_eq!(s.codec_id(), "json");
        assert!(matches!(&s.layers()[0], LayerDescriptor::Crypto { profile, .. } if profile == "auto-xor-0x3c"));
        assert_eq!(gold(&s), Node::I64(1200));
    }

    #[test]
    fn unknown_binary_falls_back_to_raw_and_edits_in_place() {
        let mut data = vec![0u8; 256];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i * 7 % 13) as u8;
        }
        data[64..68].copy_from_slice(&1500i32.to_le_bytes());
        let ctx = DetectContext::default();
        let mut s = pipeline().open(&data, &ctx).unwrap();
        assert_eq!(s.codec_id(), "raw");
        assert_eq!(s.edit_mode(), EditMode::Structural);
        let loc = Location::Raw {
            region: NodePath::root(),
            offset: 64,
            ty: savelens_core::ScanType::I32,
            endian: savelens_core::Endianness::Little,
        };
        s.write_location(&loc, ScalarValue::Int(42)).unwrap();
        let out = s.encode_verified(&pipeline(), &ctx).unwrap();
        assert_eq!(&out[64..68], &42i32.to_le_bytes());
    }

    #[test]
    fn raw_mode_keeps_layers_and_recomputes_checksum() {
        let mut body = vec![0u8; 128];
        body[10..14].copy_from_slice(&777u32.to_le_bytes());
        for (i, b) in body.iter_mut().enumerate().skip(20) {
            *b = (i % 251) as u8;
        }
        let rule = ChecksumRule {
            algorithm: ChecksumAlgorithm::Sha1,
            encoding: DigestEncoding::RawBe,
            field: FieldAnchor::FromStart(0),
            covered_skip_start: 0,
            covered_skip_end: 0,
        };
        let file = crate::layers::checksum::sign(&rule, &body).unwrap();
        let ctx = DetectContext::default();
        let mut s = pipeline().open(&file, &ctx).unwrap().into_raw_mode();
        assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
        let loc = Location::Raw {
            region: NodePath::root(),
            offset: 10,
            ty: savelens_core::ScanType::U32,
            endian: savelens_core::Endianness::Little,
        };
        s.write_location(&loc, ScalarValue::Int(123_456)).unwrap();
        let out = s.encode().unwrap();
        assert!(crate::layers::checksum::verify(&rule, &out));
        assert_eq!(&out[20 + 10..20 + 14], &123_456u32.to_le_bytes());
    }

    #[test]
    fn forced_codec_errors_are_reported() {
        let p = pipeline();
        assert!(p.open_with_codec(b"not json", &DetectContext::default(), "json").is_err());
        assert!(p.open_with_codec(b"x", &DetectContext::default(), "nope").is_err());
    }
}
