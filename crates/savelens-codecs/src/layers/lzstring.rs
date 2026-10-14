//! LZString text layer (`LZString.compressToBase64` and
//! `compressToEncodedURIComponent`), used by RPG Maker MV saves (`.rpgsave`)
//! and Twine/SugarCube saves.
//!
//! This is a faithful port of pieroxy's lz-string 1.4.x algorithm (the
//! library those engines ship), so re-compressing an unmodified payload
//! reproduces the original text exactly. The decompressed string is exposed
//! as UTF-8 bytes (typically JSON).

use std::collections::{HashMap, HashSet};

use savelens_core::LayerDescriptor;

use super::{Identity, Layer};
use crate::error::{CodecError, CodecResult};

const BASE64_ALPHABET: &[u8; 65] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=";
const URI_ALPHABET: &[u8; 65] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+-$";

/// Which lz-string text encoding is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LzVariant {
    /// `compressToBase64` (padded with `=`).
    Base64,
    /// `compressToEncodedURIComponent`.
    Uri,
}

impl LzVariant {
    fn alphabet(self) -> &'static [u8; 65] {
        match self {
            LzVariant::Base64 => BASE64_ALPHABET,
            LzVariant::Uri => URI_ALPHABET,
        }
    }

    fn name(self) -> &'static str {
        match self {
            LzVariant::Base64 => "compressToBase64",
            LzVariant::Uri => "compressToEncodedURIComponent",
        }
    }
}

struct BitWriter<'a> {
    out: Vec<u8>,
    val: u32,
    pos: u32,
    bits_per_char: u32,
    alphabet: &'a [u8; 65],
}

impl BitWriter<'_> {
    fn push(&mut self, bit: u32) {
        self.val = (self.val << 1) | bit;
        if self.pos == self.bits_per_char - 1 {
            self.pos = 0;
            self.out.push(self.alphabet[self.val as usize]);
            self.val = 0;
        } else {
            self.pos += 1;
        }
    }

    fn push_value(&mut self, mut value: u32, nbits: u32) {
        for _ in 0..nbits {
            self.push(value & 1);
            value >>= 1;
        }
    }

    fn flush(mut self) -> Vec<u8> {
        loop {
            self.val <<= 1;
            if self.pos == self.bits_per_char - 1 {
                self.out.push(self.alphabet[self.val as usize]);
                break;
            }
            self.pos += 1;
        }
        self.out
    }
}

/// `_compress(uncompressed, 6, alphabet)` over UTF-16 code units.
fn compress_units(input: &[u16], alphabet: &'static [u8; 65]) -> Vec<u8> {
    let mut dictionary: HashMap<Vec<u16>, u32> = HashMap::new();
    let mut to_create: HashSet<Vec<u16>> = HashSet::new();
    let mut w: Vec<u16> = Vec::new();
    let mut enlarge_in: u32 = 2;
    let mut dict_size: u32 = 3;
    let mut num_bits: u32 = 2;
    let mut bw = BitWriter { out: Vec::new(), val: 0, pos: 0, bits_per_char: 6, alphabet };

    fn emit_w(
        w: &[u16],
        dictionary: &HashMap<Vec<u16>, u32>,
        to_create: &mut HashSet<Vec<u16>>,
        bw: &mut BitWriter<'_>,
        enlarge_in: &mut u32,
        num_bits: &mut u32,
    ) {
        if to_create.contains(w) {
            let ch = w[0] as u32;
            if ch < 256 {
                bw.push_value(0, *num_bits);
                bw.push_value(ch, 8);
            } else {
                bw.push_value(1, *num_bits);
                bw.push_value(ch, 16);
            }
            *enlarge_in -= 1;
            if *enlarge_in == 0 {
                *enlarge_in = 1 << *num_bits;
                *num_bits += 1;
            }
            to_create.remove(w);
        } else {
            let value = dictionary.get(w).copied().unwrap_or(0);
            bw.push_value(value, *num_bits);
        }
        *enlarge_in -= 1;
        if *enlarge_in == 0 {
            *enlarge_in = 1 << *num_bits;
            *num_bits += 1;
        }
    }

    for &c in input {
        let cv = vec![c];
        if !dictionary.contains_key(&cv) {
            dictionary.insert(cv.clone(), dict_size);
            dict_size += 1;
            to_create.insert(cv.clone());
        }
        let mut wc = w.clone();
        wc.push(c);
        if dictionary.contains_key(&wc) {
            w = wc;
        } else {
            emit_w(&w, &dictionary, &mut to_create, &mut bw, &mut enlarge_in, &mut num_bits);
            dictionary.insert(wc, dict_size);
            dict_size += 1;
            w = cv;
        }
    }
    if !w.is_empty() {
        emit_w(&w, &dictionary, &mut to_create, &mut bw, &mut enlarge_in, &mut num_bits);
    }
    bw.push_value(2, num_bits);
    bw.flush()
}

