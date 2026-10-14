//! FromSoftware BND4 binder (`.sl2` save containers of Dark Souls III,
//! Sekiro, Elden Ring, Armored Core VI, …).
//!
//! Layout (all integers little-endian unless the big-endian flag is set):
//!
//! ```text
//! 0x00  "BND4"
//! 0x04  u8 unk04, u8 unk05, u8[2] reserved
//! 0x08  u8 reserved, u8 big_endian, u8 bit_little_endian, u8 reserved
//! 0x0C  u32 file count
//! 0x10  u64 header size (0x40)
//! 0x18  char[8] version ("00000001" for saves)
//! 0x20  u64 file header size
//! 0x28  u64 end of the file headers / name table
//! 0x30  u8 unicode, u8 raw format, u8 extended, u8 reserved
//! 0x34  u32 reserved
//! 0x38  u64 hash buckets offset (only meaningful when extended == 4)
//! 0x40  file headers, then the name table, then the entry payloads
//! ```
//!
//! The raw format byte is stored bit-reversed unless the binder is
//! "bit big-endian" (the SoulsFormats rule is reproduced exactly). Each file
//! header contains: flags, `-1`, the stored size, optionally the
//! uncompressed size, the data offset (32 or 64 bit), optionally an id and a
//! name offset (UTF-16 or single-byte names).
//!
//! Every byte that does not belong to an entry payload — the header, file
//! headers, name table, hash buckets, alignment padding and gaps — is kept
//! verbatim in an ordered segment list, so the writer reproduces the file bit
//! for bit. Entry payloads are opened through the pipeline with
//! [`NestedEntry`]. An Elden Ring slot (`MD5(16) ‖ data`) gets an MD5 header
//! [`ChecksumLayer`] (re-signed after an edit); the codec verifies that rule
//! itself instead of letting the checksum hunter brute-force it, see
//! [`Bnd4Entry::open`]. A Dark Souls III slot
//! (`MD5(16) ‖ IV(16) ‖ AES-128-CBC data`) is additionally decrypted, exactly
//! once, with the `dark-souls-3` profile of `game_keys.toml`, then
//! re-encrypted with the original IV and re-signed after an edit.
//! Save slots are fixed-size: an edit that changes the size of an entry is
//! rejected.

use savelens_core::{ChecksumAlgorithm, ChecksumRule, DigestEncoding, Endianness, FieldAnchor, Node, NodeMap};

use crate::codecs::container::NestedEntry;
use crate::document::{Capabilities, Confidence, DetectContext, Document, StructuralCodec};
use crate::entropy;
use crate::error::{CodecError, CodecResult};
use crate::io::ByteReader;
use crate::layers::checksum::ChecksumLayer;
use crate::layers::crypto::keys::{IvSource, KeyDatabase, KeyProfile};
use crate::layers::crypto::CryptoLayer;
use crate::layers::Layer;
use crate::pipeline::{EnvelopePipeline, PipelineOptions};

const CODEC: &str = "bnd4";
const HEADER_LEN: usize = 0x40;
/// Upper bound for the declared file count (real saves have a dozen).
const MAX_FILES: u32 = 1 << 20;
/// Longest entry name accepted (in characters).
const MAX_NAME_CHARS: usize = 1024;

/// Binder format bits (after bit-order normalisation, SoulsFormats naming).
pub mod format {
    pub const BIG_ENDIAN: u8 = 0b0000_0001;
    pub const IDS: u8 = 0b0000_0010;
    pub const NAMES1: u8 = 0b0000_0100;
    pub const NAMES2: u8 = 0b0000_1000;
    pub const LONG_OFFSETS: u8 = 0b0001_0000;
    pub const COMPRESSION: u8 = 0b0010_0000;
}

/// Parsed BND4 header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bnd4Header {
    pub unk04: u8,
    pub unk05: u8,
    /// Byte 0x09: integers are big-endian.
    pub big_endian: bool,
    /// Byte 0x0A as stored (0 means "bit big-endian").
    pub bit_little_endian: u8,
    pub file_count: u32,
    pub header_size: u64,
    /// Version string, e.g. `00000001`.
    pub version: [u8; 8],
    pub file_header_size: u64,
    pub headers_end: u64,
    pub unicode: bool,
    /// Format byte as stored in the file.
    pub raw_format: u8,
    /// Normalised format bits (see [`format`]).
    pub format: u8,
    pub extended: u8,
    pub buckets_offset: u64,
}

impl Bnd4Header {
    fn has(&self, bit: u8) -> bool {
        self.format & bit != 0
    }

    fn has_names(&self) -> bool {
        self.has(format::NAMES1) || self.has(format::NAMES2)
    }

    /// File header size implied by the format bits.
    pub fn expected_file_header_size(&self) -> u64 {
        let mut n = 0x10;
        if self.has(format::COMPRESSION) {
            n += 8;
        }
        n += if self.has(format::LONG_OFFSETS) { 8 } else { 4 };
        if self.has(format::IDS) {
            n += 4;
        }
        if self.has_names() {
            n += 4;
        }
        if self.format == format::NAMES1 {
            n += 8;
        }
        n
    }
}

