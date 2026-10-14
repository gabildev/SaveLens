//! Multi-layer compression wrapper: Zlib, Gzip, raw Deflate, LZ4 (frame and
//! size-prefixed block) and Zstd, optionally preceded by a 4/8-byte
//! uncompressed-size prefix.
//!
//! On open the layer decompresses the stream and searches for the encoder
//! parameters (level, flags) that reproduce the original bytes. Unmodified
//! payloads are always re-emitted verbatim; modified payloads are compressed
//! with the recovered parameters (bit-exact with the original encoder when the
//! search succeeded, otherwise a valid stream of the same format).

use std::io::{Read, Write};

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use savelens_core::{CompressionAlgorithm, LayerDescriptor, SizePrefix};

use super::{Framing, Identity, Layer};
use crate::error::{CodecError, CodecResult};

/// Refuse to inflate beyond this size (zip-bomb guard).
pub const MAX_DECOMPRESSED: usize = 1 << 30;
/// Exact-parameter search is skipped for payloads larger than this.
const SEARCH_LIMIT: usize = 64 * 1024 * 1024;

fn comp_err(algo: CompressionAlgorithm, reason: impl Into<String>) -> CodecError {
    CodecError::Compression {
        algorithm: algo.name().to_string(),
        reason: reason.into(),
    }
}

/// Inflates a zlib (`zlib_header = true`) or raw deflate stream; returns the
/// output and the number of input bytes consumed.
pub fn inflate(data: &[u8], zlib_header: bool) -> CodecResult<(Vec<u8>, usize)> {
    let algo = if zlib_header { CompressionAlgorithm::Zlib } else { CompressionAlgorithm::Deflate };
    let mut d = Decompress::new(zlib_header);
    let mut out: Vec<u8> = Vec::with_capacity((data.len() * 4).min(MAX_DECOMPRESSED).max(64));
    loop {
        let in_before = d.total_in() as usize;
        let out_before = d.total_out();
        if out.len() == out.capacity() {
            if out.len() >= MAX_DECOMPRESSED {
                return Err(comp_err(algo, "decompressed size exceeds the 1 GiB safety limit"));
            }
            out.reserve(out.len().max(4096));
        }
        let status = d
            .decompress_vec(&data[in_before..], &mut out, FlushDecompress::None)
            .map_err(|e| comp_err(algo, e.to_string()))?;
        match status {
            Status::StreamEnd => return Ok((out, d.total_in() as usize)),
            Status::Ok | Status::BufError => {
                let progressed = d.total_in() as usize != in_before || d.total_out() != out_before;
                if !progressed && out.len() < out.capacity() {
                    return Err(comp_err(algo, "truncated stream"));
                }
            }
        }
    }
}

/// Deflates with flate2 at `level`, with or without the zlib wrapper.
pub fn deflate(data: &[u8], level: u32, zlib_header: bool) -> CodecResult<Vec<u8>> {
    let algo = if zlib_header { CompressionAlgorithm::Zlib } else { CompressionAlgorithm::Deflate };
    let mut c = Compress::new(Compression::new(level), zlib_header);
    let mut out = Vec::with_capacity(data.len() / 2 + 64);
    loop {
        if out.len() == out.capacity() {
            out.reserve(out.len().max(4096));
        }
        let consumed = c.total_in() as usize;
        let status = c
            .compress_vec(&data[consumed..], &mut out, FlushCompress::Finish)
            .map_err(|e| comp_err(algo, e.to_string()))?;
        if status == Status::StreamEnd {
            return Ok(out);
        }
    }
}

fn read_prefix(data: &[u8], p: SizePrefix) -> Option<u64> {
    Some(match p {
        SizePrefix::U32Le => u32::from_le_bytes(data.get(..4)?.try_into().ok()?) as u64,
        SizePrefix::U32Be => u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as u64,
        SizePrefix::U64Le => u64::from_le_bytes(data.get(..8)?.try_into().ok()?),
        SizePrefix::U64Be => u64::from_be_bytes(data.get(..8)?.try_into().ok()?),
    })
}

