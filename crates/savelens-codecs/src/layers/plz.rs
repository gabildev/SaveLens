//! Palworld `PlZ` save wrapper.
//!
//! Layout: `u32 LE uncompressed length`, `u32 LE compressed length`,
//! magic `PlZ`, type byte (`'1'` = one zlib stream, `'2'` = zlib of zlib),
//! then the zlib data. For type `'2'` the compressed-length field holds the
//! size of the *inner* zlib stream. The payload is a GVAS save.

use savelens_core::{CompressionAlgorithm, LayerDescriptor};

use super::compression::CompressionLayer;
use super::{Identity, Layer};
use crate::error::{CodecError, CodecResult};

const MAGIC: &[u8; 3] = b"PlZ";

fn plz_err(reason: impl Into<String>) -> CodecError {
    CodecError::Compression { algorithm: "Palworld PlZ".into(), reason: reason.into() }
}

/// The PlZ wrapper layer.
#[derive(Debug, Clone)]
pub struct PlzLayer {
    save_type: u8,
    outer: CompressionLayer,
    inner: Option<CompressionLayer>,
    identity: Identity,
}

impl PlzLayer {
    /// Detects and unwraps a PlZ save.
    pub fn detect(data: &[u8]) -> Option<(PlzLayer, Vec<u8>)> {
        Self::open(data).ok()
    }

    /// Unwraps a PlZ save, reporting why it is not one.
    pub fn open(data: &[u8]) -> CodecResult<(PlzLayer, Vec<u8>)> {
        if data.len() < 14 || &data[8..11] != MAGIC {
            return Err(plz_err("missing PlZ header"));
        }
        let read_u32 = |o: usize| u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]) as usize;
        let (ulen, clen, save_type) = (read_u32(0), read_u32(4), data[11]);
        let (outer, stage1) = CompressionLayer::open_as(CompressionAlgorithm::Zlib, &data[12..], None)?;
        if !outer.framing.is_empty() {
            return Err(plz_err("trailing bytes after the zlib stream"));
        }
        let (inner, payload) = match save_type {
            b'1' => {
                if clen != data.len() - 12 {
                    return Err(plz_err("compressed length field does not match the stream"));
                }
                (None, stage1)
            }
            b'2' => {
                if clen != stage1.len() {
                    return Err(plz_err("inner compressed length field does not match"));
                }
                let (inner, payload) = CompressionLayer::open_as(CompressionAlgorithm::Zlib, &stage1, None)?;
                if !inner.framing.is_empty() {
                    return Err(plz_err("trailing bytes after the inner zlib stream"));
                }
                (Some(inner), payload)
            }
            other => return Err(plz_err(format!("unsupported PlZ save type {:#04x}", other))),
        };
        if payload.len() != ulen {
            return Err(plz_err("uncompressed length field does not match"));
        }
        Ok((PlzLayer { save_type, outer, inner, identity: Identity::new(data, &payload) }, payload))
    }
}

impl Layer for PlzLayer {
    fn descriptor(&self) -> LayerDescriptor {
        let exact = self.outer.exact && self.inner.as_ref().is_none_or(|i| i.exact);
        let detail = match self.save_type {
            b'2' => "type 2, zlib ×2",
            _ => "type 1, zlib",
        };
        LayerDescriptor::Wrapper { name: "Palworld PlZ".into(), detail: detail.into(), exact }
    }

    fn wrap(&self, payload: &[u8]) -> CodecResult<Vec<u8>> {
        if let Some(outer) = self.identity.lookup(payload) {
            return Ok(outer);
        }
        let ulen = u32::try_from(payload.len()).map_err(|_| plz_err("payload larger than 4 GiB"))?;
        let (body, clen) = match &self.inner {
            Some(inner) => {
                let stage1 = inner.params.compress(payload)?;
                let clen = stage1.len();
                (self.outer.params.compress(&stage1)?, clen)
            }
            None => {
                let body = self.outer.params.compress(payload)?;
                let clen = body.len();
                (body, clen)
            }
        };
        let clen = u32::try_from(clen).map_err(|_| plz_err("compressed stream larger than 4 GiB"))?;
        let mut out = Vec::with_capacity(12 + body.len());
        out.extend_from_slice(&ulen.to_le_bytes());
        out.extend_from_slice(&clen.to_le_bytes());
        out.extend_from_slice(MAGIC);
        out.push(self.save_type);
        out.extend_from_slice(&body);
        Ok(out)
    }

    fn exact_reencode(&self) -> bool {
        self.outer.exact && self.inner.as_ref().is_none_or(|i| i.exact)
    }

    fn clone_box(&self) -> Box<dyn Layer> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layers::compression::deflate;

    fn build(payload: &[u8], double: bool) -> Vec<u8> {
        let stage1 = deflate(payload, 6, true).unwrap();
        let (body, clen) =
            if double { (deflate(&stage1, 6, true).unwrap(), stage1.len()) } else { (stage1.clone(), stage1.len()) };
        let mut out = (payload.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(&(clen as u32).to_le_bytes());
        out.extend_from_slice(b"PlZ");
        out.push(if double { b'2' } else { b'1' });
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn single_and_double_roundtrip_and_edit() {
        let payload: Vec<u8> =
            b"GVAS".iter().copied().chain((0..5000u32).flat_map(|i| (i % 97).to_le_bytes())).collect();
        for double in [false, true] {
            let file = build(&payload, double);
            let (layer, inner) = PlzLayer::open(&file).unwrap();
            assert_eq!(inner, payload);
            assert_eq!(layer.wrap(&inner).unwrap(), file);
            let mut edited = payload.clone();
            edited[100] = 0xEE;
            let out = layer.wrap(&edited).unwrap();
            assert_eq!(PlzLayer::open(&out).unwrap().1, edited);
            assert!(layer.descriptor().to_string().contains("Palworld PlZ"));
        }
    }

    #[test]
    fn rejects_inconsistent_headers() {
        let mut file = build(b"GVAS-payload-bytes", false);
        file[0] ^= 1;
        assert!(PlzLayer::open(&file).is_err());
        assert!(PlzLayer::open(b"short").is_err());
        let mut bad = build(b"GVAS-payload-bytes", false);
        bad[11] = b'9';
        assert!(PlzLayer::open(&bad).is_err());
    }
}