/// One file header of the binder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bnd4FileHeader {
    /// Flags byte as stored.
    pub raw_flags: u8,
    /// Stored (possibly compressed) size of the payload.
    pub size: u64,
    /// Uncompressed size (only with the compression format bit).
    pub uncompressed_size: Option<i64>,
    pub data_offset: u64,
    pub id: Option<i32>,
    pub name_offset: Option<u32>,
    /// Decoded name (`None` when the binder has no names).
    pub name: Option<String>,
}

/// Parsed layout without opened entries (used by detection too).
#[derive(Debug, Clone)]
struct Layout {
    header: Bnd4Header,
    files: Vec<Bnd4FileHeader>,
}

fn reader_u32(r: &mut ByteReader<'_>, be: bool, ctx: &str) -> CodecResult<u32> {
    if be {
        r.u32_be(ctx)
    } else {
        r.u32_le(ctx)
    }
}

fn reader_u64(r: &mut ByteReader<'_>, be: bool, ctx: &str) -> CodecResult<u64> {
    if be {
        r.u64_be(ctx)
    } else {
        r.u64_le(ctx)
    }
}

fn reader_i64(r: &mut ByteReader<'_>, be: bool, ctx: &str) -> CodecResult<i64> {
    if be {
        r.i64_be(ctx)
    } else {
        r.i64_le(ctx)
    }
}

fn reader_i32(r: &mut ByteReader<'_>, be: bool, ctx: &str) -> CodecResult<i32> {
    if be {
        r.i32_be(ctx)
    } else {
        r.i32_le(ctx)
    }
}

fn to_usize(v: u64, what: &str, offset: usize) -> CodecResult<usize> {
    usize::try_from(v).map_err(|_| CodecError::malformed(CODEC, offset, format!("{what} {v} does not fit in memory")))
}

/// Reads a NUL-terminated name at `offset`.
fn read_name(data: &[u8], offset: usize, unicode: bool, big_endian: bool) -> CodecResult<String> {
    let tail = data
        .get(offset..)
        .ok_or_else(|| CodecError::malformed(CODEC, offset, "name offset beyond the end of the file"))?;
    if unicode {
        let mut units = Vec::new();
        for pair in tail.chunks_exact(2) {
            let u = if big_endian {
                u16::from_be_bytes([pair[0], pair[1]])
            } else {
                u16::from_le_bytes([pair[0], pair[1]])
            };
            if u == 0 {
                return Ok(String::from_utf16_lossy(&units));
            }
            units.push(u);
            if units.len() > MAX_NAME_CHARS {
                break;
            }
        }
        Err(CodecError::malformed(CODEC, offset, "unterminated UTF-16 entry name"))
    } else {
        let end = tail
            .iter()
            .take(MAX_NAME_CHARS + 1)
            .position(|&b| b == 0)
            .ok_or_else(|| CodecError::malformed(CODEC, offset, "unterminated entry name"))?;
        // Shift-JIS names are shown with their ASCII part; non-ASCII bytes
        // are escaped (the raw bytes stay untouched in the name table).
        let mut s = String::with_capacity(end);
        for &b in &tail[..end] {
            if b.is_ascii() && !b.is_ascii_control() {
                s.push(b as char);
            } else {
                s.push_str(&format!("\\x{b:02x}"));
            }
        }
        Ok(s)
    }
}