fn write_prefix(p: SizePrefix, v: usize) -> CodecResult<Vec<u8>> {
    let too_big = || CodecError::Compression {
        algorithm: "size prefix".into(),
        reason: format!("payload of {v} bytes does not fit a {p:?} prefix"),
    };
    Ok(match p {
        SizePrefix::U32Le => u32::try_from(v).map_err(|_| too_big())?.to_le_bytes().to_vec(),
        SizePrefix::U32Be => u32::try_from(v).map_err(|_| too_big())?.to_be_bytes().to_vec(),
        SizePrefix::U64Le => (v as u64).to_le_bytes().to_vec(),
        SizePrefix::U64Be => (v as u64).to_be_bytes().to_vec(),
    })
}

/// Parameters needed to re-encode a stream.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamParams {
    Zlib { level: u32 },
    Deflate { level: u32 },
    Gzip { header: Vec<u8>, level: u32 },
    Zstd { level: i32, checksum: bool, content_size: bool },
    Lz4Frame { block_size: lz4_flex::frame::BlockSize, independent: bool, block_checksums: bool, content_checksum: bool, content_size: bool },
    Lz4Block,
}

impl StreamParams {
    fn algorithm(&self) -> CompressionAlgorithm {
        match self {
            StreamParams::Zlib { .. } => CompressionAlgorithm::Zlib,
            StreamParams::Deflate { .. } => CompressionAlgorithm::Deflate,
            StreamParams::Gzip { .. } => CompressionAlgorithm::Gzip,
            StreamParams::Zstd { .. } => CompressionAlgorithm::Zstd,
            StreamParams::Lz4Frame { .. } => CompressionAlgorithm::Lz4Frame,
            StreamParams::Lz4Block => CompressionAlgorithm::Lz4Block,
        }
    }

    fn level(&self) -> Option<i32> {
        match self {
            StreamParams::Zlib { level } | StreamParams::Deflate { level } | StreamParams::Gzip { level, .. } => Some(*level as i32),
            StreamParams::Zstd { level, .. } => Some(*level),
            StreamParams::Lz4Frame { .. } | StreamParams::Lz4Block => None,
        }
    }

    /// Compresses `data` with these parameters.
    pub fn compress(&self, data: &[u8]) -> CodecResult<Vec<u8>> {
        match self {
            StreamParams::Zlib { level } => deflate(data, *level, true),
            StreamParams::Deflate { level } => deflate(data, *level, false),
            StreamParams::Gzip { header, level } => {
                let mut out = header.clone();
                out.extend_from_slice(&deflate(data, *level, false)?);
                out.extend_from_slice(&crc32fast::hash(data).to_le_bytes());
                out.extend_from_slice(&(data.len() as u32).to_le_bytes());
                Ok(out)
            }
            StreamParams::Zstd {
                level,
                checksum,
                content_size,
            } => {
                use zstd::stream::raw::CParameter;
                let algo = CompressionAlgorithm::Zstd;
                let mut c = zstd::bulk::Compressor::new(*level).map_err(|e| comp_err(algo, e.to_string()))?;
                c.set_parameter(CParameter::ChecksumFlag(*checksum))
                    .map_err(|e| comp_err(algo, e.to_string()))?;
                c.set_parameter(CParameter::ContentSizeFlag(*content_size))
                    .map_err(|e| comp_err(algo, e.to_string()))?;
                c.compress(data).map_err(|e| comp_err(algo, e.to_string()))
            }
            StreamParams::Lz4Frame {
                block_size,
                independent,
                block_checksums,
                content_checksum,
                content_size,
            } => {
                use lz4_flex::frame::{BlockMode, FrameEncoder, FrameInfo};
                let info = FrameInfo::new()
                    .block_size(*block_size)
                    .block_mode(if *independent { BlockMode::Independent } else { BlockMode::Linked })
                    .block_checksums(*block_checksums)
                    .content_checksum(*content_checksum)
                    .content_size(content_size.then_some(data.len() as u64));
                let mut enc = FrameEncoder::with_frame_info(info, Vec::new());
                enc.write_all(data)
                    .map_err(|e| comp_err(CompressionAlgorithm::Lz4Frame, e.to_string()))?;
                enc.finish()
                    .map_err(|e| comp_err(CompressionAlgorithm::Lz4Frame, e.to_string()))
            }
            StreamParams::Lz4Block => Ok(lz4_flex::block::compress(data)),
        }
    }
}

