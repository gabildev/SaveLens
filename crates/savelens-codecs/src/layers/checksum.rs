//! Smart Checksum Hunter & Recalculator.
//!
//! Detects header/trailer digests (CRC32, CRC32C, Adler32, FNV-1a, MD5, SHA1,
//! SHA256 — raw LE/BE or ASCII hex) covering the rest of the payload, strips
//! them, and recomputes them whenever the payload changes.

use md5::Md5;
use savelens_core::{ChecksumAlgorithm, ChecksumRule, DigestEncoding, FieldAnchor, LayerDescriptor};
use sha1::{Digest, Sha1};
use sha2::Sha256;

use super::{Identity, Layer};
use crate::error::{CodecError, CodecResult};

/// Streaming digest over up to two slices (the stripped payload is the input
/// minus the checksum field, i.e. two discontiguous slices).
fn digest_parts(algo: ChecksumAlgorithm, parts: &[&[u8]]) -> Vec<u8> {
    match algo {
        ChecksumAlgorithm::Crc32 => {
            let mut h = crc32fast::Hasher::new();
            for p in parts {
                h.update(p);
            }
            h.finalize().to_be_bytes().to_vec()
        }
        ChecksumAlgorithm::Crc32c => {
            let mut c = 0u32;
            for p in parts {
                c = crc32c::crc32c_append(c, p);
            }
            c.to_be_bytes().to_vec()
        }
        ChecksumAlgorithm::Adler32 => {
            let (mut a, mut b) = (1u32, 0u32);
            for p in parts {
                for chunk in p.chunks(5552) {
                    for &x in chunk {
                        a += x as u32;
                        b += a;
                    }
                    a %= 65521;
                    b %= 65521;
                }
            }
            ((b << 16) | a).to_be_bytes().to_vec()
        }
        ChecksumAlgorithm::Fnv1a32 => {
            let mut h: u32 = 0x811c_9dc5;
            for p in parts {
                for &x in *p {
                    h ^= x as u32;
                    h = h.wrapping_mul(0x0100_0193);
                }
            }
            h.to_be_bytes().to_vec()
        }
        ChecksumAlgorithm::Fnv1a64 => {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for p in parts {
                for &x in *p {
                    h ^= x as u64;
                    h = h.wrapping_mul(0x0000_0100_0000_01b3);
                }
            }
            h.to_be_bytes().to_vec()
        }
        ChecksumAlgorithm::Md5 => {
            let mut h = Md5::new();
            for p in parts {
                h.update(p);
            }
            h.finalize().to_vec()
        }
        ChecksumAlgorithm::Sha1 => {
            let mut h = Sha1::new();
            for p in parts {
                h.update(p);
            }
            h.finalize().to_vec()
        }
        ChecksumAlgorithm::Sha256 => {
            let mut h = Sha256::new();
            for p in parts {
                h.update(p);
            }
            h.finalize().to_vec()
        }
    }
}

/// Digest of a contiguous buffer (big-endian / natural byte order).
pub fn digest(algo: ChecksumAlgorithm, data: &[u8]) -> Vec<u8> {
    digest_parts(algo, &[data])
}

fn is_integer_algo(algo: ChecksumAlgorithm) -> bool {
    algo.digest_len() <= 8
}

/// Encodings that make sense for an algorithm.
pub fn encodings_for(algo: ChecksumAlgorithm) -> &'static [DigestEncoding] {
    if is_integer_algo(algo) {
        &[DigestEncoding::RawLe, DigestEncoding::RawBe, DigestEncoding::HexLower, DigestEncoding::HexUpper]
    } else {
        &[DigestEncoding::RawBe, DigestEncoding::HexLower, DigestEncoding::HexUpper]
    }
}

/// Encodes a natural-order digest for storage.
pub fn encode_digest(algo: ChecksumAlgorithm, enc: DigestEncoding, natural: &[u8]) -> Vec<u8> {
    match enc {
        DigestEncoding::RawBe => natural.to_vec(),
        DigestEncoding::RawLe => {
            if is_integer_algo(algo) {
                natural.iter().rev().copied().collect()
            } else {
                natural.to_vec()
            }
        }
        DigestEncoding::HexLower => hex::encode(natural).into_bytes(),
        DigestEncoding::HexUpper => hex::encode_upper(natural).into_bytes(),
    }
}

