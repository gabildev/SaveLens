//! ZIP archive codec (Ren'Py saves, many Java / Godot / Unity games).
//!
//! **Layout.** The parser reads the end-of-central-directory record (and the
//! ZIP64 locator + record when present), every central-directory record and
//! the local file header, data and optional data descriptor of every entry.
//! Everything is kept as byte ranges of the original archive: headers with
//! their timestamps, version and flag fields, extra fields (ZIP64 included),
//! gaps between entries, bytes between the last entry and the central
//! directory, the archive comment and trailing bytes. Unedited archives are
//! rebuilt from these pieces bit for bit.
//!
//! **Entries.** Stored (method 0) and deflated (method 8) entries are
//! decompressed and opened through the full pipeline with [`NestedEntry`], so
//! a `json` entry becomes a JSON tree, a pickled `log` a pickle tree and a
//! PNG a raw region. For deflated entries the codec searches the compression
//! level that reproduces the original stream; when none does, the entry is
//! still editable but a re-compressed entry is not bit-identical to what the
//! original tool would have written (a note says so). Encrypted entries and
//! other compression methods stay opaque `Bytes` and cannot be edited.
//!
//! **Edits.** Only edited entries are re-encoded; their CRC-32, compressed
//! and uncompressed sizes are patched in the local header, the data
//! descriptor, the central directory and the ZIP64 extra fields (each field
//! is patched only when it held the real value, so streamed headers with
//! zeroed fields stay zeroed). Local-header offsets of every following entry,
//! the central-directory offset and the ZIP64 locator are recomputed.
//! Adding, removing or renaming entries is rejected.

use std::sync::Arc;

use savelens_core::{bit_equal, Node, NodeMap, NodePath};

use super::container::NestedEntry;
use crate::document::{Capabilities, Confidence, DetectContext, Document, StructuralCodec};
use crate::error::{CodecError, CodecResult};
use crate::layers::compression::{deflate, inflate};

const CODEC: &str = "zip";
const SIG_LOCAL: u32 = 0x0403_4b50;
const SIG_CENTRAL: u32 = 0x0201_4b50;
const SIG_EOCD: u32 = 0x0605_4b50;
const SIG_EOCD64: u32 = 0x0606_4b50;
const SIG_LOCATOR: u32 = 0x0706_4b50;
const SIG_DESCRIPTOR: u32 = 0x0807_4b50;
const MASK32: u32 = 0xffff_ffff;
const ZIP64_EXTRA: u16 = 0x0001;
/// Deflate levels tried to reproduce an entry, most common first.
const LEVELS: [u32; 10] = [6, 9, 1, 2, 3, 4, 5, 7, 8, 0];
/// Deflated entries larger than this are not searched for an exact level.
const LEVEL_SEARCH_LIMIT: usize = 64 << 20;

fn u16_at(d: &[u8], pos: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(pos..pos.checked_add(2)?)?.try_into().ok()?))
}

fn u32_at(d: &[u8], pos: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(pos..pos.checked_add(4)?)?.try_into().ok()?))
}

fn u64_at(d: &[u8], pos: usize) -> Option<u64> {
    Some(u64::from_le_bytes(d.get(pos..pos.checked_add(8)?)?.try_into().ok()?))
}

fn truncated(pos: usize, what: &str) -> CodecError {
    CodecError::malformed(CODEC, pos, format!("truncated {what}"))
}

fn to_usize(v: u64, pos: usize, what: &str) -> CodecResult<usize> {
    usize::try_from(v).map_err(|_| CodecError::malformed(CODEC, pos, format!("{what} does not fit in memory")))
}

/// Which header a patchable field lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Local,
    Descriptor,
    Central,
}

/// What a patchable field holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum What {
    Crc,
    Compressed,
    Uncompressed,
    Offset,
}

/// A size / CRC / offset field, relative to the start of its header.
#[derive(Debug, Clone, Copy)]
struct Field {
    part: Part,
    what: What,
    pos: usize,
    width: u8,
}

/// A field after the central directory that depends on its position.
#[derive(Debug, Clone, Copy)]
enum TailKind {
    /// Holds the central-directory offset.
    CdOffset,
    /// Holds an offset after the central directory (ZIP64 record offset).
    Shifted(u64),
}

#[derive(Debug, Clone, Copy)]
struct TailField {
    /// Absolute position in the original archive.
    pos: usize,
    width: u8,
    kind: TailKind,
}

/// Central-directory layout (enough for detection).
struct CdLayout {
    cd_start: usize,
    cd_end: usize,
    records: Vec<(usize, usize)>,
    tail_fields: Vec<TailField>,
    zip64: bool,
    comment_len: usize,
}

fn find_eocd(d: &[u8]) -> Option<usize> {
    if d.len() < 22 {
        return None;
    }
    let min = d.len().saturating_sub(22 + 0xffff);
    let mut fallback = None;
    for pos in (min..=d.len() - 22).rev() {
        if u32_at(d, pos) != Some(SIG_EOCD) {
            continue;
        }
        let clen = usize::from(u16_at(d, pos + 20)?);
        if pos + 22 + clen == d.len() {
            return Some(pos);
        }
        if fallback.is_none() && pos + 22 + clen <= d.len() {
            fallback = Some(pos);
        }
    }
    fallback
}