/// Parses a gzip member header; returns its length.
fn gzip_header_len(data: &[u8]) -> CodecResult<usize> {
    let algo = CompressionAlgorithm::Gzip;
    if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b || data[2] != 8 {
        return Err(comp_err(algo, "not a gzip member"));
    }
    let flg = data[3];
    if flg & 0xe0 != 0 {
        return Err(comp_err(algo, "reserved gzip flags set"));
    }
    let mut pos = 10;
    if flg & 0x04 != 0 {
        let xlen = u16::from_le_bytes([*data.get(pos).ok_or_else(|| comp_err(algo, "truncated FEXTRA"))?, *data.get(pos + 1).ok_or_else(|| comp_err(algo, "truncated FEXTRA"))?]) as usize;
        pos += 2 + xlen;
    }
    for flag in [0x08u8, 0x10] {
        if flg & flag != 0 {
            let end = data
                .get(pos..)
                .and_then(|s| s.iter().position(|&b| b == 0))
                .ok_or_else(|| comp_err(algo, "unterminated FNAME/FCOMMENT"))?;
            pos += end + 1;
        }
    }
    if flg & 0x02 != 0 {
        pos += 2;
    }
    if pos > data.len() {
        return Err(comp_err(algo, "truncated header"));
    }
    Ok(pos)
}

/// Total length of an LZ4 frame starting at `data[0]`.
fn lz4_frame_len(data: &[u8]) -> CodecResult<(usize, StreamParams)> {
    use lz4_flex::frame::BlockSize;
    let algo = CompressionAlgorithm::Lz4Frame;
    let err = |r: &str| comp_err(algo, r);
    if data.len() < 7 || data[..4] != [0x04, 0x22, 0x4d, 0x18] {
        return Err(err("missing LZ4 frame magic"));
    }
    let flg = data[4];
    let bd = data[5];
    if flg >> 6 != 1 {
        return Err(err("unsupported LZ4 frame version"));
    }
    let independent = flg & 0x20 != 0;
    let block_checksums = flg & 0x10 != 0;
    let has_content_size = flg & 0x08 != 0;
    let content_checksum = flg & 0x04 != 0;
    let has_dict = flg & 0x01 != 0;
    let block_size = match (bd >> 4) & 0x7 {
        4 => BlockSize::Max64KB,
        5 => BlockSize::Max256KB,
        6 => BlockSize::Max1MB,
        7 => BlockSize::Max4MB,
        _ => return Err(err("invalid block size id")),
    };
    let mut pos = 6 + if has_content_size { 8 } else { 0 } + if has_dict { 4 } else { 0 } + 1;
    loop {
        let sz = data.get(pos..pos + 4).ok_or_else(|| err("truncated block header"))?;
        let raw = u32::from_le_bytes([sz[0], sz[1], sz[2], sz[3]]);
        pos += 4;
        if raw == 0 {
            break;
        }
        pos += (raw & 0x7fff_ffff) as usize + if block_checksums { 4 } else { 0 };
        if pos > data.len() {
            return Err(err("truncated block"));
        }
    }
    if content_checksum {
        pos += 4;
    }
    if pos > data.len() {
        return Err(err("truncated content checksum"));
    }
    Ok((
        pos,
        StreamParams::Lz4Frame {
            block_size,
            independent,
            block_checksums,
            content_checksum,
            content_size: has_content_size,
        },
    ))
}

/// A successfully decoded stream.
#[derive(Debug, Clone)]
pub struct DecodedStream {
    pub params: StreamParams,
    pub output: Vec<u8>,
    /// Length of the compressed stream inside the input.
    pub consumed: usize,
}