fn parse_layout(data: &[u8]) -> CodecResult<Layout> {
    let mut r = ByteReader::new(data, CODEC);
    if r.take(4, "magic")? != b"BND4" {
        return Err(CodecError::malformed(CODEC, 0, "missing BND4 magic"));
    }
    let unk04 = r.u8("unk04")?;
    let unk05 = r.u8("unk05")?;
    r.take(3, "reserved")?;
    let big_endian = r.u8("big-endian flag")? != 0;
    let bit_little_endian = r.u8("bit endianness flag")?;
    r.take(1, "reserved")?;
    let be = big_endian;
    let file_count = reader_u32(&mut r, be, "file count")?;
    let header_size = reader_u64(&mut r, be, "header size")?;
    let version = r.array::<8>("version")?;
    let file_header_size = reader_u64(&mut r, be, "file header size")?;
    let headers_end = reader_u64(&mut r, be, "headers end")?;
    let unicode = r.u8("unicode flag")? != 0;
    let raw_format = r.u8("format")?;
    let extended = r.u8("extended flag")?;
    r.take(1, "reserved")?;
    r.take(4, "reserved")?;
    let buckets_offset = reader_u64(&mut r, be, "buckets offset")?;

    if header_size != HEADER_LEN as u64 {
        return Err(CodecError::malformed(CODEC, 0x10, format!("unexpected header size {header_size:#x}")));
    }
    if file_count > MAX_FILES {
        return Err(CodecError::malformed(CODEC, 0x0c, format!("implausible file count {file_count}")));
    }
    let bit_big_endian = bit_little_endian == 0;
    let reverse = bit_big_endian || (raw_format & 1 != 0 && raw_format & 0x80 == 0);
    let fmt = if reverse { raw_format } else { raw_format.reverse_bits() };
    let header = Bnd4Header {
        unk04,
        unk05,
        big_endian,
        bit_little_endian,
        file_count,
        header_size,
        version,
        file_header_size,
        headers_end,
        unicode,
        raw_format,
        format: fmt,
        extended,
        buckets_offset,
    };
    let expected = header.expected_file_header_size();
    if file_header_size < expected || file_header_size > 0x100 {
        return Err(CodecError::malformed(
            CODEC,
            0x20,
            format!("file header size {file_header_size:#x} does not match format {fmt:#04x} (needs {expected:#x})"),
        ));
    }
    let fhs = file_header_size as usize;
    r.check_count(file_count as usize, fhs, "file headers")?;

    let mut files = Vec::with_capacity(file_count as usize);
    for i in 0..file_count as usize {
        let start = HEADER_LEN + i * fhs;
        r.seek(start)?;
        let raw_flags = r.u8("file flags")?;
        r.take(3, "file header reserved")?;
        r.take(4, "file header marker")?;
        let size_raw = reader_i64(&mut r, be, "file size")?;
        let size = u64::try_from(size_raw)
            .map_err(|_| CodecError::malformed(CODEC, start + 8, format!("negative entry size {size_raw}")))?;
        let uncompressed_size =
            if header.has(format::COMPRESSION) { Some(reader_i64(&mut r, be, "uncompressed size")?) } else { None };
        let data_offset = if header.has(format::LONG_OFFSETS) {
            reader_u64(&mut r, be, "data offset")?
        } else {
            u64::from(reader_u32(&mut r, be, "data offset")?)
        };
        let mut id = if header.has(format::IDS) { Some(reader_i32(&mut r, be, "file id")?) } else { None };
        let name_offset = if header.has_names() { Some(reader_u32(&mut r, be, "name offset")?) } else { None };
        if fmt == format::NAMES1 {
            id = Some(reader_i32(&mut r, be, "file id")?);
        }
        let off = to_usize(data_offset, "data offset", start)?;
        let len = to_usize(size, "entry size", start)?;
        if off.checked_add(len).is_none_or(|end| end > data.len()) {
            return Err(CodecError::malformed(
                CODEC,
                start,
                format!("entry {i} data [{off:#x}, +{len:#x}) exceeds the file ({:#x} bytes)", data.len()),
            ));
        }
        if len > 0 && off < HEADER_LEN + file_count as usize * fhs {
            return Err(CodecError::malformed(CODEC, start, format!("entry {i} data overlaps the file headers")));
        }
        let name = match name_offset {
            Some(no) => Some(read_name(data, no as usize, unicode, big_endian)?),
            None => None,
        };
        files.push(Bnd4FileHeader { raw_flags, size, uncompressed_size, data_offset, id, name_offset, name });
    }
    // Entry payloads must not overlap: the writer splices them independently.
    let mut ranges: Vec<(usize, usize, usize)> = files
        .iter()
        .enumerate()
        .filter(|(_, f)| f.size > 0)
        .map(|(i, f)| (f.data_offset as usize, f.data_offset as usize + f.size as usize, i))
        .collect();
    ranges.sort_unstable();
    for w in ranges.windows(2) {
        if w[1].0 < w[0].1 {
            return Err(CodecError::malformed(
                CODEC,
                w[1].0,
                format!(
                    "entries {} and {} overlap; overlapping payloads cannot be edited independently",
                    w[0].2, w[1].2
                ),
            ));
        }
    }
    Ok(Layout { header, files })
}

/// Piece of the output file.
#[derive(Debug, Clone)]
enum Segment {
    /// Bytes outside every entry (header, name table, padding, gaps).
    Verbatim(Vec<u8>),
    /// Payload of entry `n`.
    Entry(usize),
}

/// A parsed BND4 binder.
#[derive(Debug, Clone)]
pub struct Bnd4Document {
    header: Bnd4Header,
    files: Vec<Bnd4FileHeader>,
    keys: Vec<String>,
    entries: Vec<Bnd4Entry>,
    segments: Vec<Segment>,
}

fn unique_keys(files: &[Bnd4FileHeader]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let base = match (&f.name, f.id) {
                (Some(n), _) if !n.is_empty() => n.clone(),
                (_, Some(id)) => format!("id:{id}"),
                _ => format!("entry{i}"),
            };
            let mut k = base.clone();
            let mut n = 2;
            while !seen.insert(k.clone()) {
                k = format!("{base}#{n}");
                n += 1;
            }
            k
        })
        .collect()
}

/// Key profiles that describe a whole-slot cipher (AES with the IV stored in
/// front of the data, selected by AppID or entry name, e.g. `dark-souls-3`).
fn slot_cipher_profiles<'a>(keys: &'a KeyDatabase, entry_ctx: &DetectContext) -> Vec<&'a KeyProfile> {
    keys.candidates(entry_ctx)
        .into_iter()
        .filter(|p| !p.generic && p.scheme.aes().is_some() && p.iv == IvSource::Prefix)
        .collect()
}

const MD5_HEADER: ChecksumRule = ChecksumRule {
    algorithm: ChecksumAlgorithm::Md5,
    encoding: DigestEncoding::RawBe,
    field: FieldAnchor::FromStart(0),
    covered_skip_start: 0,
    covered_skip_end: 0,
};

