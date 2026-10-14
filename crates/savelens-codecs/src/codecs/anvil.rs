//! Minecraft region files: Anvil (`.mca`) and McRegion (`.mcr`).
//!
//! ```text
//! 0x0000  1024 × u32 BE location  = sector offset (24 bit) << 8 | sector count
//! 0x1000  1024 × u32 BE timestamp (seconds since the epoch, last save)
//! 0x2000  4 KiB sectors; a chunk is u32 BE length + u8 compression type
//!         (1 gzip, 2 zlib, 3 none, 4 LZ4, 127 custom; bit 0x80 = stored in
//!         an external `c.X.Z.mcc` file) + payload of `length - 1` bytes
//! ```
//!
//! Chunk index `i` holds the chunk at local coordinates `x = i % 32`,
//! `z = i / 32`; the AST key is `"x,z"`. Every present chunk payload is
//! opened through the full pipeline with [`NestedEntry`] (zlib / gzip are
//! peeled by the compression layer, the NBT codec parses the tree).
//!
//! Losslessness: the header is re-serialised from the parsed 2048 integers,
//! each chunk from its length, type, payload and the exact bytes that follow
//! it inside its allocated sectors (often stale data from an older, longer
//! version of the chunk), and every other byte (unused sectors, corrupt or
//! overlapping chunks, trailing data) is kept verbatim.
//!
//! Edits: a re-encoded chunk that still fits in its allocated sectors is
//! rewritten in place (length updated, the bytes freed by a shrinking chunk
//! zeroed, the original padding beyond the old end kept). A chunk that no
//! longer fits is appended in new zero-padded sectors at the end of the file
//! and its location entry is updated; its old sectors are left untouched
//! (unreferenced), as Minecraft itself does. Timestamps are kept.

use savelens_core::{bit_equal, Endianness, Node, NodeMap};

use crate::codecs::container::NestedEntry;
use crate::document::{Capabilities, Confidence, DetectContext, Document, StructuralCodec};
use crate::error::{CodecError, CodecResult};
use crate::layers::compression::decode_stream;
use crate::pipeline::{EnvelopePipeline, PipelineOptions};
use savelens_core::CompressionAlgorithm;

const CODEC: &str = "anvil";
/// Sector size.
pub const SECTOR: usize = 4096;
/// Header size (locations + timestamps).
pub const HEADER: usize = 2 * SECTOR;
const CHUNKS: usize = 1024;
/// AST key of the read-only timestamp table.
pub const TIMESTAMPS_KEY: &str = "$timestamps";

/// Human name of a chunk compression type.
pub fn compression_name(ty: u8) -> &'static str {
    match ty & 0x7f {
        1 => "gzip",
        2 => "zlib",
        3 => "uncompressed",
        4 => "lz4",
        127 => "custom",
        _ => "unknown",
    }
}

/// AST key of a chunk index.
pub fn chunk_key(index: usize) -> String {
    format!("{},{}", index % 32, index / 32)
}

/// One editable chunk.
#[derive(Debug, Clone)]
struct Chunk {
    index: usize,
    key: String,
    /// Byte offset of the chunk (sector offset × 4096).
    start: usize,
    /// Bytes usable in place: allocated sectors, clipped to the file end.
    avail: usize,
    /// Length field as stored (type byte + payload).
    length: u32,
    ctype: u8,
    entry: NestedEntry,
    /// Bytes after the chunk data up to `avail`, verbatim.
    padding: Vec<u8>,
}

impl Chunk {
    /// Bytes occupied by length field, type and payload in the original file.
    fn data_len(&self) -> usize {
        4 + self.length as usize
    }
}

#[derive(Debug, Clone)]
enum Segment {
    Verbatim(Vec<u8>),
    Chunk(usize),
}

/// Header-only view used by detection and parsing.
struct Header {
    locations: Vec<u32>,
    timestamps: Vec<u32>,
}