/// Byte range `[start, end)` of the field within an input of `len` bytes.
fn field_range(rule: &ChecksumRule, len: usize) -> Option<(usize, usize)> {
    let flen = rule.encoding.encoded_len(rule.algorithm.digest_len());
    match rule.field {
        FieldAnchor::FromStart(o) => {
            let end = o.checked_add(flen)?;
            (end <= len).then_some((o, end))
        }
        FieldAnchor::FromEnd(n) => {
            let end = len.checked_sub(n)?;
            let start = end.checked_sub(flen)?;
            Some((start, end))
        }
    }
}

/// Covered range of the stripped payload as up to two slices of the input.
fn covered_parts<'a>(rule: &ChecksumRule, data: &'a [u8], fs: usize, fe: usize) -> Option<[&'a [u8]; 2]> {
    let stripped_len = data.len() - (fe - fs);
    let cs = rule.covered_skip_start;
    let ce = stripped_len.checked_sub(rule.covered_skip_end)?;
    if cs > ce {
        return None;
    }
    // Map stripped coordinates to input coordinates: [0, fs) maps directly,
    // [fs, stripped_len) maps to [fe, len).
    let map = |a: usize, b: usize| -> [&'a [u8]; 2] {
        let first = if a < fs { &data[a..b.min(fs)] } else { &data[0..0] };
        let second = if b > fs {
            let sa = a.max(fs) + (fe - fs);
            let sb = b + (fe - fs);
            &data[sa..sb]
        } else {
            &data[0..0]
        };
        [first, second]
    };
    Some(map(cs, ce))
}

/// Checks whether `rule` holds for `data`.
pub fn verify(rule: &ChecksumRule, data: &[u8]) -> bool {
    let Some((fs, fe)) = field_range(rule, data.len()) else { return false };
    let Some(parts) = covered_parts(rule, data, fs, fe) else { return false };
    let covered: usize = parts.iter().map(|p| p.len()).sum();
    if covered < 4 {
        return false;
    }
    let natural = digest_parts(rule.algorithm, &parts);
    encode_digest(rule.algorithm, rule.encoding, &natural) == data[fs..fe]
}

/// Removes the checksum field.
pub fn strip(rule: &ChecksumRule, data: &[u8]) -> CodecResult<Vec<u8>> {
    let (fs, fe) = field_range(rule, data.len())
        .ok_or_else(|| CodecError::Checksum(format!("input of {} bytes too short for rule {rule}", data.len())))?;
    let mut out = Vec::with_capacity(data.len() - (fe - fs));
    out.extend_from_slice(&data[..fs]);
    out.extend_from_slice(&data[fe..]);
    Ok(out)
}

/// Computes the digest over the stripped payload and inserts the field.
pub fn sign(rule: &ChecksumRule, stripped: &[u8]) -> CodecResult<Vec<u8>> {
    let cs = rule.covered_skip_start;
    let ce = stripped.len().checked_sub(rule.covered_skip_end).filter(|ce| *ce >= cs).ok_or_else(|| {
        CodecError::Checksum(format!("payload of {} bytes too short for rule {rule}", stripped.len()))
    })?;
    let natural = digest(rule.algorithm, &stripped[cs..ce]);
    let field = encode_digest(rule.algorithm, rule.encoding, &natural);
    let pos = match rule.field {
        FieldAnchor::FromStart(o) => o,
        FieldAnchor::FromEnd(n) => stripped
            .len()
            .checked_sub(n)
            .ok_or_else(|| CodecError::Checksum("payload shorter than trailer offset".into()))?,
    };
    if pos > stripped.len() {
        return Err(CodecError::Checksum(format!(
            "checksum field offset {pos} beyond payload of {} bytes",
            stripped.len()
        )));
    }
    let mut out = Vec::with_capacity(stripped.len() + field.len());
    out.extend_from_slice(&stripped[..pos]);
    out.extend_from_slice(&field);
    out.extend_from_slice(&stripped[pos..]);
    Ok(out)
}