fn locate(d: &[u8]) -> CodecResult<CdLayout> {
    let eocd =
        find_eocd(d).ok_or_else(|| CodecError::malformed(CODEC, d.len(), "no end-of-central-directory record"))?;
    let r16 = |p: usize| u16_at(d, p).ok_or_else(|| truncated(p, "end-of-central-directory record"));
    let r32 = |p: usize| u32_at(d, p).ok_or_else(|| truncated(p, "end-of-central-directory record"));
    let mut disk = u32::from(r16(eocd + 4)?);
    let mut cd_disk = u32::from(r16(eocd + 6)?);
    let mut total = u64::from(r16(eocd + 10)?);
    let cd_size32 = r32(eocd + 12)?;
    let cd_off32 = r32(eocd + 16)?;
    let comment_len = usize::from(r16(eocd + 20)?);
    let mut cd_size = u64::from(cd_size32);
    let mut cd_off = u64::from(cd_off32);
    let mut tail_fields = Vec::new();
    if cd_off32 != MASK32 {
        tail_fields.push(TailField { pos: eocd + 16, width: 4, kind: TailKind::CdOffset });
    }
    let mut cd_limit = eocd;
    let mut zip64 = false;
    if eocd >= 20 && u32_at(d, eocd - 20) == Some(SIG_LOCATOR) {
        let loc = eocd - 20;
        let rec = u64_at(d, loc + 8).ok_or_else(|| truncated(loc, "ZIP64 locator"))?;
        let rec_pos = to_usize(rec, loc, "ZIP64 record offset")?;
        if u32_at(d, rec_pos) != Some(SIG_EOCD64) || rec_pos + 56 > loc {
            return Err(CodecError::malformed(CODEC, loc, "ZIP64 locator does not point to a ZIP64 record"));
        }
        let t = |p: usize| truncated(p, "ZIP64 end-of-central-directory record");
        disk = u32_at(d, rec_pos + 16).ok_or_else(|| t(rec_pos))?;
        cd_disk = u32_at(d, rec_pos + 20).ok_or_else(|| t(rec_pos))?;
        total = u64_at(d, rec_pos + 32).ok_or_else(|| t(rec_pos))?;
        cd_size = u64_at(d, rec_pos + 40).ok_or_else(|| t(rec_pos))?;
        cd_off = u64_at(d, rec_pos + 48).ok_or_else(|| t(rec_pos))?;
        tail_fields.push(TailField { pos: loc + 8, width: 8, kind: TailKind::Shifted(rec) });
        tail_fields.push(TailField { pos: rec_pos + 48, width: 8, kind: TailKind::CdOffset });
        cd_limit = rec_pos;
        zip64 = true;
    }
    let single_disk = |v: u32| v == 0 || v == 0xffff || v == MASK32;
    if !single_disk(disk) || !single_disk(cd_disk) {
        return Err(CodecError::unsupported(CODEC, "multi-disk (spanned) ZIP archives"));
    }
    let cd_start = to_usize(cd_off, eocd, "central-directory offset")?;
    let cd_end_declared = cd_start
        .checked_add(to_usize(cd_size, eocd, "central-directory size")?)
        .filter(|&e| e <= cd_limit)
        .ok_or_else(|| CodecError::malformed(CODEC, eocd, "central directory lies outside the archive"))?;
    let mut records = Vec::new();
    let mut pos = cd_start;
    while pos + 46 <= cd_end_declared && u32_at(d, pos) == Some(SIG_CENTRAL) {
        let n = usize::from(u16_at(d, pos + 28).ok_or_else(|| truncated(pos, "central-directory record"))?);
        let e = usize::from(u16_at(d, pos + 30).ok_or_else(|| truncated(pos, "central-directory record"))?);
        let c = usize::from(u16_at(d, pos + 32).ok_or_else(|| truncated(pos, "central-directory record"))?);
        let end = pos + 46 + n + e + c;
        if end > cd_end_declared {
            return Err(truncated(pos, "central-directory record"));
        }
        records.push((pos, end));
        pos = end;
    }
    if records.len() as u64 != total {
        return Err(CodecError::malformed(
            CODEC,
            pos,
            format!("central directory declares {total} entries but {} records were found", records.len()),
        ));
    }
    Ok(CdLayout { cd_start, cd_end: pos, records, tail_fields, zip64, comment_len })
}

/// Iterates the extra-field blocks of a header: `(id, data offset, size)`.
fn extra_blocks(extra: &[u8]) -> Vec<(u16, usize, usize)> {
    let mut out = Vec::new();
    let mut p = 0;
    while p + 4 <= extra.len() {
        let (Some(id), Some(size)) = (u16_at(extra, p), u16_at(extra, p + 2)) else { break };
        let size = usize::from(size);
        if p + 4 + size > extra.len() {
            break;
        }
        out.push((id, p + 4, size));
        p += 4 + size;
    }
    out
}

/// How an entry's data is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    Stored,
    Deflate { level: u32, exact: bool },
}

#[derive(Debug, Clone)]
enum Content {
    Entry { entry: Box<NestedEntry>, method: Method },
    Opaque { reason: String },
}

#[derive(Debug, Clone)]
struct ZipEntry {
    name: String,
    /// Local header start, header end (= data start), data end, descriptor end
    /// and end of the bytes kept after the entry (next header / central directory).
    lho: usize,
    lfh_end: usize,
    data_end: usize,
    desc_end: usize,
    gap_end: usize,
    /// Central-directory record span.
    cd_start: usize,
    cd_end: usize,
    fields: Vec<Field>,
    content: Content,
    descriptor_missing: bool,
}

struct Payload {
    data: Vec<u8>,
    crc: u32,
    csize: u64,
    usize: u64,
}

impl ZipEntry {
    fn payload(&self) -> CodecResult<Option<Payload>> {
        let Content::Entry { entry, method } = &self.content else { return Ok(None) };
        if !entry.is_dirty() {
            return Ok(None);
        }
        let raw = entry.encode()?;
        if raw == entry.original() {
            return Ok(None);
        }
        let data = match method {
            Method::Stored => raw.clone(),
            Method::Deflate { level, .. } => deflate(&raw, *level, false)?,
        };
        Ok(Some(Payload { crc: crc32fast::hash(&raw), csize: data.len() as u64, usize: raw.len() as u64, data }))
    }
}

fn put(buf: &mut [u8], f: &Field, value: u64, name: &str) -> CodecResult<()> {
    let slot = buf
        .get_mut(f.pos..f.pos + usize::from(f.width))
        .ok_or_else(|| CodecError::malformed(CODEC, f.pos, format!("field of entry `{name}` outside its header")))?;
    if f.width == 4 {
        let v = u32::try_from(value).ok().filter(|&v| v != MASK32 || f.what == What::Crc).ok_or_else(|| {
            CodecError::unencodable(CODEC, name, format!("value {value} needs ZIP64 fields this entry does not have"))
        })?;
        slot.copy_from_slice(&v.to_le_bytes());
    } else {
        slot.copy_from_slice(&value.to_le_bytes());
    }
    Ok(())
}