fn parse_header(data: &[u8]) -> CodecResult<Header> {
    if data.len() < HEADER {
        return Err(CodecError::Truncated {
            codec: CODEC,
            context: "region header".into(),
            offset: 0,
            needed: HEADER - data.len(),
        });
    }
    let word = |i: usize| u32::from_be_bytes([data[i * 4], data[i * 4 + 1], data[i * 4 + 2], data[i * 4 + 3]]);
    Ok(Header { locations: (0..CHUNKS).map(word).collect(), timestamps: (CHUNKS..2 * CHUNKS).map(word).collect() })
}

/// Why a present location cannot be used as an editable chunk.
fn check_location(data: &[u8], loc: u32) -> Result<(usize, usize, u32, u8), String> {
    let offset = (loc >> 8) as usize;
    let count = (loc & 0xff) as usize;
    if offset < 2 {
        return Err(format!("sector offset {offset} points into the header"));
    }
    if count == 0 {
        return Err("allocated sector count is 0".into());
    }
    let start = offset * SECTOR;
    if start + 5 > data.len() {
        return Err(format!("sector {offset} is beyond the end of the file"));
    }
    let length = u32::from_be_bytes([data[start], data[start + 1], data[start + 2], data[start + 3]]);
    let ctype = data[start + 4];
    if length == 0 {
        return Err("chunk length is 0".into());
    }
    let data_len = 4 + length as usize;
    if data_len > count * SECTOR {
        return Err(format!("chunk length {length} exceeds its {count} allocated sector(s)"));
    }
    if start + data_len > data.len() {
        return Err(format!("chunk length {length} runs past the end of the file"));
    }
    if ctype & 0x80 != 0 {
        return Err(format!("chunk is stored in an external .mcc file (type {ctype:#04x})"));
    }
    Ok((start, count, length, ctype))
}

/// A parsed region file.
#[derive(Debug, Clone)]
pub struct AnvilDocument {
    locations: Vec<u32>,
    timestamps: Vec<u32>,
    chunks: Vec<Chunk>,
    segments: Vec<Segment>,
    /// Indexes of present-but-unusable chunks with the reason.
    skipped: Vec<(usize, String)>,
}

impl AnvilDocument {
    /// Parses a region file and opens every valid chunk.
    pub fn parse(data: &[u8], ctx: &DetectContext) -> CodecResult<AnvilDocument> {
        let Header { locations, timestamps } = parse_header(data)?;
        let mut skipped = Vec::new();
        let mut candidates: Vec<(usize, usize, usize, u32, u8)> = Vec::new();
        for (i, &loc) in locations.iter().enumerate() {
            if loc == 0 {
                continue;
            }
            match check_location(data, loc) {
                Ok((start, count, length, ctype)) => candidates.push((i, start, count, length, ctype)),
                Err(reason) => skipped.push((i, reason)),
            }
        }
        // Chunks sharing a sector cannot be edited independently.
        let mut owners: std::collections::HashMap<usize, Vec<usize>> = std::collections::HashMap::new();
        for &(i, start, count, _, _) in &candidates {
            for s in start / SECTOR..start / SECTOR + count {
                owners.entry(s).or_default().push(i);
            }
        }
        let shared: std::collections::BTreeSet<usize> =
            owners.values().filter(|v| v.len() > 1).flatten().copied().collect();
        candidates.retain(|&(i, ..)| {
            if shared.contains(&i) {
                skipped.push((i, "allocated sectors overlap another chunk".into()));
                false
            } else {
                true
            }
        });
        skipped.sort_by_key(|(i, _)| *i);

        // Chunk payloads are bare compressed streams (the type byte names the
        // algorithm): never search them for checksum fields, which would try
        // hundreds of digest rules on each of up to 1024 chunks.
        let base = ctx.pipeline();
        let options = PipelineOptions { hunt_checksums: false, ..PipelineOptions::default() };
        let pipeline = EnvelopePipeline::new(base.registry().clone(), base.keys().clone(), options);
        let ctx = &DetectContext { pipeline: Some(pipeline), ..ctx.clone() };
        // `candidates` is in index order, so `chunks` is too.
        let mut chunks = Vec::with_capacity(candidates.len());
        for (i, start, count, length, ctype) in candidates {
            let avail = (count * SECTOR).min(data.len() - start);
            let data_end = start + 4 + length as usize;
            let key = chunk_key(i);
            let entry = NestedEntry::open(ctx, &key, &data[start + 5..data_end]);
            chunks.push(Chunk {
                index: i,
                key,
                start,
                avail,
                length,
                ctype,
                entry,
                padding: data[data_end..start + avail].to_vec(),
            });
        }
        let mut order: Vec<usize> = (0..chunks.len()).collect();
        order.sort_by_key(|&c| chunks[c].start);
        let mut segments = Vec::with_capacity(order.len() * 2 + 1);
        let mut pos = HEADER;
        for c in order {
            let ch = &chunks[c];
            if ch.start > pos {
                segments.push(Segment::Verbatim(data[pos..ch.start].to_vec()));
            }
            segments.push(Segment::Chunk(c));
            pos = ch.start + ch.avail;
        }
        if pos < data.len() {
            segments.push(Segment::Verbatim(data[pos..].to_vec()));
        }
        Ok(AnvilDocument { locations, timestamps, chunks, segments, skipped })
    }