/// One entry of the binder.
#[derive(Debug, Clone)]
pub enum Bnd4Entry {
    /// Payload without a valid MD5 header, opened directly through the
    /// pipeline (checksum hunter included).
    Plain(NestedEntry),
    /// `MD5(16) ‖ body` slot. The body is optionally decrypted exactly once
    /// with a slot-cipher profile (`dark-souls-3`: `IV(16) ‖ AES-128-CBC`);
    /// the (decrypted) body is a nested entry.
    Signed { md5: Box<ChecksumLayer>, crypto: Option<Box<CryptoLayer>>, inner: NestedEntry, original: Vec<u8> },
}

impl Bnd4Entry {
    /// Opens one entry payload.
    ///
    /// * **MD5 header.** The pipeline's checksum hunter does find the MD5 of
    ///   an Elden Ring slot, but it gets there by trying every candidate rule
    ///   over the 2.6 MB slot and then searching the stripped body again for a
    ///   second checksum (≈ 6 s per slot in a debug build, ≈ 2.5 min per
    ///   `.sl2` open). The codec therefore checks the one rule the format
    ///   defines (MD5 of everything after the first 16 bytes) and builds the
    ///   same [`ChecksumLayer`] itself; the body is opened with checksum
    ///   hunting disabled. Entries without a valid MD5 header still go
    ///   through the full pipeline.
    /// * **Slot ciphers.** Profiles that describe a whole-slot cipher are
    ///   applied here, once, and removed from the pipeline used for the
    ///   payload: the generic crypto loop accepts any low-entropy output of a
    ///   profile that matches by name/AppID, and AES-CBC "decrypts" runs of
    ///   zero blocks (plentiful in plaintext slots) to a repeating block, so it
    ///   would re-apply the profile to the plaintext again and again, or
    ///   "decrypt" an unencrypted Elden Ring slot that is also named
    ///   `USER_DATA*`.
    fn open(ctx: &DetectContext, name: &str, bytes: &[u8]) -> Bnd4Entry {
        let base = ctx.pipeline();
        let slot_profiles = slot_cipher_profiles(base.keys(), &ctx.nested(name));
        let mut keys = KeyDatabase::default();
        for p in base.keys().profiles().iter().rev() {
            if slot_profiles.iter().any(|s| s.name == p.name) {
                continue;
            }
            // Profiles of a loaded database are already validated.
            if let Err(e) = keys.add_front(p.clone()) {
                tracing::warn!("bnd4: key profile `{}` skipped for entry {name}: {e}", p.name);
            }
        }
        let Ok((md5, body)) = ChecksumLayer::with_rule(MD5_HEADER, bytes) else {
            let pipeline = EnvelopePipeline::new(base.registry().clone(), keys, PipelineOptions::default());
            let plain_ctx = DetectContext { pipeline: Some(pipeline), ..ctx.clone() };
            return Bnd4Entry::Plain(NestedEntry::open(&plain_ctx, name, bytes));
        };
        let mut crypto = None;
        let mut payload = body;
        // Real ciphertext looks random; plaintext slots never do.
        if payload.len() >= 32 && payload.len() % 16 == 0 && entropy::looks_random(&payload) {
            for p in &slot_profiles {
                let Ok((layer, plain)) = CryptoLayer::open(p, &payload) else { continue };
                if !entropy::looks_random(&plain) {
                    crypto = Some(Box::new(layer));
                    payload = plain;
                    break;
                }
            }
        }
        let options = PipelineOptions { hunt_checksums: false, ..PipelineOptions::default() };
        let pipeline = EnvelopePipeline::new(base.registry().clone(), keys, options);
        let inner_ctx = DetectContext { pipeline: Some(pipeline), ..ctx.clone() };
        let inner = NestedEntry::open(&inner_ctx, name, &payload);
        Bnd4Entry::Signed { md5: Box::new(md5), crypto, inner, original: bytes.to_vec() }
    }

    fn inner(&self) -> &NestedEntry {
        match self {
            Bnd4Entry::Plain(e) | Bnd4Entry::Signed { inner: e, .. } => e,
        }
    }

    /// Original bytes of the entry as stored in the binder.
    pub fn original(&self) -> &[u8] {
        match self {
            Bnd4Entry::Plain(e) => e.original(),
            Bnd4Entry::Signed { original, .. } => original,
        }
    }