/// Compresses a string with the given variant.
pub fn compress(text: &str, variant: LzVariant) -> Vec<u8> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut out = compress_units(&units, variant.alphabet());
    if variant == LzVariant::Base64 {
        while !out.len().is_multiple_of(4) {
            out.push(b'=');
        }
    }
    out
}

fn lz_err(reason: &str) -> CodecError {
    CodecError::Compression { algorithm: "LZString".into(), reason: reason.into() }
}

/// Upper bound of decompressed code units (zip-bomb guard).
const MAX_UNITS: usize = 256 * 1024 * 1024;

/// `_decompress(length, 32, getBaseValue)`; returns UTF-16 code units.
fn decompress_units(input: &[u8], variant: LzVariant) -> CodecResult<Vec<u16>> {
    let alphabet = variant.alphabet();
    let mut reverse = [255u8; 256];
    for (i, &c) in alphabet.iter().enumerate() {
        reverse[c as usize] = i as u8;
    }
    let value_at = |i: usize| -> CodecResult<u32> {
        match input.get(i) {
            None => Ok(0),
            Some(&c) => match reverse[c as usize] {
                255 => Err(lz_err("character outside the lz-string alphabet")),
                v => Ok(v as u32),
            },
        }
    };
    let length = input.len();
    let reset: u32 = 32;
    let mut data_val = value_at(0)?;
    let mut data_pos = reset;
    let mut data_index = 1usize;
    let read_bits = |n: u32, data_val: &mut u32, data_pos: &mut u32, data_index: &mut usize| -> CodecResult<u32> {
        let mut bits = 0u32;
        let mut power = 1u32;
        for _ in 0..n {
            let resb = *data_val & *data_pos;
            *data_pos >>= 1;
            if *data_pos == 0 {
                *data_pos = reset;
                *data_val = value_at(*data_index)?;
                *data_index += 1;
            }
            if resb > 0 {
                bits |= power;
            }
            power <<= 1;
        }
        Ok(bits)
    };

    let mut dictionary: Vec<Vec<u16>> = vec![Vec::new(), Vec::new(), Vec::new()];
    let mut enlarge_in: u32 = 4;
    let mut num_bits: u32 = 3;
    let first = read_bits(2, &mut data_val, &mut data_pos, &mut data_index)?;
    let c: Vec<u16> = match first {
        0 => vec![read_bits(8, &mut data_val, &mut data_pos, &mut data_index)? as u16],
        1 => vec![read_bits(16, &mut data_val, &mut data_pos, &mut data_index)? as u16],
        2 => return Ok(Vec::new()),
        _ => return Err(lz_err("invalid stream start")),
    };
    dictionary.push(c.clone());
    let mut w = c.clone();
    let mut result: Vec<u16> = c;
    loop {
        if data_index > length {
            return Err(lz_err("truncated stream (no end marker)"));
        }
        let mut code = read_bits(num_bits, &mut data_val, &mut data_pos, &mut data_index)? as usize;
        match code {
            0 | 1 => {
                let n = if code == 0 { 8 } else { 16 };
                let ch = read_bits(n, &mut data_val, &mut data_pos, &mut data_index)? as u16;
                dictionary.push(vec![ch]);
                code = dictionary.len() - 1;
                enlarge_in = enlarge_in.saturating_sub(1);
            }
            2 => return Ok(result),
            _ => {}
        }
        if enlarge_in == 0 {
            enlarge_in = 1 << num_bits;
            num_bits += 1;
            if num_bits > 31 {
                return Err(lz_err("dictionary overflow"));
            }
        }
        let entry: Vec<u16> = if code < dictionary.len() && code > 2 {
            dictionary[code].clone()
        } else if code == dictionary.len() {
            let mut e = w.clone();
            e.push(w[0]);
            e
        } else {
            return Err(lz_err("invalid dictionary reference"));
        };
        result.extend_from_slice(&entry);
        if result.len() > MAX_UNITS {
            return Err(lz_err("decompressed size exceeds the safety limit"));
        }
        let mut nw = w.clone();
        nw.push(entry[0]);
        dictionary.push(nw);
        enlarge_in = enlarge_in.saturating_sub(1);
        w = entry;
        if enlarge_in == 0 {
            enlarge_in = 1 << num_bits;
            num_bits += 1;
        }
    }
}