/// Decodes a stream of a given algorithm starting at `data[0]`.
pub fn decode_stream(algo: CompressionAlgorithm, data: &[u8], expected_len: Option<usize>) -> CodecResult<DecodedStream> {
    match algo {
        CompressionAlgorithm::Zlib => {
            if data.len() < 6 || data[0] & 0x0f != 8 || (u16::from(data[0]) << 8 | u16::from(data[1])) % 31 != 0 {
                return Err(comp_err(algo, "invalid zlib header"));
            }
            let (output, consumed) = inflate(data, true)?;
            Ok(DecodedStream {
                params: StreamParams::Zlib { level: 6 },
                output,
                consumed,
            })
        }
        CompressionAlgorithm::Deflate => {
            let (output, consumed) = inflate(data, false)?;
            Ok(DecodedStream {
                params: StreamParams::Deflate { level: 6 },
                output,
                consumed,
            })
        }
        CompressionAlgorithm::Gzip => {
            let hlen = gzip_header_len(data)?;
            let (output, used) = inflate(&data[hlen..], false)?;
            let tpos = hlen + used;
            let trailer = data.get(tpos..tpos + 8).ok_or_else(|| comp_err(algo, "missing gzip trailer"))?;
            let crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
            let isize = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
            if crc != crc32fast::hash(&output) || isize != output.len() as u32 {
                return Err(comp_err(algo, "gzip trailer CRC/size mismatch"));
            }
            Ok(DecodedStream {
                params: StreamParams::Gzip {
                    header: data[..hlen].to_vec(),
                    level: 6,
                },
                output,
                consumed: tpos + 8,
            })
        }
        CompressionAlgorithm::Zstd => {
            if data.len() < 9 || data[..4] != [0x28, 0xb5, 0x2f, 0xfd] {
                return Err(comp_err(algo, "missing zstd magic"));
            }
            let consumed = zstd::zstd_safe::find_frame_compressed_size(data)
                .map_err(|code| comp_err(algo, zstd::zstd_safe::get_error_name(code)))?;
            let mut dec = zstd::stream::read::Decoder::new(&data[..consumed]).map_err(|e| comp_err(algo, e.to_string()))?;
            let mut output = Vec::new();
            (&mut dec)
                .take(MAX_DECOMPRESSED as u64 + 1)
                .read_to_end(&mut output)
                .map_err(|e| comp_err(algo, e.to_string()))?;
            if output.len() > MAX_DECOMPRESSED {
                return Err(comp_err(algo, "decompressed size exceeds the 1 GiB safety limit"));
            }
            let fhd = data[4];
            let checksum = fhd & 0x04 != 0;
            let content_size = (fhd >> 6) != 0 || fhd & 0x20 != 0;
            Ok(DecodedStream {
                params: StreamParams::Zstd {
                    level: 3,
                    checksum,
                    content_size,
                },
                output,
                consumed,
            })
        }
        CompressionAlgorithm::Lz4Frame => {
            let (len, params) = lz4_frame_len(data)?;
            let mut dec = lz4_flex::frame::FrameDecoder::new(&data[..len]);
            let mut output = Vec::new();
            (&mut dec)
                .take(MAX_DECOMPRESSED as u64 + 1)
                .read_to_end(&mut output)
                .map_err(|e| comp_err(algo, e.to_string()))?;
            if output.len() > MAX_DECOMPRESSED {
                return Err(comp_err(algo, "decompressed size exceeds the 1 GiB safety limit"));
            }
            Ok(DecodedStream {
                params,
                output,
                consumed: len,
            })
        }
        CompressionAlgorithm::Lz4Block => {
            let size = expected_len.ok_or_else(|| comp_err(algo, "LZ4 block streams need an uncompressed-size prefix"))?;
            if size > MAX_DECOMPRESSED {
                return Err(comp_err(algo, "declared size exceeds the 1 GiB safety limit"));
            }
            let output = lz4_flex::block::decompress(data, size).map_err(|e| comp_err(algo, e.to_string()))?;
            if output.len() != size {
                return Err(comp_err(algo, "decompressed size differs from prefix"));
            }
            Ok(DecodedStream {
                params: StreamParams::Lz4Block,
                output,
                consumed: data.len(),
            })
        }
    }
}