    /// Codec id of the (innermost) payload.
    pub fn codec_id(&self) -> &'static str {
        self.inner().codec_id()
    }

    /// The nested entry (for signed slots: the body after the MD5 header,
    /// decrypted when a slot cipher applies).
    pub fn nested(&self) -> &NestedEntry {
        self.inner()
    }

    /// True when the entry has a (verified) MD5 header.
    pub fn is_md5_signed(&self) -> bool {
        matches!(self, Bnd4Entry::Signed { .. })
    }

    /// Name of the slot-cipher profile used, if any.
    pub fn cipher_profile(&self) -> Option<&str> {
        match self {
            Bnd4Entry::Signed { crypto: Some(c), .. } => Some(c.profile_name()),
            _ => None,
        }
    }

    /// True once an edit changed the entry.
    pub fn is_dirty(&self) -> bool {
        self.inner().is_dirty()
    }

    fn to_ast(&self) -> Node {
        self.inner().to_ast()
    }

    fn apply_ast(&mut self, node: &Node) -> CodecResult<()> {
        match self {
            Bnd4Entry::Plain(e) | Bnd4Entry::Signed { inner: e, .. } => e.apply_ast(node),
        }
    }

    /// Bytes to store in the binder (re-encrypted and re-signed when edited).
    pub fn encode(&self) -> CodecResult<Vec<u8>> {
        match self {
            Bnd4Entry::Plain(e) => e.encode(),
            Bnd4Entry::Signed { original, inner, .. } if !inner.is_dirty() => Ok(original.clone()),
            Bnd4Entry::Signed { md5, crypto, inner, .. } => {
                let mut body = inner.encode()?;
                if let Some(c) = crypto {
                    body = c.wrap(&body)?;
                }
                md5.wrap(&body)
            }
        }
    }

    fn describe(&self) -> String {
        match self {
            Bnd4Entry::Plain(e) => e.describe(),
            Bnd4Entry::Signed { md5, crypto, inner, .. } => {
                let mut layers = vec![md5.descriptor().to_string()];
                if let Some(c) = crypto {
                    layers.push(c.descriptor().to_string());
                }
                if let Some(o) = inner.opened() {
                    layers.extend(o.layers().iter().map(|l| l.to_string()));
                }
                format!("{}: {} via {} (slot header handled by bnd4)", inner.name, inner.codec_id(), layers.join(" → "))
            }
        }
    }
}

impl Bnd4Document {
    /// Parses a binder and opens every entry through the pipeline of `ctx`.
    pub fn parse(data: &[u8], ctx: &DetectContext) -> CodecResult<Bnd4Document> {
        let Layout { header, files } = parse_layout(data)?;
        let keys = unique_keys(&files);
        let entries: Vec<Bnd4Entry> = files
            .iter()
            .zip(&keys)
            .map(|(f, k)| {
                let start = f.data_offset as usize;
                let end = start + f.size as usize;
                let name = f.name.clone().unwrap_or_else(|| k.clone());
                Bnd4Entry::open(ctx, &name, &data[start..end])
            })
            .collect();
        let mut order: Vec<usize> = (0..files.len()).filter(|&i| files[i].size > 0).collect();
        order.sort_by_key(|&i| files[i].data_offset);
        let mut segments = Vec::with_capacity(order.len() * 2 + 1);
        let mut pos = 0usize;
        for i in order {
            let start = files[i].data_offset as usize;
            if start > pos {
                segments.push(Segment::Verbatim(data[pos..start].to_vec()));
            }
            segments.push(Segment::Entry(i));
            pos = start + files[i].size as usize;
        }
        if pos < data.len() {
            segments.push(Segment::Verbatim(data[pos..].to_vec()));
        }
        Ok(Bnd4Document { header, files, keys, entries, segments })
    }

    /// The binder header.
    pub fn header(&self) -> &Bnd4Header {
        &self.header
    }

    /// File headers in file order.
    pub fn files(&self) -> &[Bnd4FileHeader] {
        &self.files
    }

    /// AST keys of the entries, in file order.
    pub fn entry_names(&self) -> &[String] {
        &self.keys
    }

    /// The opened entries, in file order.
    pub fn entries(&self) -> &[Bnd4Entry] {
        &self.entries
    }
}