/// Decompresses lz-string text into a Rust string.
pub fn decompress(input: &[u8], variant: LzVariant) -> CodecResult<String> {
    let units = decompress_units(input, variant)?;
    String::from_utf16(&units).map_err(|_| lz_err("decompressed text is not valid UTF-16"))
}

/// LZString layer.
#[derive(Debug, Clone)]
pub struct LzStringLayer {
    variant: LzVariant,
    /// Whitespace after the text (e.g. a trailing newline) kept verbatim.
    trailing: Vec<u8>,
    exact: bool,
    identity: Identity,
}

impl LzStringLayer {
    /// Detects lz-string text. Requires a minimum length and an alphabet
    /// match; the pipeline only keeps the layer when the decompressed text is
    /// recognised.
    pub fn detect(data: &[u8]) -> Option<(LzStringLayer, Vec<u8>)> {
        let end = data.iter().rposition(|b| !b.is_ascii_whitespace())? + 1;
        let body = &data[..end];
        if body.len() < 8 {
            return None;
        }
        let variant = if body.iter().all(|b| BASE64_ALPHABET.contains(b)) {
            LzVariant::Base64
        } else if body.iter().all(|b| URI_ALPHABET.contains(b)) {
            LzVariant::Uri
        } else {
            return None;
        };
        let text = decompress(body, variant).ok()?;
        if text.is_empty() {
            return None;
        }
        let inner = text.into_bytes();
        let exact = std::str::from_utf8(&inner).map(|t| compress(t, variant) == body).unwrap_or(false);
        Some((
            LzStringLayer { variant, trailing: data[end..].to_vec(), exact, identity: Identity::new(data, &inner) },
            inner,
        ))
    }
}

impl Layer for LzStringLayer {
    fn descriptor(&self) -> LayerDescriptor {
        LayerDescriptor::Wrapper { name: "LZString".into(), detail: self.variant.name().into(), exact: self.exact }
    }