    /// Number of editable chunks.
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// AST keys (`"x,z"`) of the editable chunks, in index order.
    pub fn chunk_keys(&self) -> Vec<&str> {
        self.chunks.iter().map(|c| c.key.as_str()).collect()
    }

    /// Present chunks that are kept verbatim but cannot be edited, with the reason.
    pub fn skipped(&self) -> &[(usize, String)] {
        &self.skipped
    }

    /// Raw location entry of a chunk index.
    pub fn location(&self, index: usize) -> Option<u32> {
        self.locations.get(index).copied()
    }

    fn timestamps_node(&self) -> Node {
        let mut m = NodeMap::with_capacity(self.chunks.len());
        for c in &self.chunks {
            m.insert(c.key.clone(), Node::U32(self.timestamps[c.index]));
        }
        Node::Map(m)
    }
}

impl Document for AnvilDocument {
    fn codec_id(&self) -> &'static str {
        CODEC
    }

    fn to_ast(&self) -> Node {
        let mut m = NodeMap::with_capacity(self.chunks.len() + 1);
        for c in &self.chunks {
            m.insert(c.key.clone(), c.entry.to_ast());
        }
        m.insert(TIMESTAMPS_KEY.to_string(), self.timestamps_node());
        Node::Map(m)
    }

    fn apply_ast(&mut self, root: &Node) -> CodecResult<()> {
        let Node::Map(m) = root else {
            return Err(CodecError::type_mismatch(CODEC, "$", "map (\"x,z\" → chunk)", root.kind().name()));
        };
        if m.len() != self.chunks.len() + 1
            || m.keys().any(|k| k != TIMESTAMPS_KEY && !self.chunks.iter().any(|c| &c.key == k))
        {
            return Err(CodecError::structure(
                CODEC,
                "$",
                "chunks cannot be added, removed or renamed; edit the contents of an existing chunk",
            ));
        }
        match m.get(TIMESTAMPS_KEY) {
            Some(ts) if bit_equal(ts, &self.timestamps_node()) => {}
            _ => {
                return Err(CodecError::structure(CODEC, TIMESTAMPS_KEY, "chunk timestamps are read-only"));
            }
        }
        let mut work = self.chunks.clone();
        for c in &mut work {
            let node = m.get(&c.key).ok_or_else(|| CodecError::path_not_found(CODEC, &c.key))?;
            c.entry.apply_ast(node)?;
        }
        self.chunks = work;
        // Fail early on edits that cannot be stored (e.g. > 255 sectors).
        self.encode().map(|_| ())
    }

    fn encode(&self) -> CodecResult<Vec<u8>> {
        let mut locations = self.locations.clone();
        let mut out = vec![0u8; HEADER];
        let mut relocated: Vec<(usize, Vec<u8>)> = Vec::new();
        for s in &self.segments {
            match s {
                Segment::Verbatim(b) => out.extend_from_slice(b),
                Segment::Chunk(n) => {
                    let c = &self.chunks[*n];
                    let payload = c.entry.encode()?;
                    let need = 5 + payload.len();
                    if !c.entry.is_dirty() || need <= c.avail {
                        let length = if c.entry.is_dirty() {
                            u32::try_from(payload.len() + 1)
                                .map_err(|_| CodecError::unencodable(CODEC, &c.key, "chunk larger than 4 GiB"))?
                        } else {
                            c.length
                        };
                        let begin = out.len();
                        out.extend_from_slice(&length.to_be_bytes());
                        out.push(c.ctype);
                        out.extend_from_slice(&payload);
                        // Bytes the old chunk data occupied become zero; the
                        // original padding beyond the old end is kept.
                        let old_end = c.data_len();
                        for p in need..c.avail {
                            out.push(if p < old_end { 0 } else { c.padding[p - old_end] });
                        }
                        debug_assert!(!c.entry.is_dirty() || out.len() - begin == c.avail);
                    } else {
                        // Keep the stale bytes in place, write the chunk at the end.
                        out.extend_from_slice(&c.length.to_be_bytes());
                        out.push(c.ctype);
                        out.extend_from_slice(c.entry.original());
                        out.extend_from_slice(&c.padding);
                        relocated.push((*n, payload));
                    }
                }
            }
        }
        for (n, payload) in relocated {
            let c = &self.chunks[n];
            let aligned = out.len().div_ceil(SECTOR) * SECTOR;
            out.resize(aligned, 0);
            let sector = aligned / SECTOR;
            let need = 5 + payload.len();
            let count = need.div_ceil(SECTOR);
            if count > 255 {
                return Err(CodecError::unencodable(
                    CODEC,
                    &c.key,
                    format!("chunk needs {count} sectors; region files allow at most 255 (external .mcc chunks are not written)"),
                ));
            }
            if sector > 0x00ff_ffff {
                return Err(CodecError::unencodable(CODEC, &c.key, "region file exceeds the 24-bit sector offset"));
            }
            let length = u32::try_from(payload.len() + 1)
                .map_err(|_| CodecError::unencodable(CODEC, &c.key, "chunk larger than 4 GiB"))?;
            out.extend_from_slice(&length.to_be_bytes());
            out.push(c.ctype);
            out.extend_from_slice(&payload);
            out.resize(aligned + count * SECTOR, 0);
            locations[c.index] = ((sector as u32) << 8) | count as u32;
        }
        for (i, v) in locations.iter().chain(&self.timestamps).enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
        }
        Ok(out)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::VALUES_ONLY
    }

    fn endianness(&self) -> Endianness {
        Endianness::Big
    }

    fn notes(&self) -> Vec<String> {
        let mut types: std::collections::BTreeMap<&'static str, usize> = std::collections::BTreeMap::new();
        for c in &self.chunks {
            *types.entry(compression_name(c.ctype)).or_default() += 1;
        }
        let mut n = vec![format!(
            "region file: {} editable chunk(s) ({}), timestamps exposed read-only under `{TIMESTAMPS_KEY}`",
            self.chunks.len(),
            types.iter().map(|(k, v)| format!("{v} {k}")).collect::<Vec<_>>().join(", ")
        )];
        for (i, reason) in &self.skipped {
            n.push(format!("chunk {} (index {i}) kept verbatim, not editable: {reason}", chunk_key(*i)));
        }
        let opaque = self.chunks.iter().filter(|c| c.entry.codec_id() != "nbt").count();
        if opaque > 0 {
            n.push(format!("{opaque} chunk(s) are not NBT after decompression and are edited as raw bytes"));
        }
        n
    }

    fn clone_box(&self) -> Box<dyn Document> {
        Box::new(self.clone())
    }
}