/// Tries encoder parameters until one reproduces `original` exactly.
fn search_exact(params: &StreamParams, payload: &[u8], original: &[u8]) -> Option<StreamParams> {
    if payload.len() > SEARCH_LIMIT {
        return None;
    }
    let candidates: Vec<StreamParams> = match params {
        StreamParams::Zlib { .. } => {
            // Order levels by the FLEVEL hint in the header to find the match early.
            let flevel = original.get(1).map(|b| b >> 6).unwrap_or(2);
            let mut levels: Vec<u32> = match flevel {
                0 => vec![1, 0],
                1 => vec![2, 3, 4, 5],
                2 => vec![6],
                _ => vec![9, 7, 8],
            };
            for l in 0..=9 {
                if !levels.contains(&l) {
                    levels.push(l);
                }
            }
            levels.into_iter().map(|level| StreamParams::Zlib { level }).collect()
        }
        StreamParams::Deflate { .. } => [6u32, 9, 1, 2, 3, 4, 5, 7, 8, 0]
            .into_iter()
            .map(|level| StreamParams::Deflate { level })
            .collect(),
        StreamParams::Gzip { header, .. } => {
            let xfl = header.get(8).copied().unwrap_or(0);
            let mut levels: Vec<u32> = match xfl {
                2 => vec![9, 8, 7],
                4 => vec![1],
                _ => vec![6],
            };
            for l in 0..=9 {
                if !levels.contains(&l) {
                    levels.push(l);
                }
            }
            levels
                .into_iter()
                .map(|level| StreamParams::Gzip {
                    header: header.clone(),
                    level,
                })
                .collect()
        }
        StreamParams::Zstd {
            checksum, content_size, ..
        } => {
            let mut levels: Vec<i32> = vec![3, 1, 2, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, -1, -3, -5];
            if payload.len() > 4 * 1024 * 1024 {
                levels.truncate(10);
            }
            levels
                .into_iter()
                .map(|level| StreamParams::Zstd {
                    level,
                    checksum: *checksum,
                    content_size: *content_size,
                })
                .collect()
        }
        other => vec![other.clone()],
    };
    candidates
        .into_iter()
        .find(|p| p.compress(payload).map(|c| c == original).unwrap_or(false))
}

/// The compression layer.
#[derive(Debug, Clone)]
pub struct CompressionLayer {
    pub params: StreamParams,
    pub size_prefix: Option<SizePrefix>,
    pub framing: Framing,
    pub exact: bool,
    identity: Identity,
}

const PREFIXES: [Option<SizePrefix>; 5] = [
    None,
    Some(SizePrefix::U32Le),
    Some(SizePrefix::U64Le),
    Some(SizePrefix::U32Be),
    Some(SizePrefix::U64Be),
];

fn magic_algorithm(data: &[u8]) -> Option<CompressionAlgorithm> {
    match data {
        [0x1f, 0x8b, 0x08, ..] => Some(CompressionAlgorithm::Gzip),
        [0x28, 0xb5, 0x2f, 0xfd, ..] => Some(CompressionAlgorithm::Zstd),
        [0x04, 0x22, 0x4d, 0x18, ..] => Some(CompressionAlgorithm::Lz4Frame),
        [a, b, ..] if a & 0x0f == 8 && a >> 4 <= 7 && (u16::from(*a) << 8 | u16::from(*b)) % 31 == 0 && b & 0x20 == 0 => {
            Some(CompressionAlgorithm::Zlib)
        }
        _ => None,
    }
}

impl CompressionLayer {
    /// Detects a compressed stream by magic bytes (optionally after a size
    /// prefix). Raw deflate and LZ4 block are only detected with a matching
    /// size prefix.
    pub fn detect(data: &[u8]) -> Option<(CompressionLayer, Vec<u8>)> {
        for prefix in PREFIXES {
            let skip = prefix.map(SizePrefix::width).unwrap_or(0);
            let Some(body) = data.get(skip..) else { continue };
            let declared = match prefix {
                Some(p) => {
                    let v = read_prefix(data, p)?;
                    if v == 0 || v > MAX_DECOMPRESSED as u64 {
                        continue;
                    }
                    Some(v as usize)
                }
                None => None,
            };
            let mut algos: Vec<CompressionAlgorithm> = magic_algorithm(body).into_iter().collect();
            if prefix.is_some() {
                algos.push(CompressionAlgorithm::Lz4Block);
                algos.push(CompressionAlgorithm::Deflate);
            }
            for algo in algos {
                // LZ4 block / raw deflate need a plausible prefix.
                if matches!(algo, CompressionAlgorithm::Lz4Block | CompressionAlgorithm::Deflate) {
                    let d = declared.unwrap_or(0);
                    if d < body.len() / 2 || d > body.len().saturating_mul(255) {
                        continue;
                    }
                }
                let Ok(stream) = decode_stream(algo, body, declared) else { continue };
                if let Some(d) = declared {
                    if stream.output.len() != d {
                        continue;
                    }
                }
                if let Some(layer) = Self::build(data, prefix, skip, stream) {
                    return Some(layer);
                }
            }
        }
        None
    }