/// A parsed ZIP archive.
#[derive(Debug, Clone)]
pub struct ZipDocument {
    orig: Arc<Vec<u8>>,
    entries: Vec<ZipEntry>,
    /// Entry indices in local-header order.
    file_order: Vec<usize>,
    /// Bytes before the first local header.
    head_end: usize,
    cd_start: usize,
    cd_end: usize,
    tail_fields: Vec<TailField>,
    keys: Vec<String>,
    zip64: bool,
    comment_len: usize,
}

fn unique_key(keys: &[String], name: &str) -> String {
    if !keys.iter().any(|k| k == name) {
        return name.to_string();
    }
    (2..).map(|n| format!("{name}#{n}")).find(|k| !keys.contains(k)).unwrap_or_else(|| name.to_string())
}

impl ZipDocument {
    /// Parses an archive and opens every entry through the pipeline of `ctx`.
    pub fn parse(d: &[u8], ctx: &DetectContext) -> CodecResult<ZipDocument> {
        let layout = locate(d)?;
        let mut entries = Vec::with_capacity(layout.records.len());
        for &(rs, re) in &layout.records {
            entries.push(Self::parse_entry(d, rs, re, ctx)?);
        }
        let mut file_order: Vec<usize> = (0..entries.len()).collect();
        file_order.sort_by_key(|&i| entries[i].lho);
        for w in file_order.windows(2) {
            let (a, b) = (&entries[w[0]], &entries[w[1]]);
            if a.desc_end > b.lho {
                return Err(CodecError::malformed(
                    CODEC,
                    b.lho,
                    format!("entries `{}` and `{}` overlap", a.name, b.name),
                ));
            }
        }
        for (k, &i) in file_order.iter().enumerate() {
            let next = file_order.get(k + 1).map(|&j| entries[j].lho).unwrap_or(layout.cd_start);
            if entries[i].desc_end > next {
                return Err(CodecError::malformed(
                    CODEC,
                    next,
                    format!("entry `{}` overlaps the central directory", entries[i].name),
                ));
            }
            entries[i].gap_end = next;
        }
        let head_end = file_order.first().map(|&i| entries[i].lho).unwrap_or(layout.cd_start);
        let mut keys: Vec<String> = Vec::with_capacity(entries.len());
        for e in &entries {
            let k = unique_key(&keys, &e.name);
            keys.push(k);
        }
        Ok(ZipDocument {
            orig: Arc::new(d.to_vec()),
            entries,
            file_order,
            head_end,
            cd_start: layout.cd_start,
            cd_end: layout.cd_end,
            tail_fields: layout.tail_fields,
            keys,
            zip64: layout.zip64,
            comment_len: layout.comment_len,
        })
    }

