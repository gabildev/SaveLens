//! Base64 text layer (standard / URL-safe, padded or not, optionally wrapped
//! into fixed-width lines).

use base64::engine::general_purpose::{GeneralPurpose, STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use savelens_core::{Base64Variant, LayerDescriptor};

use super::{Identity, Layer};
use crate::error::{CodecError, CodecResult};

fn engine(v: Base64Variant) -> &'static GeneralPurpose {
    match v {
        Base64Variant::Standard => &STANDARD,
        Base64Variant::StandardNoPad => &STANDARD_NO_PAD,
        Base64Variant::UrlSafe => &URL_SAFE,
        Base64Variant::UrlSafeNoPad => &URL_SAFE_NO_PAD,
    }
}

/// Exact textual layout of a base64 payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base64Layout {
    pub variant: Base64Variant,
    /// Characters per line (0 = single line).
    pub line_width: usize,
    /// Line separator used when wrapped.
    pub line_ending: &'static str,
    /// Whitespace before / after the encoded text.
    pub leading: Vec<u8>,
    pub trailing: Vec<u8>,
}

impl Base64Layout {
    /// Encodes `data` with this layout.
    pub fn encode(&self, data: &[u8]) -> Vec<u8> {
        let text = engine(self.variant).encode(data);
        let mut out = self.leading.clone();
        if self.line_width == 0 {
            out.extend_from_slice(text.as_bytes());
        } else {
            for (i, chunk) in text.as_bytes().chunks(self.line_width).enumerate() {
                if i > 0 {
                    out.extend_from_slice(self.line_ending.as_bytes());
                }
                out.extend_from_slice(chunk);
            }
        }
        out.extend_from_slice(&self.trailing);
        out
    }
}

/// Parses base64 text, inferring its layout. Returns `None` for anything that
/// is not plausibly base64 (too short, wrong alphabet, inconsistent lines).
pub fn parse(data: &[u8]) -> Option<(Base64Layout, Vec<u8>)> {
    let start = data.iter().position(|b| !b.is_ascii_whitespace())?;
    let end = data.iter().rposition(|b| !b.is_ascii_whitespace())? + 1;
    let body = &data[start..end];
    if body.len() < 16 {
        return None;
    }
    let line_ending: &'static str = if body.windows(2).any(|w| w == b"\r\n") { "\r\n" } else { "\n" };
    let lines: Vec<&[u8]> = if body.contains(&b'\n') {
        body.split(|&b| b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l)).collect()
    } else {
        vec![body]
    };
    let line_width = if lines.len() > 1 {
        let w = lines[0].len();
        if w == 0 || lines[..lines.len() - 1].iter().any(|l| l.len() != w) || lines[lines.len() - 1].len() > w {
            return None;
        }
        w
    } else {
        0
    };
    let joined: Vec<u8> = lines.concat();
    let url = joined.iter().any(|&b| b == b'-' || b == b'_');
    let std_chars = joined.iter().any(|&b| b == b'+' || b == b'/');
    if url && std_chars {
        return None;
    }
    let valid = joined.iter().all(|&b| {
        b.is_ascii_alphanumeric() || b == b'=' || if url { b == b'-' || b == b'_' } else { b == b'+' || b == b'/' }
    });
    if !valid {
        return None;
    }
    // Padding only at the very end.
    let pad = joined.iter().rev().take_while(|&&b| b == b'=').count();
    if pad > 2 || joined[..joined.len() - pad].contains(&b'=') {
        return None;
    }
    let variant = match (url, pad > 0 || joined.len() % 4 == 0) {
        (false, true) => Base64Variant::Standard,
        (false, false) => Base64Variant::StandardNoPad,
        (true, true) => Base64Variant::UrlSafe,
        (true, false) => Base64Variant::UrlSafeNoPad,
    };
    let decoded = engine(variant).decode(&joined).ok()?;
    if decoded.is_empty() {
        return None;
    }
    let layout = Base64Layout {
        variant,
        line_width,
        line_ending,
        leading: data[..start].to_vec(),
        trailing: data[end..].to_vec(),
    };
    Some((layout, decoded))
}

/// Base64 envelope layer.
#[derive(Debug, Clone)]
pub struct Base64Layer {
    pub layout: Base64Layout,
    pub exact: bool,
    identity: Identity,
}

impl Base64Layer {
    /// Detects base64 text. The pipeline only keeps the layer if the decoded
    /// payload is itself recognised (to avoid decoding ordinary words).
    pub fn detect(data: &[u8]) -> Option<(Base64Layer, Vec<u8>)> {
        let (layout, decoded) = parse(data)?;
        let exact = layout.encode(&decoded) == data;
        Some((Base64Layer { layout, exact, identity: Identity::new(data, &decoded) }, decoded))
    }
}

impl Layer for Base64Layer {
    fn descriptor(&self) -> LayerDescriptor {
        LayerDescriptor::Base64 { variant: self.layout.variant, line_width: self.layout.line_width }
    }

    fn wrap(&self, inner: &[u8]) -> CodecResult<Vec<u8>> {
        if let Some(outer) = self.identity.lookup(inner) {
            return Ok(outer);
        }
        let out = self.layout.encode(inner);
        if out.is_empty() && !inner.is_empty() {
            return Err(CodecError::Compression {
                algorithm: "base64".into(),
                reason: "encoder produced no output".into(),
            });
        }
        Ok(out)
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

    #[test]
    fn single_line_variants() {
        let data: Vec<u8> = (0..100u8).collect();
        for v in
            [Base64Variant::Standard, Base64Variant::StandardNoPad, Base64Variant::UrlSafe, Base64Variant::UrlSafeNoPad]
        {
            let text = engine(v).encode(&data).into_bytes();
            let (layer, inner) = Base64Layer::detect(&text).unwrap();
            assert_eq!(inner, data);
            assert!(layer.exact, "{v:?}");
            assert_eq!(layer.wrap(&inner).unwrap(), text);
        }
    }

    #[test]
    fn wrapped_lines_with_crlf_and_trailing_newline() {
        let data: Vec<u8> = (0..=255u8).cycle().take(300).collect();
        let layout = Base64Layout {
            variant: Base64Variant::Standard,
            line_width: 76,
            line_ending: "\r\n",
            leading: Vec::new(),
            trailing: b"\r\n".to_vec(),
        };
        let text = layout.encode(&data);
        let (layer, inner) = Base64Layer::detect(&text).unwrap();
        assert_eq!(layer.layout, layout);
        assert_eq!(inner, data);
        assert!(layer.exact);
        let mut edited = inner.clone();
        edited[0] = 1;
        let out = layer.wrap(&edited).unwrap();
        assert_eq!(parse(&out).unwrap().1, edited);
    }

    #[test]
    fn rejects_non_base64() {
        assert!(parse(b"hello world, this is not base64!").is_none());
        assert!(parse(b"short").is_none());
        assert!(parse(b"abcd=efghijklmnopqrstuvwx").is_none());
        assert!(parse(b"ab+d_fghijklmnopqrstuvwx").is_none());
    }
}