/// True when the chunk at `loc` can be decompressed (strict detection).
fn first_chunk_decodes(data: &[u8], loc: u32) -> bool {
    let Ok((start, _, length, ctype)) = check_location(data, loc) else { return false };
    let payload = &data[start + 5..start + 4 + length as usize];
    let inner = match ctype {
        1 => decode_stream(CompressionAlgorithm::Gzip, payload, None).map(|s| s.output),
        2 => decode_stream(CompressionAlgorithm::Zlib, payload, None).map(|s| s.output),
        3 => Ok(payload.to_vec()),
        _ => return false,
    };
    matches!(inner, Ok(v) if v.first() == Some(&crate::codecs::nbt::tag::COMPOUND))
}

/// The region codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct AnvilCodec;

impl StructuralCodec for AnvilCodec {
    fn id(&self) -> &'static str {
        CODEC
    }

    fn name(&self) -> &'static str {
        "Minecraft region (Anvil)"
    }

    fn detect(&self, bytes: &[u8], ctx: &DetectContext) -> Confidence {
        let Ok(h) = parse_header(bytes) else { return 0 };
        if matches!(ctx.extension().as_deref(), Some("mca") | Some("mcr")) {
            return 85;
        }
        let sectors = bytes.len().div_ceil(SECTOR);
        let mut first = None;
        for (i, &loc) in h.locations.iter().enumerate() {
            if loc == 0 {
                continue;
            }
            let offset = (loc >> 8) as usize;
            let count = (loc & 0xff) as usize;
            if offset < 2 || count == 0 || offset + count > sectors {
                return 0;
            }
            first.get_or_insert(i);
        }
        match first {
            Some(i) if first_chunk_decodes(bytes, h.locations[i]) => 85,
            _ => 0,
        }
    }

    fn decode(&self, bytes: &[u8], ctx: &DetectContext) -> CodecResult<Box<dyn Document>> {
        Ok(Box::new(AnvilDocument::parse(bytes, ctx)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::compression::deflate;
    use crate::pipeline::EnvelopePipeline;
    use savelens_core::{NodePath, RoundTripStatus};

    /// Test-side NBT writer: `{"": {DataVersion: int, xPos: int, Name: string, Blob: byte[]}}`.
    fn chunk_nbt(x: i32, name: &str, blob: usize) -> Vec<u8> {
        let mut v = vec![10, 0, 0];
        v.push(3);
        v.extend_from_slice(&11u16.to_be_bytes());
        v.extend_from_slice(b"DataVersion");
        v.extend_from_slice(&3337i32.to_be_bytes());
        v.push(3);
        v.extend_from_slice(&4u16.to_be_bytes());
        v.extend_from_slice(b"xPos");
        v.extend_from_slice(&x.to_be_bytes());
        v.push(8);
        v.extend_from_slice(&4u16.to_be_bytes());
        v.extend_from_slice(b"Name");
        v.extend_from_slice(&(name.len() as u16).to_be_bytes());
        v.extend_from_slice(name.as_bytes());
        v.push(7);
        v.extend_from_slice(&4u16.to_be_bytes());
        v.extend_from_slice(b"Blob");
        v.extend_from_slice(&(blob as i32).to_be_bytes());
        // Pseudo-random bytes so that compression cannot shrink them much.
        let mut s = 0x1234_5678u32 ^ x as u32;
        for _ in 0..blob {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            v.push(s as u8);
        }
        v.push(0);
        v
    }

    /// Test-side region writer: chunks `(index, compressed payload, type, sectors)`
    /// with stale `0xAB` padding after each chunk.
    fn write_region(chunks: &[(usize, Vec<u8>, u8, usize)]) -> Vec<u8> {
        let mut out = vec![0u8; HEADER];
        for (index, payload, ty, sectors) in chunks {
            let sector = out.len() / SECTOR;
            let loc = ((sector as u32) << 8) | *sectors as u32;
            out[index * 4..index * 4 + 4].copy_from_slice(&loc.to_be_bytes());
            out[SECTOR + index * 4..SECTOR + index * 4 + 4]
                .copy_from_slice(&(1_700_000_000u32 + *index as u32).to_be_bytes());
            let begin = out.len();
            out.extend_from_slice(&(payload.len() as u32 + 1).to_be_bytes());
            out.push(*ty);
            out.extend_from_slice(payload);
            out.resize(begin + sectors * SECTOR, 0xab);
        }
        out
    }

    fn sample() -> Vec<u8> {
        let a = deflate(&chunk_nbt(0, "alpha", 100), 6, true).unwrap();
        let b = deflate(&chunk_nbt(1, "beta", 200), 6, true).unwrap();
        let c = chunk_nbt(2, "gamma", 50);
        write_region(&[(0, a, 2, 1), (33, b, 2, 2), (1023, c, 3, 1)])
    }

    fn open(bytes: &[u8]) -> crate::pipeline::OpenedSave {
        EnvelopePipeline::builtin().open(bytes, &DetectContext::for_file("r.0.0.mca")).unwrap()
    }

    fn slice_of(file: &[u8], doc_loc: u32) -> &[u8] {
        let start = (doc_loc >> 8) as usize * SECTOR;
        &file[start..start + (doc_loc & 0xff) as usize * SECTOR]
    }

    fn loc(file: &[u8], i: usize) -> u32 {
        u32::from_be_bytes(file[i * 4..i * 4 + 4].try_into().unwrap())
    }

    #[test]
    fn detects_and_round_trips() {
        let file = sample();
        assert_eq!(AnvilCodec.detect(&file, &DetectContext::default()), 85, "strict header detection");
        assert_eq!(AnvilCodec.detect(&file, &DetectContext::for_file("r.0.0.mcr")), 85);
        assert_eq!(AnvilCodec.detect(&[0u8; 100], &DetectContext::for_file("r.0.0.mca")), 0);
        assert_eq!(AnvilCodec.detect(&vec![0x55; HEADER * 2], &DetectContext::default()), 0);
        let s = open(&file);
        assert_eq!(s.codec_id(), CODEC);
        assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
        assert_eq!(s.encode().unwrap(), file);
        let root = s.root();
        let Node::Map(m) = &root else { panic!() };
        assert_eq!(m.keys().collect::<Vec<_>>(), vec!["0,0", "1,1", "31,31", TIMESTAMPS_KEY]);
        assert_eq!(root.get(&NodePath::root().key("1,1").key("Name")), Some(&Node::String("beta".into())));
        assert_eq!(
            root.get(&NodePath::root().key(TIMESTAMPS_KEY).key("31,31")),
            Some(&Node::U32(1_700_000_000 + 1023))
        );
    }

    #[test]
    fn in_place_edit_keeps_other_chunks() {
        let file = sample();
        let mut s = open(&file);
        s.set(&NodePath::root().key("1,1").key("xPos"), Node::I32(-77)).unwrap();
        let out = s.encode_verified(&EnvelopePipeline::builtin(), &DetectContext::for_file("r.0.0.mca")).unwrap();
        assert_eq!(out.len(), file.len(), "fits in place");
        assert_eq!(&out[..HEADER], &file[..HEADER], "header unchanged");
        assert_eq!(slice_of(&out, loc(&file, 0)), slice_of(&file, loc(&file, 0)));
        assert_eq!(slice_of(&out, loc(&file, 1023)), slice_of(&file, loc(&file, 1023)));
        let re = open(&out);
        assert_eq!(re.root().get(&NodePath::root().key("1,1").key("xPos")), Some(&Node::I32(-77)));
        // Uncompressed chunk edit: the string shrinks, freed bytes are zeroed.
        let mut s = open(&file);
        s.set(&NodePath::root().key("31,31").key("Name"), Node::String("g".into())).unwrap();
        let out = s.encode().unwrap();
        let area = slice_of(&out, loc(&file, 1023));
        let len = u32::from_be_bytes(area[..4].try_into().unwrap()) as usize;
        let old_len = u32::from_be_bytes(slice_of(&file, loc(&file, 1023))[..4].try_into().unwrap()) as usize;
        assert_eq!(len + 4, old_len);
        assert!(area[4 + len..4 + old_len].iter().all(|&b| b == 0));
        assert!(area[4 + old_len..].iter().all(|&b| b == 0xab), "original padding kept");
    }

    #[test]
    fn growing_chunk_is_relocated() {
        let file = sample();
        let mut s = open(&file);
        let path = NodePath::root().key("0,0").key("Blob");
        let Some(Node::Bytes(blob)) = s.root().get(&path).cloned() else { panic!() };
        let mut big = blob.clone();
        let mut x = 7u32;
        for _ in 0..6000 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            big.push((x >> 16) as u8);
        }
        s.set(&path, Node::Bytes(big.clone())).unwrap();
        let out = s.encode_verified(&EnvelopePipeline::builtin(), &DetectContext::for_file("r.0.0.mca")).unwrap();
        assert_eq!(out.len() % SECTOR, 0);
        let new_loc = loc(&out, 0);
        assert_eq!((new_loc >> 8) as usize, file.len() / SECTOR, "appended at the end");
        assert!(new_loc & 0xff >= 2);
        // The old area and the other chunks are byte-identical.
        assert_eq!(&out[HEADER..file.len()], &file[HEADER..]);
        assert_eq!(loc(&out, 33), loc(&file, 33));
        assert_eq!(&out[SECTOR..HEADER], &file[SECTOR..HEADER], "timestamps kept");
        let re = open(&out);
        assert_eq!(re.root().get(&path), Some(&Node::Bytes(big)));
    }

    #[test]
    fn rejects_structure_and_timestamp_edits() {
        let file = sample();
        let mut s = open(&file);
        let mut root = s.root();
        root.set(&NodePath::root().key(TIMESTAMPS_KEY).key("0,0"), Node::U32(5), false).unwrap();
        assert!(matches!(s.apply_ast(&root), Err(CodecError::StructureUnsupported { .. })));
        let mut root = s.root();
        root.remove(&NodePath::root().key("0,0")).unwrap();
        assert!(matches!(s.apply_ast(&root), Err(CodecError::StructureUnsupported { .. })));
        assert_eq!(s.encode().unwrap(), file);
    }

    #[test]
    fn corrupt_locations_are_kept_verbatim() {
        let mut file = sample();
        // Index 5: points beyond EOF; index 6 and 7: share a sector.
        file[5 * 4..5 * 4 + 4].copy_from_slice(&((500u32 << 8) | 1).to_be_bytes());
        let l33 = loc(&file, 33);
        file[6 * 4..6 * 4 + 4].copy_from_slice(&l33.to_be_bytes());
        let doc = AnvilDocument::parse(&file, &DetectContext::for_file("r.0.0.mca")).unwrap();
        assert_eq!(doc.chunk_keys(), vec!["0,0", "31,31"]);
        assert_eq!(doc.skipped().len(), 3);
        assert_eq!(doc.encode().unwrap(), file);
        assert_eq!(AnvilCodec.detect(&file, &DetectContext::default()), 0, "inconsistent header without extension");
    }

    #[test]
    fn empty_region() {
        let file = vec![0u8; HEADER];
        let s = open(&file);
        assert_eq!(s.codec_id(), CODEC);
        assert_eq!(s.encode().unwrap(), file);
    }
}