    fn parse_entry(d: &[u8], rs: usize, re: usize, ctx: &DetectContext) -> CodecResult<ZipEntry> {
        let rec = &d[rs..re];
        let t = || truncated(rs, "central-directory record");
        let flags = u16_at(rec, 8).ok_or_else(t)?;
        let method = u16_at(rec, 10).ok_or_else(t)?;
        let crc = u32_at(rec, 16).ok_or_else(t)?;
        let csize32 = u32_at(rec, 20).ok_or_else(t)?;
        let usize32 = u32_at(rec, 24).ok_or_else(t)?;
        let nlen = usize::from(u16_at(rec, 28).ok_or_else(t)?);
        let elen = usize::from(u16_at(rec, 30).ok_or_else(t)?);
        let disk = u16_at(rec, 34).ok_or_else(t)?;
        let lho32 = u32_at(rec, 42).ok_or_else(t)?;
        let name_bytes = rec.get(46..46 + nlen).ok_or_else(t)?;
        let name = String::from_utf8_lossy(name_bytes).into_owned();
        if disk != 0 && disk != 0xffff {
            return Err(CodecError::unsupported(CODEC, format!("entry `{name}` lives on another disk")));
        }
        let mut fields = vec![Field { part: Part::Central, what: What::Crc, pos: 16, width: 4 }];
        let (mut csize, mut usize_, mut lho) = (u64::from(csize32), u64::from(usize32), u64::from(lho32));
        if usize32 != MASK32 {
            fields.push(Field { part: Part::Central, what: What::Uncompressed, pos: 24, width: 4 });
        }
        if csize32 != MASK32 {
            fields.push(Field { part: Part::Central, what: What::Compressed, pos: 20, width: 4 });
        }
        if lho32 != MASK32 {
            fields.push(Field { part: Part::Central, what: What::Offset, pos: 42, width: 4 });
        }
        if usize32 == MASK32 || csize32 == MASK32 || lho32 == MASK32 {
            let extra_at = 46 + nlen;
            let extra = rec.get(extra_at..extra_at + elen).ok_or_else(t)?;
            let (_, off, size) = extra_blocks(extra).into_iter().find(|b| b.0 == ZIP64_EXTRA).ok_or_else(|| {
                CodecError::malformed(CODEC, rs, format!("entry `{name}` lacks its ZIP64 extra field"))
            })?;
            let mut p = 0;
            for (masked, what) in [
                (usize32 == MASK32, What::Uncompressed),
                (csize32 == MASK32, What::Compressed),
                (lho32 == MASK32, What::Offset),
            ] {
                if !masked {
                    continue;
                }
                if p + 8 > size {
                    return Err(CodecError::malformed(CODEC, rs, format!("entry `{name}`: short ZIP64 extra field")));
                }
                let pos = extra_at + off + p;
                let v = u64_at(rec, pos).ok_or_else(t)?;
                match what {
                    What::Uncompressed => usize_ = v,
                    What::Compressed => csize = v,
                    _ => lho = v,
                }
                fields.push(Field { part: Part::Central, what, pos, width: 8 });
                p += 8;
            }
        }
        let lho = to_usize(lho, rs, "local header offset")?;
        let csize_n = to_usize(csize, rs, "compressed size")?;

        // Local file header.
        let lt = || truncated(lho, "local file header");
        if u32_at(d, lho) != Some(SIG_LOCAL) {
            return Err(CodecError::malformed(CODEC, lho, format!("entry `{name}`: missing local file header")));
        }
        let lnlen = usize::from(u16_at(d, lho + 26).ok_or_else(lt)?);
        let lelen = usize::from(u16_at(d, lho + 28).ok_or_else(lt)?);
        let lfh_end = lho + 30 + lnlen + lelen;
        if lfh_end > d.len() {
            return Err(lt());
        }
        let lcrc = u32_at(d, lho + 14).ok_or_else(lt)?;
        let lcsize = u32_at(d, lho + 18).ok_or_else(lt)?;
        let lusize = u32_at(d, lho + 22).ok_or_else(lt)?;
        if lcrc == crc {
            fields.push(Field { part: Part::Local, what: What::Crc, pos: 14, width: 4 });
        }
        if lcsize != MASK32 && u64::from(lcsize) == csize {
            fields.push(Field { part: Part::Local, what: What::Compressed, pos: 18, width: 4 });
        }
        if lusize != MASK32 && u64::from(lusize) == usize_ {
            fields.push(Field { part: Part::Local, what: What::Uncompressed, pos: 22, width: 4 });
        }
        let mut local_zip64 = false;
        if lcsize == MASK32 || lusize == MASK32 {
            let extra_at = 30 + lnlen;
            let extra = &d[lho + extra_at..lfh_end];
            if let Some((_, off, size)) = extra_blocks(extra).into_iter().find(|b| b.0 == ZIP64_EXTRA) {
                local_zip64 = true;
                let slots: Vec<What> = if size >= 16 {
                    vec![What::Uncompressed, What::Compressed]
                } else {
                    [(lusize == MASK32, What::Uncompressed), (lcsize == MASK32, What::Compressed)]
                        .into_iter()
                        .filter(|s| s.0)
                        .map(|s| s.1)
                        .collect()
                };
                for (k, what) in slots.into_iter().enumerate() {
                    let pos = extra_at + off + k * 8;
                    if k * 8 + 8 > size {
                        break;
                    }
                    let v = u64_at(d, lho + pos).ok_or_else(lt)?;
                    let truth = if what == What::Uncompressed { usize_ } else { csize };
                    if v == truth {
                        fields.push(Field { part: Part::Local, what, pos, width: 8 });
                    }
                }
            }
        }
        let data_end = lfh_end
            .checked_add(csize_n)
            .filter(|&e| e <= d.len())
            .ok_or_else(|| truncated(lfh_end, &format!("data of entry `{name}`")))?;

        // Optional data descriptor.
        let mut desc_end = data_end;
        let mut descriptor_missing = false;
        if flags & 0x08 != 0 {
            let wide_first = local_zip64 || csize > u64::from(MASK32 - 1) || usize_ > u64::from(MASK32 - 1);
            let order: [(bool, bool); 4] = if wide_first {
                [(true, true), (false, true), (true, false), (false, false)]
            } else {
                [(true, false), (true, true), (false, false), (false, true)]
            };
            let mut found = None;
            for (sig, wide) in order {
                let mut p = data_end;
                if sig {
                    if u32_at(d, p) != Some(SIG_DESCRIPTOR) {
                        continue;
                    }
                    p += 4;
                }
                let w = if wide { 8 } else { 4 };
                let read = |at: usize| if wide { u64_at(d, at) } else { u32_at(d, at).map(u64::from) };
                let (Some(c), Some(cs), Some(us)) = (u32_at(d, p), read(p + 4), read(p + 4 + w)) else { continue };
                if c == crc && cs == csize && us == usize_ {
                    found = Some((p - data_end, w, p + 4 + 2 * w));
                    break;
                }
            }
            match found {
                Some((base, w, end)) => {
                    let width = w as u8;
                    fields.push(Field { part: Part::Descriptor, what: What::Crc, pos: base, width: 4 });
                    fields.push(Field { part: Part::Descriptor, what: What::Compressed, pos: base + 4, width });
                    fields.push(Field { part: Part::Descriptor, what: What::Uncompressed, pos: base + 4 + w, width });
                    desc_end = end;
                }
                None => descriptor_missing = true,
            }
        }

        let data = &d[lfh_end..data_end];
        let content = if flags & 0x01 != 0 {
            Content::Opaque { reason: "encrypted".into() }
        } else {
            match method {
                0 if crc32fast::hash(data) == crc => {
                    Content::Entry { entry: Box::new(NestedEntry::open(ctx, &name, data)), method: Method::Stored }
                }
                0 => Content::Opaque { reason: "stored with a CRC-32 mismatch".into() },
                8 => match inflate(data, false) {
                    Ok((raw, used))
                        if used == data.len() && raw.len() as u64 == usize_ && crc32fast::hash(&raw) == crc =>
                    {
                        let level = if raw.len() <= LEVEL_SEARCH_LIMIT {
                            LEVELS.iter().copied().find(|&l| deflate(&raw, l, false).is_ok_and(|c| c == data))
                        } else {
                            None
                        };
                        let method = match level {
                            Some(level) => Method::Deflate { level, exact: true },
                            None => Method::Deflate { level: 6, exact: false },
                        };
                        Content::Entry { entry: Box::new(NestedEntry::open(ctx, &name, &raw)), method }
                    }
                    Ok(_) => Content::Opaque {
                        reason: "a deflate stream that does not match its recorded size / CRC-32".into(),
                    },
                    Err(e) => Content::Opaque { reason: format!("an invalid deflate stream ({e})") },
                },
                m => Content::Opaque { reason: format!("compressed with unsupported method {m}") },
            }
        };
        Ok(ZipEntry {
            name,
            lho,
            lfh_end,
            data_end,
            desc_end,
            gap_end: desc_end,
            cd_start: rs,
            cd_end: re,
            fields,
            content,
            descriptor_missing,
        })
    }