    fn build(data: &[u8], prefix: Option<SizePrefix>, skip: usize, stream: DecodedStream) -> Option<(CompressionLayer, Vec<u8>)> {
        let end = skip + stream.consumed;
        let framing = Framing {
            prefix: Vec::new(),
            suffix: data.get(end..)?.to_vec(),
        };
        // Raw deflate / lz4 block with trailing garbage is too likely a false positive.
        if !framing.suffix.is_empty() && matches!(stream.params, StreamParams::Deflate { .. } | StreamParams::Lz4Block) {
            return None;
        }
        let original_stream = &data[skip..end];
        let (params, exact) = match search_exact(&stream.params, &stream.output, original_stream) {
            Some(p) => (p, true),
            None => (stream.params.clone(), false),
        };
        let layer = CompressionLayer {
            params,
            size_prefix: prefix,
            framing,
            exact,
            identity: Identity::new(data, &stream.output),
        };
        Some((layer, stream.output))
    }

    /// Builds a layer for a specific algorithm (no magic sniffing), used for
    /// raw deflate fallbacks and tests.
    pub fn open_as(algo: CompressionAlgorithm, data: &[u8], prefix: Option<SizePrefix>) -> CodecResult<(CompressionLayer, Vec<u8>)> {
        let skip = prefix.map(SizePrefix::width).unwrap_or(0);
        let body = data.get(skip..).ok_or_else(|| comp_err(algo, "input shorter than size prefix"))?;
        let declared = match prefix {
            Some(p) => Some(read_prefix(data, p).ok_or_else(|| comp_err(algo, "missing size prefix"))? as usize),
            None => None,
        };
        let stream = decode_stream(algo, body, declared)?;
        if let Some(d) = declared {
            if stream.output.len() != d {
                return Err(comp_err(algo, "size prefix does not match decompressed size"));
            }
        }
        let end = skip + stream.consumed;
        let original_stream = &data[skip..end];
        let (params, exact) = match search_exact(&stream.params, &stream.output, original_stream) {
            Some(p) => (p, true),
            None => (stream.params.clone(), false),
        };
        Ok((
            CompressionLayer {
                params,
                size_prefix: prefix,
                framing: Framing {
                    prefix: Vec::new(),
                    suffix: data[end..].to_vec(),
                },
                exact,
                identity: Identity::new(data, &stream.output),
            },
            stream.output,
        ))
    }
}

impl Layer for CompressionLayer {
    fn descriptor(&self) -> LayerDescriptor {
        LayerDescriptor::Compression {
            algorithm: self.params.algorithm(),
            size_prefix: self.size_prefix,
            level: if self.exact { self.params.level() } else { None },
            exact: self.exact,
        }
    }

    fn wrap(&self, inner: &[u8]) -> CodecResult<Vec<u8>> {
        if let Some(outer) = self.identity.lookup(inner) {
            return Ok(outer);
        }
        let mut out = match self.size_prefix {
            Some(p) => write_prefix(p, inner.len())?,
            None => Vec::new(),
        };
        out.extend_from_slice(&self.params.compress(inner)?);
        Ok(self.framing.apply(out))
    }

    fn exact_reencode(&self) -> bool {
        self.exact
    }