/// Candidate rules in priority order for an input of `len` bytes.
pub fn candidate_rules(len: usize) -> Vec<ChecksumRule> {
    let mut out = Vec::new();
    let large = len > 4 * 1024 * 1024;
    for algo in ChecksumAlgorithm::ALL {
        let cheap = is_integer_algo(algo);
        for &enc in encodings_for(algo) {
            let flen = enc.encoded_len(algo.digest_len());
            if len < flen + 8 {
                continue;
            }
            // Trailer.
            let trailer_skips: &[usize] = if large && !cheap { &[0] } else { &[0, 4, 8, 16, 32] };
            for &skip in trailer_skips {
                out.push(ChecksumRule {
                    algorithm: algo,
                    encoding: enc,
                    field: FieldAnchor::FromEnd(0),
                    covered_skip_start: skip,
                    covered_skip_end: 0,
                });
            }
            if !(large && !cheap) {
                // Trailer followed by a 4-byte footer (e.g. version), footer not covered.
                out.push(ChecksumRule {
                    algorithm: algo,
                    encoding: enc,
                    field: FieldAnchor::FromEnd(4),
                    covered_skip_start: 0,
                    covered_skip_end: 4,
                });
            }
            // Header at common offsets; covered = everything else, or only what follows the field.
            let offsets: &[usize] = if large && !cheap { &[0] } else { &[0, 4, 8, 12, 16, 32] };
            for &o in offsets {
                if o + flen + 8 > len {
                    continue;
                }
                let mut skips = vec![0usize];
                if o > 0 {
                    skips.push(o);
                }
                for skip in skips {
                    out.push(ChecksumRule {
                        algorithm: algo,
                        encoding: enc,
                        field: FieldAnchor::FromStart(o),
                        covered_skip_start: skip,
                        covered_skip_end: 0,
                    });
                }
            }
        }
    }
    out
}

/// Searches for a checksum rule matching `data`.
pub fn hunt(data: &[u8]) -> Option<ChecksumRule> {
    if data.len() < 12 || data.len() > 512 * 1024 * 1024 {
        return None;
    }
    candidate_rules(data.len()).into_iter().find(|r| {
        // Reject degenerate fields (all zero / all 0xff) — they match nothing meaningful.
        let Some((fs, fe)) = field_range(r, data.len()) else { return false };
        let field = &data[fs..fe];
        if field.iter().all(|&b| b == 0) || field.iter().all(|&b| b == 0xff) {
            return false;
        }
        verify(r, data)
    })
}

/// Checksum layer: strips the field on open, re-signs on wrap.
#[derive(Debug, Clone)]
pub struct ChecksumLayer {
    pub rule: ChecksumRule,
    identity: Identity,
}

impl ChecksumLayer {
    /// Detects a checksum on `data` and returns the layer and stripped payload.
    pub fn detect(data: &[u8]) -> Option<(ChecksumLayer, Vec<u8>)> {
        let rule = hunt(data)?;
        let inner = strip(&rule, data).ok()?;
        Some((ChecksumLayer { rule, identity: Identity::new(data, &inner) }, inner))
    }

    /// Builds a layer from a known rule (verifying it).
    pub fn with_rule(rule: ChecksumRule, data: &[u8]) -> CodecResult<(ChecksumLayer, Vec<u8>)> {
        if !verify(&rule, data) {
            return Err(CodecError::Checksum(format!("rule {rule} does not match the data")));
        }
        let inner = strip(&rule, data)?;
        Ok((ChecksumLayer { rule, identity: Identity::new(data, &inner) }, inner))
    }
}

impl Layer for ChecksumLayer {
    fn descriptor(&self) -> LayerDescriptor {
        LayerDescriptor::Checksum { rule: self.rule }
    }

    fn wrap(&self, inner: &[u8]) -> CodecResult<Vec<u8>> {
        if let Some(outer) = self.identity.lookup(inner) {
            return Ok(outer);
        }
        sign(&self.rule, inner)
    }

    fn exact_reencode(&self) -> bool {
        true
    }

    fn clone_box(&self) -> Box<dyn Layer> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> Vec<u8> {
        (0..200u32).flat_map(|i| (i * 7919).to_le_bytes()).collect()
    }