    fn index(&self, key: &str) -> Option<usize> {
        self.keys.iter().position(|k| k == key)
    }

    /// Entry keys of the AST map (entry names, duplicates suffixed `#2`, …),
    /// in central-directory order.
    pub fn entry_names(&self) -> &[String] {
        &self.keys
    }

    /// Codec of an entry (`opaque` for encrypted / unsupported entries).
    pub fn entry_codec(&self, key: &str) -> Option<&'static str> {
        let e = &self.entries[self.index(key)?];
        Some(match &e.content {
            Content::Entry { entry, .. } => entry.codec_id(),
            Content::Opaque { .. } => "opaque",
        })
    }

    /// Bytes of an entry exactly as stored in the archive (compressed).
    pub fn entry_stored_bytes(&self, key: &str) -> Option<&[u8]> {
        let e = &self.entries[self.index(key)?];
        Some(&self.orig[e.lfh_end..e.data_end])
    }

    /// Decompressed bytes of an entry (`None` for opaque entries).
    pub fn entry_data(&self, key: &str) -> Option<&[u8]> {
        match &self.entries[self.index(key)?].content {
            Content::Entry { entry, .. } => Some(entry.original()),
            Content::Opaque { .. } => None,
        }
    }

    /// True when re-compressing the entry reproduces the original encoder.
    pub fn entry_exact_recompression(&self, key: &str) -> Option<bool> {
        match &self.entries[self.index(key)?].content {
            Content::Entry { method: Method::Deflate { exact, .. }, .. } => Some(*exact),
            Content::Entry { method: Method::Stored, .. } => Some(true),
            Content::Opaque { .. } => None,
        }
    }

    /// Rebuilds the archive.
    pub fn serialize(&self) -> CodecResult<Vec<u8>> {
        let d = &self.orig;
        let mut out = Vec::with_capacity(d.len() + 1024);
        out.extend_from_slice(&d[..self.head_end]);
        let mut offsets = vec![0u64; self.entries.len()];
        let mut payloads: Vec<Option<Payload>> = (0..self.entries.len()).map(|_| None).collect();
        let patch = |buf: &mut [u8], e: &ZipEntry, part: Part, p: &Payload| -> CodecResult<()> {
            for f in e.fields.iter().filter(|f| f.part == part) {
                let v = match f.what {
                    What::Crc => u64::from(p.crc),
                    What::Compressed => p.csize,
                    What::Uncompressed => p.usize,
                    What::Offset => continue,
                };
                put(buf, f, v, &e.name)?;
            }
            Ok(())
        };
        for &i in &self.file_order {
            let e = &self.entries[i];
            offsets[i] = out.len() as u64;
            let payload = e.payload()?;
            let mut lfh = d[e.lho..e.lfh_end].to_vec();
            let mut desc = d[e.data_end..e.desc_end].to_vec();
            match &payload {
                Some(p) => {
                    patch(&mut lfh, e, Part::Local, p)?;
                    patch(&mut desc, e, Part::Descriptor, p)?;
                    out.extend_from_slice(&lfh);
                    out.extend_from_slice(&p.data);
                }
                None => {
                    out.extend_from_slice(&lfh);
                    out.extend_from_slice(&d[e.lfh_end..e.data_end]);
                }
            }
            out.extend_from_slice(&desc);
            out.extend_from_slice(&d[e.desc_end..e.gap_end]);
            payloads[i] = payload;
        }
        let new_cd_start = out.len() as u64;
        for (i, e) in self.entries.iter().enumerate() {
            let mut rec = d[e.cd_start..e.cd_end].to_vec();
            if let Some(p) = &payloads[i] {
                patch(&mut rec, e, Part::Central, p)?;
            }
            for f in e.fields.iter().filter(|f| f.part == Part::Central && f.what == What::Offset) {
                put(&mut rec, f, offsets[i], &e.name)?;
            }
            out.extend_from_slice(&rec);
        }
        let mut tail = d[self.cd_end..].to_vec();
        let shift = new_cd_start as i128 - self.cd_start as i128;
        for tf in &self.tail_fields {
            let v = match tf.kind {
                TailKind::CdOffset => new_cd_start,
                TailKind::Shifted(orig) => u64::try_from(orig as i128 + shift)
                    .map_err(|_| CodecError::malformed(CODEC, tf.pos, "ZIP64 record offset underflow"))?,
            };
            let f = Field { part: Part::Central, what: What::Offset, pos: tf.pos - self.cd_end, width: tf.width };
            put(&mut tail, &f, v, "end of central directory")?;
        }
        out.extend_from_slice(&tail);
        Ok(out)
    }
}