impl Document for Bnd4Document {
    fn codec_id(&self) -> &'static str {
        CODEC
    }

    fn to_ast(&self) -> Node {
        let mut m = NodeMap::with_capacity(self.entries.len());
        for (k, e) in self.keys.iter().zip(&self.entries) {
            m.insert(k.clone(), e.to_ast());
        }
        Node::Map(m)
    }

    fn apply_ast(&mut self, root: &Node) -> CodecResult<()> {
        let Node::Map(m) = root else {
            return Err(CodecError::type_mismatch(CODEC, "$", "map (entry name → entry)", root.kind().name()));
        };
        if m.len() != self.keys.len() {
            return Err(CodecError::structure(
                CODEC,
                "$",
                format!(
                    "a BND4 save has a fixed set of {} entries; adding or removing entries is not supported",
                    self.keys.len()
                ),
            ));
        }
        for k in m.keys() {
            if !self.keys.contains(k) {
                return Err(CodecError::structure(CODEC, k, "unknown entry; entries cannot be added or renamed"));
            }
        }
        let mut work = self.entries.clone();
        for (k, e) in self.keys.iter().zip(work.iter_mut()) {
            let Some(node) = m.get(k) else {
                return Err(CodecError::path_not_found(CODEC, k));
            };
            e.apply_ast(node)?;
            if e.is_dirty() {
                let len = e.encode()?.len();
                if len != e.original().len() {
                    return Err(CodecError::structure(
                        CODEC,
                        k,
                        format!(
                            "BND4 save entries are fixed-size: entry is {} bytes, the edit would make it {len}",
                            e.original().len()
                        ),
                    ));
                }
            }
        }
        self.entries = work;
        Ok(())
    }

    fn encode(&self) -> CodecResult<Vec<u8>> {
        let total: usize = self
            .segments
            .iter()
            .map(|s| match s {
                Segment::Verbatim(b) => b.len(),
                Segment::Entry(i) => self.files[*i].size as usize,
            })
            .sum();
        let mut out = Vec::with_capacity(total);
        for s in &self.segments {
            match s {
                Segment::Verbatim(b) => out.extend_from_slice(b),
                Segment::Entry(i) => {
                    let bytes = self.entries[*i].encode()?;
                    if bytes.len() as u64 != self.files[*i].size {
                        return Err(CodecError::structure(
                            CODEC,
                            &self.keys[*i],
                            format!(
                                "entry re-encoded to {} bytes but its slot holds {}",
                                bytes.len(),
                                self.files[*i].size
                            ),
                        ));
                    }
                    out.extend_from_slice(&bytes);
                }
            }
        }
        Ok(out)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::VALUES_ONLY
    }

    fn endianness(&self) -> Endianness {
        if self.header.big_endian {
            Endianness::Big
        } else {
            Endianness::Little
        }
    }

    fn notes(&self) -> Vec<String> {
        let h = &self.header;
        let mut n = vec![format!(
            "BND4 version {:?}, {} entries, format {:#04x} (raw {:#04x}), {} names, {}-endian, extended {:#x}",
            String::from_utf8_lossy(&h.version),
            h.file_count,
            h.format,
            h.raw_format,
            if h.unicode { "UTF-16" } else { "single-byte" },
            if h.big_endian { "big" } else { "little" },
            h.extended
        )];
        if !h.unicode && self.files.iter().any(|f| f.name.as_deref().is_some_and(|s| s.contains("\\x"))) {
            n.push(
                "non-ASCII (Shift-JIS) name bytes are shown escaped as \\xNN; the name table is kept verbatim".into(),
            );
        }
        if h.has(format::COMPRESSION) {
            n.push("binder declares per-entry compression (DCX): compressed entries are edited as raw bytes".into());
        }
        for e in &self.entries {
            n.push(e.describe());
        }
        n
    }

    fn clone_box(&self) -> Box<dyn Document> {
        Box::new(self.clone())
    }
}

/// The BND4 codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct Bnd4Codec;