    #[test]
    fn known_vectors() {
        assert_eq!(digest(ChecksumAlgorithm::Crc32, b"123456789"), 0xCBF4_3926u32.to_be_bytes());
        assert_eq!(digest(ChecksumAlgorithm::Crc32c, b"123456789"), 0xE306_9283u32.to_be_bytes());
        assert_eq!(digest(ChecksumAlgorithm::Adler32, b"Wikipedia"), 0x11E6_0398u32.to_be_bytes());
        assert_eq!(digest(ChecksumAlgorithm::Fnv1a32, b"a"), 0xe40c_292cu32.to_be_bytes());
        assert_eq!(digest(ChecksumAlgorithm::Fnv1a64, b"a"), 0xaf63_dc4c_8601_ec8cu64.to_be_bytes());
        assert_eq!(hex::encode(digest(ChecksumAlgorithm::Md5, b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(hex::encode(digest(ChecksumAlgorithm::Sha1, b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            hex::encode(digest(ChecksumAlgorithm::Sha256, b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn adler_long_input_matches_reference() {
        // Reference: the zlib Adler-32 of 100000 bytes of 0xff.
        let data = vec![0xffu8; 100_000];
        let mut a: u64 = 1;
        let mut b: u64 = 0;
        for &x in &data {
            a = (a + x as u64) % 65521;
            b = (b + a) % 65521;
        }
        let expect = ((b << 16) | a) as u32;
        assert_eq!(digest(ChecksumAlgorithm::Adler32, &data), expect.to_be_bytes());
    }

    fn roundtrip(rule: ChecksumRule) {
        let body = payload();
        let signed = sign(&rule, &body).unwrap();
        assert!(verify(&rule, &signed), "{rule}");
        let found = hunt(&signed).expect("hunter found nothing");
        // The hunter may find an equivalent rule; it must strip to the same body.
        let (layer, inner) = ChecksumLayer::with_rule(found, &signed).unwrap();
        assert_eq!(inner, body, "{rule} found as {found}");
        assert_eq!(layer.wrap(&inner).unwrap(), signed);
        // Edit + re-sign.
        let mut edited = inner.clone();
        edited[100] ^= 0x55;
        let resigned = layer.wrap(&edited).unwrap();
        assert!(verify(&found, &resigned));
        assert_ne!(resigned, signed);
    }

    #[test]
    fn all_algorithms_header_and_trailer() {
        for algo in ChecksumAlgorithm::ALL {
            for &enc in encodings_for(algo) {
                roundtrip(ChecksumRule {
                    algorithm: algo,
                    encoding: enc,
                    field: FieldAnchor::FromEnd(0),
                    covered_skip_start: 0,
                    covered_skip_end: 0,
                });
                roundtrip(ChecksumRule {
                    algorithm: algo,
                    encoding: enc,
                    field: FieldAnchor::FromStart(0),
                    covered_skip_start: 0,
                    covered_skip_end: 0,
                });
            }
        }
    }

    #[test]
    fn header_after_magic_covering_only_tail() {
        roundtrip(ChecksumRule {
            algorithm: ChecksumAlgorithm::Crc32,
            encoding: DigestEncoding::RawLe,
            field: FieldAnchor::FromStart(8),
            covered_skip_start: 8,
            covered_skip_end: 0,
        });
        roundtrip(ChecksumRule {
            algorithm: ChecksumAlgorithm::Sha1,
            encoding: DigestEncoding::RawBe,
            field: FieldAnchor::FromStart(4),
            covered_skip_start: 0,
            covered_skip_end: 0,
        });
        roundtrip(ChecksumRule {
            algorithm: ChecksumAlgorithm::Md5,
            encoding: DigestEncoding::HexLower,
            field: FieldAnchor::FromEnd(0),
            covered_skip_start: 16,
            covered_skip_end: 0,
        });
    }

    #[test]
    fn trailer_with_footer() {
        roundtrip(ChecksumRule {
            algorithm: ChecksumAlgorithm::Crc32,
            encoding: DigestEncoding::RawLe,
            field: FieldAnchor::FromEnd(4),
            covered_skip_start: 0,
            covered_skip_end: 4,
        });
    }

    #[test]
    fn plain_data_has_no_checksum() {
        assert!(hunt(&payload()).is_none());
        assert!(hunt(b"{\"gold\": 1500, \"hp\": 20, \"name\": \"Hero\"}").is_none());
        assert!(hunt(&[0u8; 100]).is_none());
    }
}