impl Document for ZipDocument {
    fn codec_id(&self) -> &'static str {
        CODEC
    }

    fn to_ast(&self) -> Node {
        let mut map = NodeMap::new();
        for (k, e) in self.keys.iter().zip(&self.entries) {
            let n = match &e.content {
                Content::Entry { entry, .. } => entry.to_ast(),
                Content::Opaque { .. } => Node::Bytes(self.orig[e.lfh_end..e.data_end].to_vec()),
            };
            map.insert(k.clone(), n);
        }
        Node::Map(map)
    }

    fn apply_ast(&mut self, root: &Node) -> CodecResult<()> {
        let Node::Map(map) = root else {
            return Err(CodecError::type_mismatch(CODEC, NodePath::root(), "map", root.kind().name()));
        };
        let same_keys = map.len() == self.keys.len() && map.keys().zip(&self.keys).all(|(a, b)| a == b);
        if !same_keys {
            let path = map
                .keys()
                .find(|k| !self.keys.contains(k))
                .or_else(|| self.keys.iter().find(|k| !map.contains_key(*k)))
                .map(|k| NodePath::root().key(k.clone()))
                .unwrap_or_else(NodePath::root);
            return Err(CodecError::structure(
                CODEC,
                path,
                "adding, removing, renaming or reordering ZIP entries is not supported",
            ));
        }
        let mut work = self.entries.clone();
        for (e, (key, node)) in work.iter_mut().zip(map.iter()) {
            match &mut e.content {
                Content::Entry { entry, .. } => entry.apply_ast(node)?,
                Content::Opaque { reason } => {
                    if !bit_equal(&Node::Bytes(self.orig[e.lfh_end..e.data_end].to_vec()), node) {
                        return Err(CodecError::unsupported(
                            CODEC,
                            format!("entry `{key}` is {reason}; it can only be kept as is"),
                        ));
                    }
                }
            }
        }
        self.entries = work;
        Ok(())
    }

    fn encode(&self) -> CodecResult<Vec<u8>> {
        self.serialize()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::VALUES_ONLY
    }

    fn notes(&self) -> Vec<String> {
        let mut head = format!("ZIP archive: {} entries", self.entries.len());
        if self.zip64 {
            head.push_str(", ZIP64");
        }
        if self.comment_len > 0 {
            head.push_str(&format!(", {}-byte archive comment", self.comment_len));
        }
        let mut notes = vec![head];
        for (k, e) in self.keys.iter().zip(&self.entries) {
            match &e.content {
                Content::Entry { entry, method } => {
                    let how = match method {
                        Method::Stored => "stored".to_string(),
                        Method::Deflate { level, exact: true } => format!("deflate level {level}"),
                        Method::Deflate { exact: false, .. } => "deflate".to_string(),
                    };
                    notes.push(format!("{} [{how}]", entry.describe()));
                    if matches!(method, Method::Deflate { exact: false, .. }) {
                        notes.push(format!(
                            "{k}: no deflate level reproduces the original stream; edits re-compress at level 6 (valid but not bit-identical to the original encoder)"
                        ));
                    }
                }
                Content::Opaque { reason } => notes.push(format!("{k}: opaque, {reason}")),
            }
            if e.descriptor_missing {
                notes.push(format!("{k}: data descriptor flag set but no matching descriptor found (kept verbatim)"));
            }
        }
        notes
    }

    fn clone_box(&self) -> Box<dyn Document> {
        Box::new(self.clone())
    }
}

/// ZIP archive codec.
#[derive(Debug, Clone, Copy, Default)]
pub struct ZipCodec;