impl StructuralCodec for Bnd4Codec {
    fn id(&self) -> &'static str {
        CODEC
    }

    fn name(&self) -> &'static str {
        "FromSoftware BND4 (.sl2)"
    }

    fn detect(&self, bytes: &[u8], _ctx: &DetectContext) -> Confidence {
        if !bytes.starts_with(b"BND4") {
            return 0;
        }
        if parse_layout(bytes).is_ok() {
            95
        } else {
            0
        }
    }

    fn decode(&self, bytes: &[u8], ctx: &DetectContext) -> CodecResult<Box<dyn Document>> {
        Ok(Box::new(Bnd4Document::parse(bytes, ctx)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::crypto::cipher::{aes_encrypt, AesMode, Padding};
    use md5::{Digest, Md5};
    use savelens_core::{Location, NodePath, RoundTripStatus, ScalarValue, ScanType};

    fn md5(data: &[u8]) -> [u8; 16] {
        let mut h = Md5::new();
        h.update(data);
        h.finalize().into()
    }

    /// Elden-Ring style slot: MD5 of the body, then the body.
    fn signed_slot(body: &[u8]) -> Vec<u8> {
        let mut v = md5(body).to_vec();
        v.extend_from_slice(body);
        v
    }

    fn body(seed: u32, len: usize) -> Vec<u8> {
        let mut v: Vec<u8> = (0..len as u32).map(|i| ((i.wrapping_mul(seed) >> 3) % 97) as u8).collect();
        if len >= 0x48 {
            v[0x40..0x44].copy_from_slice(&1234u32.to_le_bytes());
        }
        v
    }

    /// Test-side BND4 writer (Elden Ring layout: format Names1, UTF-16
    /// names, 0x20-byte file headers, payloads aligned to 0x10).
    fn write_bnd4(entries: &[(&str, Vec<u8>)], unicode: bool) -> Vec<u8> {
        let fhs = 0x20usize;
        let mut names = Vec::new();
        let mut name_offsets = Vec::new();
        let names_start = HEADER_LEN + entries.len() * fhs;
        for (n, _) in entries {
            name_offsets.push(names_start + names.len());
            if unicode {
                for u in n.encode_utf16() {
                    names.extend_from_slice(&u.to_le_bytes());
                }
                names.extend_from_slice(&[0, 0]);
            } else {
                names.extend_from_slice(n.as_bytes());
                names.push(0);
            }
        }
        let mut data_start = names_start + names.len();
        // Gap of padding bytes before the data (kept verbatim).
        data_start = (data_start + 0x1f) & !0xf;
        let mut out = Vec::new();
        out.extend_from_slice(b"BND4");
        out.extend_from_slice(&[0, 0, 0, 0, 0, 0, 1, 0]);
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        out.extend_from_slice(&0x40u64.to_le_bytes());
        out.extend_from_slice(b"00000001");
        out.extend_from_slice(&(fhs as u64).to_le_bytes());
        out.extend_from_slice(&(data_start as u64).to_le_bytes());
        out.extend_from_slice(&[u8::from(unicode), 0x20, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&0u64.to_le_bytes());
        let mut off = data_start;
        for (i, (_, payload)) in entries.iter().enumerate() {
            out.extend_from_slice(&[0x50, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
            out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
            out.extend_from_slice(&(off as u32).to_le_bytes());
            out.extend_from_slice(&(name_offsets[i] as u32).to_le_bytes());
            out.extend_from_slice(&(i as i32).to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            off += (payload.len() + 0xf) & !0xf;
        }
        out.extend_from_slice(&names);
        out.resize(data_start, 0xcd);
        for (_, payload) in entries {
            out.extend_from_slice(payload);
            let aligned = (out.len() + 0xf) & !0xf;
            out.resize(aligned, 0xee);
        }
        out
    }

    fn open(bytes: &[u8]) -> crate::pipeline::OpenedSave {
        EnvelopePipeline::builtin().open(bytes, &DetectContext::for_file("ER0000.sl2")).unwrap()
    }

    #[test]
    fn parses_header_and_names() {
        let file = write_bnd4(&[("USER_DATA000", signed_slot(&body(7, 300))), ("USER_DATA001", vec![1, 2, 3])], true);
        let layout = parse_layout(&file).unwrap();
        assert_eq!(layout.header.format, format::NAMES1);
        assert_eq!(layout.header.raw_format, 0x20);
        assert!(layout.header.unicode);
        assert_eq!(layout.files.len(), 2);
        assert_eq!(layout.files[0].name.as_deref(), Some("USER_DATA000"));
        assert_eq!(layout.files[1].id, Some(1));
        assert_eq!(Bnd4Codec.detect(&file, &DetectContext::default()), 95);
        let ascii = write_bnd4(&[("a.bin", vec![9; 20])], false);
        assert_eq!(parse_layout(&ascii).unwrap().files[0].name.as_deref(), Some("a.bin"));
    }

    #[test]
    fn lossless_and_md5_resigned_after_raw_edit() {
        let s0 = signed_slot(&body(7, 600));
        let s1 = signed_slot(&body(13, 600));
        let file = write_bnd4(&[("USER_DATA000", s0.clone()), ("USER_DATA001", s1.clone())], true);
        let mut s = open(&file);
        assert_eq!(s.codec_id(), CODEC);
        assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
        assert_eq!(s.encode().unwrap(), file);
        let root = s.root();
        let Node::Map(m) = &root else { panic!("{root:?}") };
        assert_eq!(m.keys().collect::<Vec<_>>(), vec!["USER_DATA000", "USER_DATA001"]);
        assert!(matches!(m["USER_DATA000"], Node::RawRegion(_)), "MD5 header stripped, raw slot exposed");
        assert!(
            s.envelope().notes.iter().any(|n| n.contains("USER_DATA000") && n.contains("MD5")),
            "{:?}",
            s.envelope().notes
        );

        let loc = Location::Raw {
            region: NodePath::root().key("USER_DATA000"),
            offset: 0x40,
            ty: ScanType::U32,
            endian: Endianness::Little,
        };
        assert_eq!(loc.read(&root), Some(ScalarValue::Int(1234)));
        s.write_location(&loc, ScalarValue::Int(987_654)).unwrap();
        let out = s.encode_verified(&EnvelopePipeline::builtin(), &DetectContext::for_file("ER0000.sl2")).unwrap();
        assert_eq!(out.len(), file.len());
        let layout = parse_layout(&out).unwrap();
        let f0 = &layout.files[0];
        let e0 = &out[f0.data_offset as usize..(f0.data_offset + f0.size) as usize];
        assert_eq!(&e0[16 + 0x40..16 + 0x44], &987_654u32.to_le_bytes());
        assert_eq!(&e0[..16], &md5(&e0[16..]), "MD5 recomputed");
        assert_ne!(e0, &s0[..]);
        let f1 = &layout.files[1];
        assert_eq!(&out[f1.data_offset as usize..(f1.data_offset + f1.size) as usize], &s1[..]);
        // Everything outside the edited slot is untouched.
        let start = f0.data_offset as usize;
        assert_eq!(&out[..start], &file[..start]);
        assert_eq!(&out[start + s0.len()..], &file[start + s0.len()..]);
    }

    #[test]
    fn size_changes_and_unknown_entries_are_rejected() {
        let file = write_bnd4(&[("a.json", br#"{"gold":10}"#.to_vec()), ("b", vec![0; 40])], false);
        let mut s = open(&file);
        assert_eq!(s.encode().unwrap(), file);
        let root = s.root();
        // JSON entry: growing the value changes the entry size.
        let mut grown = root.clone();
        grown.set(&NodePath::root().key("a.json").key("gold"), Node::I64(123_456), false).unwrap();
        let err = s.apply_ast(&grown).unwrap_err();
        assert!(matches!(err, CodecError::StructureUnsupported { .. }), "{err}");
        // Same-size edit works.
        let mut same = root.clone();
        same.set(&NodePath::root().key("a.json").key("gold"), Node::I64(99), false).unwrap();
        s.apply_ast(&same).unwrap();
        let out = s.encode().unwrap();
        assert!(out.windows(11).any(|w| w == br#"{"gold":99}"#));
        // Adding an entry is a structural error.
        let mut added = s.root();
        added.set(&NodePath::root().key("c"), Node::Bytes(vec![1]), true).unwrap();
        assert!(matches!(s.apply_ast(&added), Err(CodecError::StructureUnsupported { .. })));
        assert!(s.apply_ast(&Node::I32(1)).is_err());
    }

    #[test]
    fn dark_souls_3_slots_are_decrypted_and_resigned() {
        let key = hex::decode("FD464D695E69A39A10E319A7ACE8B7FA").unwrap();
        let iv = [0x11u8; 16];
        let mut plain = vec![0u8; 256];
        plain[..4].copy_from_slice(&200u32.to_le_bytes());
        plain[32..36].copy_from_slice(&5555u32.to_le_bytes());
        plain[40..52].copy_from_slice(b"AshenOneName");
        let ct = aes_encrypt(AesMode::Cbc, &key, &iv, Padding::None, &plain).unwrap();
        let mut enc = iv.to_vec();
        enc.extend_from_slice(&ct);
        let slot = signed_slot(&enc);
        let file = write_bnd4(&[("USER_DATA000", slot.clone()), ("USER_DATA001", signed_slot(&body(3, 64)))], true);
        let ctx = DetectContext::for_game("DS30000.sl2", Some(374_320));
        let p = EnvelopePipeline::builtin();
        let mut s = p.open(&file, &ctx).unwrap();
        assert_eq!(s.encode().unwrap(), file);
        let notes = s.envelope().notes.join("\n");
        assert!(notes.contains("dark-souls-3"), "{notes}");
        let doc = Bnd4Document::parse(&file, &ctx).unwrap();
        assert_eq!(doc.entries()[0].cipher_profile(), Some("dark-souls-3"));
        assert_eq!(doc.entries()[1].cipher_profile(), None, "plaintext slot is not decrypted");
        let loc = Location::Raw {
            region: NodePath::root().key("USER_DATA000"),
            offset: 32,
            ty: ScanType::U32,
            endian: Endianness::Little,
        };
        assert_eq!(loc.read(&s.root()), Some(ScalarValue::Int(5555)), "{notes}");
        s.write_location(&loc, ScalarValue::Int(99)).unwrap();
        let out = s.encode_verified(&p, &ctx).unwrap();
        let f0 = &parse_layout(&out).unwrap().files[0];
        let e0 = &out[f0.data_offset as usize..(f0.data_offset + f0.size) as usize];
        assert_eq!(&e0[..16], &md5(&e0[16..]));
        assert_eq!(&e0[16..32], &iv, "IV kept");
        let dec =
            crate::layers::crypto::cipher::aes_decrypt(AesMode::Cbc, &key, &iv, Padding::None, &e0[32..]).unwrap();
        assert_eq!(&dec[32..36], &99u32.to_le_bytes());
        assert_eq!(&dec[40..52], b"AshenOneName");
    }

    #[test]
    fn checksum_hunter_also_finds_the_slot_md5() {
        // The rule the codec checks explicitly is the one the hunter finds.
        let slot = signed_slot(&body(7, 600));
        let s = EnvelopePipeline::builtin().open(&slot, &DetectContext::for_file("slot.bin")).unwrap();
        assert!(
            matches!(s.layers()[0], savelens_core::LayerDescriptor::Checksum { rule } if rule == MD5_HEADER),
            "{:?}",
            s.layers()
        );
        let file = write_bnd4(&[("USER_DATA000", slot)], true);
        let doc = Bnd4Document::parse(&file, &DetectContext::default()).unwrap();
        assert!(doc.entries()[0].is_md5_signed());
        assert_eq!(doc.entries()[0].cipher_profile(), None);
        // A corrupt MD5 leaves the entry to the generic pipeline.
        let mut bad = file.clone();
        let off = doc.files()[0].data_offset as usize;
        bad[off] ^= 1;
        let doc = Bnd4Document::parse(&bad, &DetectContext::default()).unwrap();
        assert!(!doc.entries()[0].is_md5_signed());
        assert_eq!(doc.encode().unwrap(), bad);
    }

    #[test]
    fn malformed_binders_are_rejected() {
        let file = write_bnd4(&[("x", vec![1; 32])], true);
        assert!(parse_layout(&file[..0x30]).is_err());
        let mut bad = file.clone();
        bad[0x0c..0x10].copy_from_slice(&0x7fff_ffffu32.to_le_bytes());
        assert!(parse_layout(&bad).is_err());
        let mut oob = file.clone();
        oob[0x48..0x50].copy_from_slice(&0x10_0000u64.to_le_bytes());
        assert!(parse_layout(&oob).is_err());
        assert_eq!(Bnd4Codec.detect(b"BND4", &DetectContext::default()), 0);
        assert_eq!(Bnd4Codec.detect(b"{}", &DetectContext::default()), 0);
    }
}