    fn wrap(&self, inner: &[u8]) -> CodecResult<Vec<u8>> {
        if let Some(outer) = self.identity.lookup(inner) {
            return Ok(outer);
        }
        let text = std::str::from_utf8(inner).map_err(|_| lz_err("payload is not UTF-8 text"))?;
        let mut out = compress(text, self.variant);
        out.extend_from_slice(&self.trailing);
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

    /// Vectors produced by the reference JavaScript library (lz-string 1.4.4).
    #[test]
    fn matches_the_reference_library_bit_for_bit() {
        let vectors: [(&str, LzVariant, &str); 4] = [
            ("Hello, world", LzVariant::Base64, "BIUwNmD2A0AEDukBOYAmQ==="),
            ("", LzVariant::Base64, "Q==="),
            ("caf\u{e9} \u{1F600} ok", LzVariant::Uri, "MYQwZglwBIvBuAB7UD2BrIA"),
            (
                "{\"gold\":1500,\"actors\":[null,{\"name\":\"Harold\",\"level\":12,\"hp\":450,\"equips\":[1,2,0,3]},{\"name\":\"Therese\",\"level\":11}],\"party\":[1,2],\"switches\":[true,false,false,true,false,false,true,false,false,true,false,false,true,false,false,true,false,false,true,false,false,true,false,false,true,false,false,true,false,false,true,false,false,true,false,false,true,false,false,true],\"map\":{\"id\":7,\"x\":12,\"y\":33}}",
                LzVariant::Base64,
                "N4Ig5g9gNgJiBcBGArABlQGhAQwMYBcIAnAZwQG0A7AVyig1EuwFsBTBEACWyOjiyisAbqygJEAJiwALAA4IALGiysAjtQCWssvHKIMUzAGYAugF8GIJmw4AVaayKsS7AcNHjEZk1lk98AJ4U+hI+ICQA7hr4uA465PhE1KwYAGbYUC5pGVmJydmZKemFGHlFOeUlZQVZxblJlbUVpQ01jSnVde0t+V1tPd19nc1DrX2jvSPNwyUTrGHM2PLwoBpw8ADsWAAe4lIgQfBGRmZmQA=",
            ),
        ];
        for (text, v, expected) in vectors {
            assert_eq!(String::from_utf8(compress(text, v)).unwrap(), expected, "{text}");
            if !text.is_empty() {
                assert_eq!(decompress(expected.as_bytes(), v).unwrap(), text);
            }
        }
        assert_eq!(
            String::from_utf8(compress(&"tobeornottobe".repeat(20), LzVariant::Uri)).unwrap(),
            "C4ewRgpiBOB2LFJJU4JSm9HgptOyuW6u+2GxBlkJheVFZjpRt1z7Tbq3DXrfr0HkRLeqIniaw+kA"
        );
    }

    #[test]
    fn roundtrip_texts_including_non_bmp_and_repetition() {
        for text in [
            "a",
            "{\"gold\":1500,\"party\":[1,2,3],\"name\":\"Hero\"}",
            &"tobeornottobe".repeat(200),
            "caf\u{e9} \u{1F600} \u{4e2d}\u{6587} ok",
        ] {
            for v in [LzVariant::Base64, LzVariant::Uri] {
                let c = compress(text, v);
                assert_eq!(decompress(&c, v).unwrap(), text, "{v:?}");
            }
        }
    }

    #[test]
    fn layer_detects_edits_and_reencodes() {
        let json = "{\"gold\":100,\"actors\":[{\"name\":\"Harold\",\"level\":5}]}";
        let mut file = compress(json, LzVariant::Base64);
        file.push(b'\n');
        let (layer, inner) = LzStringLayer::detect(&file).unwrap();
        assert_eq!(inner, json.as_bytes());
        assert!(layer.exact);
        assert_eq!(layer.wrap(&inner).unwrap(), file);
        let edited = json.replace("100", "99999");
        let out = layer.wrap(edited.as_bytes()).unwrap();
        assert!(out.ends_with(b"\n"));
        assert_eq!(LzStringLayer::detect(&out).unwrap().1, edited.as_bytes());
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(LzStringLayer::detect(b"not lz string at all!!").is_none());
        assert!(
            decompress(b"AAAA", LzVariant::Base64).is_err()
                || decompress(b"AAAA", LzVariant::Base64).unwrap().is_empty()
        );
        assert!(decompress(b"%%%%", LzVariant::Base64).is_err());
    }
}