impl StructuralCodec for ZipCodec {
    fn id(&self) -> &'static str {
        CODEC
    }

    fn name(&self) -> &'static str {
        "ZIP archive"
    }

    fn detect(&self, bytes: &[u8], _ctx: &DetectContext) -> Confidence {
        if bytes.len() >= 30 && u32_at(bytes, 0) == Some(SIG_LOCAL) && locate(bytes).is_ok() {
            95
        } else {
            0
        }
    }

    fn decode(&self, bytes: &[u8], ctx: &DetectContext) -> CodecResult<Box<dyn Document>> {
        Ok(Box::new(ZipDocument::parse(bytes, ctx)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::EnvelopePipeline;
    use savelens_core::RoundTripStatus;

    /// How a test entry is written.
    #[derive(Clone)]
    struct Spec {
        name: &'static str,
        data: Vec<u8>,
        /// `None` = stored, `Some(level)` = deflate.
        level: Option<u32>,
        /// `Some(with_signature)` = streamed with a data descriptor.
        descriptor: Option<bool>,
        zip64: bool,
        flags: u16,
        method: Option<u16>,
        /// Pre-compressed body overriding `level`.
        body: Option<Vec<u8>>,
    }

    fn spec(name: &'static str, data: &[u8]) -> Spec {
        Spec {
            name,
            data: data.to_vec(),
            level: None,
            descriptor: None,
            zip64: false,
            flags: 0,
            method: None,
            body: None,
        }
    }

    /// Test-side ZIP writer.
    fn build(specs: &[Spec], comment: &[u8], gap: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cd = Vec::new();
        let mut any64 = false;
        for s in specs {
            let body = match (&s.body, s.level) {
                (Some(b), _) => b.clone(),
                (None, Some(l)) => deflate(&s.data, l, false).unwrap(),
                (None, None) => s.data.clone(),
            };
            let method = s.method.unwrap_or(if s.level.is_some() || s.body.is_some() { 8 } else { 0 });
            let crc = crc32fast::hash(&s.data);
            let flags = s.flags | if s.descriptor.is_some() { 8 } else { 0 };
            let off = out.len() as u64;
            any64 |= s.zip64;
            let version: u16 = if s.zip64 { 45 } else { 20 };
            // Local header.
            out.extend(SIG_LOCAL.to_le_bytes());
            out.extend(version.to_le_bytes());
            out.extend(flags.to_le_bytes());
            out.extend(method.to_le_bytes());
            out.extend([0x00, 0x60, 0x21, 0x50]);
            let streamed = s.descriptor.is_some();
            let (lcrc, lc, lu) = if streamed {
                (0, 0, 0)
            } else if s.zip64 {
                (crc, MASK32, MASK32)
            } else {
                (crc, body.len() as u32, s.data.len() as u32)
            };
            out.extend(lcrc.to_le_bytes());
            out.extend(lc.to_le_bytes());
            out.extend(lu.to_le_bytes());
            out.extend((s.name.len() as u16).to_le_bytes());
            let lextra: Vec<u8> = if s.zip64 {
                let mut x = vec![1, 0, 16, 0];
                x.extend((s.data.len() as u64).to_le_bytes());
                x.extend((body.len() as u64).to_le_bytes());
                x
            } else {
                vec![0xca, 0xfe, 2, 0, 0xab, 0xcd]
            };
            out.extend((lextra.len() as u16).to_le_bytes());
            out.extend(s.name.as_bytes());
            out.extend(&lextra);
            out.extend(&body);
            if let Some(sig) = s.descriptor {
                if sig {
                    out.extend(SIG_DESCRIPTOR.to_le_bytes());
                }
                out.extend(crc.to_le_bytes());
                if s.zip64 {
                    out.extend((body.len() as u64).to_le_bytes());
                    out.extend((s.data.len() as u64).to_le_bytes());
                } else {
                    out.extend((body.len() as u32).to_le_bytes());
                    out.extend((s.data.len() as u32).to_le_bytes());
                }
            }
            out.extend(gap);
            // Central record.
            cd.extend(SIG_CENTRAL.to_le_bytes());
            cd.extend(0x031eu16.to_le_bytes());
            cd.extend(version.to_le_bytes());
            cd.extend(flags.to_le_bytes());
            cd.extend(method.to_le_bytes());
            cd.extend([0x00, 0x60, 0x21, 0x50]);
            cd.extend(crc.to_le_bytes());
            let cextra: Vec<u8> = if s.zip64 {
                cd.extend(MASK32.to_le_bytes());
                cd.extend(MASK32.to_le_bytes());
                let mut x = vec![1, 0, 24, 0];
                x.extend((s.data.len() as u64).to_le_bytes());
                x.extend((body.len() as u64).to_le_bytes());
                x.extend(off.to_le_bytes());
                x
            } else {
                cd.extend((body.len() as u32).to_le_bytes());
                cd.extend((s.data.len() as u32).to_le_bytes());
                Vec::new()
            };
            cd.extend((s.name.len() as u16).to_le_bytes());
            cd.extend((cextra.len() as u16).to_le_bytes());
            cd.extend(3u16.to_le_bytes());
            cd.extend([0, 0, 0, 0]);
            cd.extend(0x81a4_0000u32.to_le_bytes());
            cd.extend((if s.zip64 { MASK32 } else { off as u32 }).to_le_bytes());
            cd.extend(s.name.as_bytes());
            cd.extend(&cextra);
            cd.extend(b"c!!");
        }
        let cd_off = out.len() as u64;
        out.extend(&cd);
        let n = specs.len() as u64;
        if any64 {
            let rec = out.len() as u64;
            out.extend(SIG_EOCD64.to_le_bytes());
            out.extend(44u64.to_le_bytes());
            out.extend(45u16.to_le_bytes());
            out.extend(45u16.to_le_bytes());
            out.extend(0u32.to_le_bytes());
            out.extend(0u32.to_le_bytes());
            out.extend(n.to_le_bytes());
            out.extend(n.to_le_bytes());
            out.extend((cd.len() as u64).to_le_bytes());
            out.extend(cd_off.to_le_bytes());
            out.extend(SIG_LOCATOR.to_le_bytes());
            out.extend(0u32.to_le_bytes());
            out.extend(rec.to_le_bytes());
            out.extend(1u32.to_le_bytes());
        }
        out.extend(SIG_EOCD.to_le_bytes());
        out.extend([0, 0, 0, 0]);
        out.extend((n as u16).to_le_bytes());
        out.extend((n as u16).to_le_bytes());
        out.extend((cd.len() as u32).to_le_bytes());
        out.extend((if any64 { MASK32 } else { cd_off as u32 }).to_le_bytes());
        out.extend((comment.len() as u16).to_le_bytes());
        out.extend(comment);
        out
    }

    fn noise(n: usize) -> Vec<u8> {
        let mut x: u32 = 0x1234_5678;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    fn sample() -> Vec<u8> {
        let mut save = spec("save.json", br#"{"gold":100,"name":"Hero","flags":[1,2,3]}"#);
        save.level = Some(9);
        let shot = spec("screenshot.png", &noise(300));
        let mut stream = spec("stream.json", br#"{"hp":50}"#);
        stream.level = Some(6);
        stream.descriptor = Some(true);
        let mut big = spec("big.json", br#"{"xp":7}"#);
        big.zip64 = true;
        let mut nosig = spec("nosig.json", br#"{"mp":3}"#);
        nosig.level = Some(1);
        nosig.descriptor = Some(false);
        let dir = spec("dir/", b"");
        build(&[save, shot, stream, big, nosig, dir], b"archive comment", b"")
    }

    fn ctx() -> DetectContext {
        DetectContext::for_file("1-1-LT1.save")
    }

    fn get(doc: &ZipDocument, path: &str) -> Option<Node> {
        doc.to_ast().get(&NodePath::parse(path).unwrap()).cloned()
    }

    fn edited(doc: &ZipDocument, path: &str, v: Node) -> (ZipDocument, Vec<u8>) {
        let mut d = doc.clone();
        let mut root = d.to_ast();
        root.set(&NodePath::parse(path).unwrap(), v, false).unwrap();
        d.apply_ast(&root).unwrap();
        let out = d.encode().unwrap();
        let re = ZipDocument::parse(&out, &ctx()).unwrap();
        assert!(bit_equal(&re.to_ast(), &d.to_ast()));
        assert!(re.notes().iter().all(|n| !n.contains("no matching descriptor")), "{:?}", re.notes());
        (re, out)
    }

    #[test]
    fn round_trip_and_projection() {
        let data = sample();
        assert_eq!(ZipCodec.detect(&data, &ctx()), 95);
        let doc = ZipDocument::parse(&data, &ctx()).unwrap();
        assert_eq!(doc.encode().unwrap(), data);
        assert_eq!(
            doc.entry_names(),
            ["save.json", "screenshot.png", "stream.json", "big.json", "nosig.json", "dir/"].map(String::from)
        );
        assert_eq!(doc.entry_codec("save.json"), Some("json"));
        assert_eq!(doc.entry_exact_recompression("save.json"), Some(true));
        assert_eq!(doc.entry_codec("big.json"), Some("json"));
        assert_eq!(get(&doc, "[\"save.json\"].gold"), Some(Node::I64(100)));
        assert_eq!(get(&doc, "[\"stream.json\"].hp"), Some(Node::I64(50)));
        assert_eq!(get(&doc, "[\"big.json\"].xp"), Some(Node::I64(7)));
        let notes = doc.notes();
        assert!(notes[0].contains("ZIP64") && notes[0].contains("15-byte archive comment"), "{notes:?}");
        // Through the pipeline.
        let s = EnvelopePipeline::builtin().open(&data, &ctx()).unwrap();
        assert_eq!(s.codec_id(), "zip");
        assert_eq!(s.envelope().round_trip, Some(RoundTripStatus::Lossless));
    }

    #[test]
    fn edits_update_crc_sizes_and_offsets() {
        let data = sample();
        let doc = ZipDocument::parse(&data, &ctx()).unwrap();
        let (re, out) = edited(&doc, "[\"save.json\"].gold", Node::I64(123_456_789));
        assert_eq!(get(&re, "[\"save.json\"].gold"), Some(Node::I64(123_456_789)));
        assert_ne!(out.len(), data.len());
        for k in ["screenshot.png", "stream.json", "big.json", "nosig.json", "dir/"] {
            assert_eq!(re.entry_stored_bytes(k), doc.entry_stored_bytes(k), "{k} untouched");
        }
        assert!(out.ends_with(b"archive comment"));
        // Streamed entry with signed descriptor, ZIP64 entry, unsigned descriptor.
        let (re, _) = edited(&doc, "[\"stream.json\"].hp", Node::I64(99_999));
        assert_eq!(get(&re, "[\"stream.json\"].hp"), Some(Node::I64(99_999)));
        let (re, _) = edited(&doc, "[\"big.json\"].xp", Node::I64(1_000_000));
        assert_eq!(get(&re, "[\"big.json\"].xp"), Some(Node::I64(1_000_000)));
        let (re, out) = edited(&doc, "[\"nosig.json\"].mp", Node::I64(42));
        assert_eq!(get(&re, "[\"nosig.json\"].mp"), Some(Node::I64(42)));
        // Streamed local headers keep their zeroed CRC / size fields.
        let lho = re.entries[4].lho;
        assert_eq!(&out[lho + 14..lho + 26], &[0u8; 12]);
        // Raw region edit of the stored screenshot.
        let mut d = doc.clone();
        let mut root = d.to_ast();
        let shot = NodePath::parse("[\"screenshot.png\"]").unwrap();
        let mut bytes = noise(300);
        bytes[0] ^= 0xff;
        let replacement = match root.get(&shot).unwrap() {
            Node::RawRegion(r) => {
                let mut r = r.clone();
                r.bytes = bytes.clone();
                Node::RawRegion(r)
            }
            _ => Node::Bytes(bytes.clone()),
        };
        root.set(&shot, replacement, false).unwrap();
        d.apply_ast(&root).unwrap();
        let re = ZipDocument::parse(&d.encode().unwrap(), &ctx()).unwrap();
        assert_eq!(re.entry_data("screenshot.png"), Some(bytes.as_slice()));
    }

    #[test]
    fn structural_edits_and_opaque_entries_are_rejected() {
        let mut enc = spec("secret.bin", b"ciphertext");
        enc.flags = 1;
        let mut bz = spec("data.bz2", b"BZh9...");
        bz.method = Some(12);
        let plain = spec("a.json", br#"{"a":1}"#);
        let data = build(&[plain, enc, bz], b"", b"GAP");
        let doc = ZipDocument::parse(&data, &ctx()).unwrap();
        assert_eq!(doc.encode().unwrap(), data, "gaps between entries are kept");
        assert_eq!(doc.entry_codec("secret.bin"), Some("opaque"));
        assert_eq!(doc.entry_codec("data.bz2"), Some("opaque"));
        assert_eq!(get(&doc, "[\"secret.bin\"]"), Some(Node::Bytes(b"ciphertext".to_vec())));
        let mut d = doc.clone();
        let mut root = d.to_ast();
        root.set(&NodePath::parse("[\"secret.bin\"]").unwrap(), Node::Bytes(b"x".to_vec()), false).unwrap();
        assert!(matches!(d.apply_ast(&root), Err(CodecError::Unsupported { .. })));
        let mut root = doc.to_ast();
        root.set(&NodePath::parse("[\"new.txt\"]").unwrap(), Node::Bytes(vec![1]), true).unwrap();
        assert!(matches!(d.apply_ast(&root), Err(CodecError::StructureUnsupported { .. })));
        let mut root = doc.to_ast();
        root.remove(&NodePath::parse("[\"a.json\"]").unwrap()).unwrap();
        assert!(matches!(d.apply_ast(&root), Err(CodecError::StructureUnsupported { .. })));
        // Edit the gap-separated JSON entry: gaps survive, offsets move.
        let (re, out) = edited(&doc, "[\"a.json\"].a", Node::I64(12345));
        assert_eq!(get(&re, "[\"a.json\"].a"), Some(Node::I64(12345)));
        assert_eq!(out.windows(3).filter(|w| *w == b"GAP").count(), 3);
    }

    #[test]
    fn non_reproducible_deflate_is_editable_with_a_note() {
        // Two hand-made stored deflate blocks: no flate2 level emits this.
        let json = br#"{"coins":5,"name":"x"}"#;
        let mut body = Vec::new();
        for (part, last) in [(&json[..5], 0u8), (&json[5..], 1u8)] {
            body.push(last);
            body.extend((part.len() as u16).to_le_bytes());
            body.extend((!(part.len() as u16)).to_le_bytes());
            body.extend(part);
        }
        let mut s = spec("save.json", json);
        s.body = Some(body);
        let data = build(&[s], b"", b"");
        let doc = ZipDocument::parse(&data, &ctx()).unwrap();
        assert_eq!(doc.encode().unwrap(), data);
        assert_eq!(doc.entry_exact_recompression("save.json"), Some(false));
        assert!(doc.notes().iter().any(|n| n.contains("not bit-identical")));
        let (re, _) = edited(&doc, "[\"save.json\"].coins", Node::I64(500));
        assert_eq!(get(&re, "[\"save.json\"].coins"), Some(Node::I64(500)));
    }

    #[test]
    fn detection() {
        let ctx = ctx();
        assert_eq!(ZipCodec.detect(b"not a zip at all, definitely not", &ctx), 0);
        let mut broken = sample();
        let n = broken.len();
        broken.truncate(n - 30);
        assert_eq!(ZipCodec.detect(&broken, &ctx), 0);
        let mut bad_count = build(&[spec("a", b"1")], b"", b"");
        let n = bad_count.len();
        bad_count[n - 12] = 5;
        assert_eq!(ZipCodec.detect(&bad_count, &ctx), 0);
        assert!(ZipDocument::parse(&bad_count, &ctx).is_err());
    }
}