    fn clone_box(&self) -> Box<dyn Layer> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        let mut v = Vec::new();
        for i in 0..2000u32 {
            v.extend_from_slice(format!("{{\"slot\":{i},\"gold\":{}}},", i * 13 % 997).as_bytes());
        }
        v
    }

    fn check(original: &[u8], algo: CompressionAlgorithm, expect_exact: bool) {
        let (layer, inner) = CompressionLayer::detect(original).unwrap_or_else(|| panic!("{algo:?} not detected"));
        assert_eq!(layer.params.algorithm(), algo);
        assert_eq!(inner, sample());
        assert_eq!(layer.wrap(&inner).unwrap(), original, "identity re-wrap");
        if expect_exact {
            assert!(layer.exact, "{algo:?} should be exact");
            assert_eq!(layer.params.compress(&inner).unwrap(), original[layer.size_prefix.map(|p| p.width()).unwrap_or(0)..original.len() - layer.framing.suffix.len()]);
        }
        // Edit → recompress → decompress yields the edit.
        let mut edited = inner.clone();
        edited[10] = b'X';
        let rewrapped = layer.wrap(&edited).unwrap();
        let (_, back) = CompressionLayer::detect(&rewrapped).expect("re-detect");
        assert_eq!(back, edited);
    }

    #[test]
    fn zlib_levels_are_recovered() {
        for level in [1u32, 6, 9] {
            let c = deflate(&sample(), level, true).unwrap();
            check(&c, CompressionAlgorithm::Zlib, true);
        }
    }

    #[test]
    fn gzip_with_name_header() {
        let mut header = vec![0x1f, 0x8b, 8, 0x08, 1, 2, 3, 4, 0, 3];
        header.extend_from_slice(b"save.json\0");
        let params = StreamParams::Gzip { header, level: 6 };
        let c = params.compress(&sample()).unwrap();
        check(&c, CompressionAlgorithm::Gzip, true);
    }

    #[test]
    fn zstd_with_checksum() {
        let p = StreamParams::Zstd {
            level: 5,
            checksum: true,
            content_size: true,
        };
        let c = p.compress(&sample()).unwrap();
        check(&c, CompressionAlgorithm::Zstd, true);
    }

    #[test]
    fn lz4_frame() {
        let p = StreamParams::Lz4Frame {
            block_size: lz4_flex::frame::BlockSize::Max64KB,
            independent: true,
            block_checksums: false,
            content_checksum: true,
            content_size: false,
        };
        let c = p.compress(&sample()).unwrap();
        check(&c, CompressionAlgorithm::Lz4Frame, true);
    }

    #[test]
    fn lz4_block_with_size_prefix() {
        let mut c = (sample().len() as u32).to_le_bytes().to_vec();
        c.extend_from_slice(&lz4_flex::block::compress(&sample()));
        check(&c, CompressionAlgorithm::Lz4Block, true);
        let (layer, _) = CompressionLayer::detect(&c).unwrap();
        assert_eq!(layer.size_prefix, Some(SizePrefix::U32Le));
    }

    #[test]
    fn zlib_with_u64_prefix_and_suffix() {
        let mut c = (sample().len() as u64).to_le_bytes().to_vec();
        c.extend_from_slice(&deflate(&sample(), 6, true).unwrap());
        c.extend_from_slice(b"TRAILER!");
        let (layer, inner) = CompressionLayer::detect(&c).unwrap();
        assert_eq!(layer.size_prefix, Some(SizePrefix::U64Le));
        assert_eq!(layer.framing.suffix, b"TRAILER!");
        assert_eq!(layer.wrap(&inner).unwrap(), c);
        let mut edited = inner.clone();
        edited.extend_from_slice(b"more");
        let out = layer.wrap(&edited).unwrap();
        assert_eq!(u64::from_le_bytes(out[..8].try_into().unwrap()) as usize, edited.len());
        assert!(out.ends_with(b"TRAILER!"));
    }

    #[test]
    fn raw_deflate_with_prefix() {
        let mut c = (sample().len() as u32).to_be_bytes().to_vec();
        c.extend_from_slice(&deflate(&sample(), 9, false).unwrap());
        let (layer, inner) = CompressionLayer::detect(&c).unwrap();
        assert_eq!(inner, sample());
        assert!(matches!(layer.params, StreamParams::Deflate { level: 9 }));
    }

    #[test]
    fn plain_text_not_detected() {
        assert!(CompressionLayer::detect(&sample()).is_none());
        assert!(CompressionLayer::detect(b"x").is_none());
    }

    #[test]
    fn corrupt_streams_are_errors() {
        let mut c = deflate(&sample(), 6, true).unwrap();
        let n = c.len();
        c.truncate(n / 2);
        assert!(decode_stream(CompressionAlgorithm::Zlib, &c, None).is_err());
        let mut g = StreamParams::Gzip {
            header: vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3],
            level: 6,
        }
        .compress(&sample())
        .unwrap();
        let l = g.len();
        g[l - 5] ^= 1; // corrupt CRC
        assert!(decode_stream(CompressionAlgorithm::Gzip, &g, None).is_err());
    }
}
