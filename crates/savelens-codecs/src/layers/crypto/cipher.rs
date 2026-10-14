//! Block / stream cipher primitives: AES (CBC, ECB, CTR with 128/192/256-bit
//! keys), fixed and rolling XOR, and the Unity Easy Save 3 scheme.

use aes::cipher::block_padding::{NoPadding, Pkcs7};
use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyInit, KeyIvInit, StreamCipher};
use serde::{Deserialize, Serialize};

use crate::error::{CodecError, CodecResult};

/// Block cipher padding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Padding {
    /// PKCS#7 (stripped on decrypt, added on encrypt).
    #[default]
    Pkcs7,
    /// No padding: the plaintext must be a multiple of 16 bytes.
    None,
    /// Zero padding: kept inside the payload on decrypt; on encrypt the
    /// payload is zero-padded up to the next block boundary.
    Zero,
}

/// AES chaining mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AesMode {
    Cbc,
    Ecb,
    Ctr,
}

fn crypto_err(scheme: &str, reason: impl Into<String>) -> CodecError {
    CodecError::Crypto {
        scheme: scheme.to_string(),
        reason: reason.into(),
    }
}

macro_rules! with_aes {
    ($key:expr, $scheme:expr, |$Cipher:ident| $body:expr) => {{
        match $key.len() {
            16 => {
                type $Cipher = aes::Aes128;
                $body
            }
            24 => {
                type $Cipher = aes::Aes192;
                $body
            }
            32 => {
                type $Cipher = aes::Aes256;
                $body
            }
            n => Err(crypto_err($scheme, format!("AES key must be 16, 24 or 32 bytes, got {n}"))),
        }
    }};
}

fn zero_pad(data: &[u8]) -> Vec<u8> {
    let mut v = data.to_vec();
    let rem = v.len() % 16;
    if rem != 0 {
        v.resize(v.len() + 16 - rem, 0);
    }
    v
}

/// AES decryption.
pub fn aes_decrypt(mode: AesMode, key: &[u8], iv: &[u8; 16], padding: Padding, data: &[u8]) -> CodecResult<Vec<u8>> {
    let scheme = "AES";
    if mode != AesMode::Ctr && data.len() % 16 != 0 {
        return Err(crypto_err(scheme, format!("ciphertext length {} is not a multiple of 16", data.len())));
    }
    with_aes!(key, scheme, |C| {
        match mode {
            AesMode::Cbc => {
                let dec = cbc::Decryptor::<C>::new_from_slices(key, iv).map_err(|e| crypto_err(scheme, e.to_string()))?;
                match padding {
                    Padding::Pkcs7 => dec
                        .decrypt_padded_vec_mut::<Pkcs7>(data)
                        .map_err(|_| crypto_err(scheme, "invalid PKCS#7 padding (wrong key?)")),
                    Padding::None | Padding::Zero => dec
                        .decrypt_padded_vec_mut::<NoPadding>(data)
                        .map_err(|_| crypto_err(scheme, "decryption failed")),
                }
            }
            AesMode::Ecb => {
                let dec = ecb::Decryptor::<C>::new_from_slice(key).map_err(|e| crypto_err(scheme, e.to_string()))?;
                match padding {
                    Padding::Pkcs7 => dec
                        .decrypt_padded_vec_mut::<Pkcs7>(data)
                        .map_err(|_| crypto_err(scheme, "invalid PKCS#7 padding (wrong key?)")),
                    Padding::None | Padding::Zero => dec
                        .decrypt_padded_vec_mut::<NoPadding>(data)
                        .map_err(|_| crypto_err(scheme, "decryption failed")),
                }
            }
            AesMode::Ctr => {
                let mut c = ctr::Ctr128BE::<C>::new_from_slices(key, iv).map_err(|e| crypto_err(scheme, e.to_string()))?;
                let mut buf = data.to_vec();
                c.apply_keystream(&mut buf);
                Ok(buf)
            }
        }
    })
}

/// AES encryption.
pub fn aes_encrypt(mode: AesMode, key: &[u8], iv: &[u8; 16], padding: Padding, data: &[u8]) -> CodecResult<Vec<u8>> {
    let scheme = "AES";
    let plain: Vec<u8> = match (mode, padding) {
        (AesMode::Ctr, _) | (_, Padding::Pkcs7) => data.to_vec(),
        (_, Padding::Zero) => zero_pad(data),
        (_, Padding::None) => {
            if data.len() % 16 != 0 {
                return Err(crypto_err(
                    scheme,
                    format!("payload of {} bytes is not block aligned and the profile has no padding", data.len()),
                ));
            }
            data.to_vec()
        }
    };
    with_aes!(key, scheme, |C| {
        match mode {
            AesMode::Cbc => {
                let enc = cbc::Encryptor::<C>::new_from_slices(key, iv).map_err(|e| crypto_err(scheme, e.to_string()))?;
                Ok(match padding {
                    Padding::Pkcs7 => enc.encrypt_padded_vec_mut::<Pkcs7>(&plain),
                    Padding::None | Padding::Zero => enc.encrypt_padded_vec_mut::<NoPadding>(&plain),
                })
            }
            AesMode::Ecb => {
                let enc = ecb::Encryptor::<C>::new_from_slice(key).map_err(|e| crypto_err(scheme, e.to_string()))?;
                Ok(match padding {
                    Padding::Pkcs7 => enc.encrypt_padded_vec_mut::<Pkcs7>(&plain),
                    Padding::None | Padding::Zero => enc.encrypt_padded_vec_mut::<NoPadding>(&plain),
                })
            }
            AesMode::Ctr => {
                let mut c = ctr::Ctr128BE::<C>::new_from_slices(key, iv).map_err(|e| crypto_err(scheme, e.to_string()))?;
                let mut buf = plain;
                c.apply_keystream(&mut buf);
                Ok(buf)
            }
        }
    })
}

/// How the XOR key evolves along the data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RollingMode {
    /// `k[i] = key[i % n]` (plain repeating key).
    #[default]
    Fixed,
    /// `k[i] = key[i % n] ^ (i mod 256)`.
    Index,
    /// Cipher feedback: `k[i] = key[i % n] ^ c[i-1]` (c[-1] = 0).
    CipherFeedback,
}

/// XOR decryption.
pub fn xor_decrypt(key: &[u8], mode: RollingMode, data: &[u8]) -> CodecResult<Vec<u8>> {
    if key.is_empty() {
        return Err(crypto_err("XOR", "empty key"));
    }
    let n = key.len();
    Ok(match mode {
        RollingMode::Fixed => data.iter().enumerate().map(|(i, &b)| b ^ key[i % n]).collect(),
        RollingMode::Index => data.iter().enumerate().map(|(i, &b)| b ^ key[i % n] ^ (i as u8)).collect(),
        RollingMode::CipherFeedback => {
            let mut prev = 0u8;
            data.iter()
                .enumerate()
                .map(|(i, &c)| {
                    let p = c ^ key[i % n] ^ prev;
                    prev = c;
                    p
                })
                .collect()
        }
    })
}

/// XOR encryption.
pub fn xor_encrypt(key: &[u8], mode: RollingMode, data: &[u8]) -> CodecResult<Vec<u8>> {
    if key.is_empty() {
        return Err(crypto_err("XOR", "empty key"));
    }
    let n = key.len();
    Ok(match mode {
        RollingMode::Fixed | RollingMode::Index => xor_decrypt(key, mode, data)?,
        RollingMode::CipherFeedback => {
            let mut prev = 0u8;
            data.iter()
                .enumerate()
                .map(|(i, &p)| {
                    let c = p ^ key[i % n] ^ prev;
                    prev = c;
                    c
                })
                .collect()
        }
    })
}

/// Easy Save 3 key derivation: PBKDF2-HMAC-SHA1, 100 iterations, salt = IV.
pub fn es3_key(password: &[u8], iv: &[u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, iv, 100, &mut key);
    key
}

/// Decrypts an Easy Save 3 payload (`IV(16) || AES-128-CBC-PKCS7(ciphertext)`).
pub fn es3_decrypt(password: &[u8], data: &[u8]) -> CodecResult<([u8; 16], Vec<u8>)> {
    if data.len() < 32 || (data.len() - 16) % 16 != 0 {
        return Err(crypto_err("ES3", "payload is not IV + whole AES blocks"));
    }
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&data[..16]);
    let key = es3_key(password, &iv);
    let plain = aes_decrypt(AesMode::Cbc, &key, &iv, Padding::Pkcs7, &data[16..])
        .map_err(|_| crypto_err("ES3", "wrong password (invalid padding)"))?;
    Ok((iv, plain))
}

/// Encrypts an Easy Save 3 payload with a given IV.
pub fn es3_encrypt(password: &[u8], iv: &[u8; 16], plain: &[u8]) -> CodecResult<Vec<u8>> {
    let key = es3_key(password, iv);
    let mut out = iv.to_vec();
    out.extend_from_slice(&aes_encrypt(AesMode::Cbc, &key, iv, Padding::Pkcs7, plain)?);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes128_cbc_nist_vector() {
        // NIST SP 800-38A F.2.1 CBC-AES128.Encrypt, first block.
        let key = hex::decode("2b7e151628aed2a6abf7158809cf4f3c").unwrap();
        let iv: [u8; 16] = hex::decode("000102030405060708090a0b0c0d0e0f").unwrap().try_into().unwrap();
        let pt = hex::decode("6bc1bee22e409f96e93d7e117393172a").unwrap();
        let ct = aes_encrypt(AesMode::Cbc, &key, &iv, Padding::None, &pt).unwrap();
        assert_eq!(hex::encode(&ct), "7649abac8119b246cee98e9b12e9197d");
        assert_eq!(aes_decrypt(AesMode::Cbc, &key, &iv, Padding::None, &ct).unwrap(), pt);
    }

    #[test]
    fn aes128_ecb_nist_vector() {
        let key = hex::decode("2b7e151628aed2a6abf7158809cf4f3c").unwrap();
        let pt = hex::decode("6bc1bee22e409f96e93d7e117393172a").unwrap();
        let ct = aes_encrypt(AesMode::Ecb, &key, &[0; 16], Padding::None, &pt).unwrap();
        assert_eq!(hex::encode(&ct), "3ad77bb40d7a3660a89ecaf32466ef97");
    }

    #[test]
    fn aes128_ctr_nist_vector() {
        let key = hex::decode("2b7e151628aed2a6abf7158809cf4f3c").unwrap();
        let iv: [u8; 16] = hex::decode("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff").unwrap().try_into().unwrap();
        let pt = hex::decode("6bc1bee22e409f96e93d7e117393172a").unwrap();
        let ct = aes_encrypt(AesMode::Ctr, &key, &iv, Padding::None, &pt).unwrap();
        assert_eq!(hex::encode(&ct), "874d6191b620e3261bef6864990db6ce");
    }

    #[test]
    fn aes256_cbc_pkcs7_roundtrip_and_wrong_key() {
        let key = [7u8; 32];
        let iv = [9u8; 16];
        let pt = b"{\"gold\":1500}".to_vec();
        let ct = aes_encrypt(AesMode::Cbc, &key, &iv, Padding::Pkcs7, &pt).unwrap();
        assert_eq!(ct.len(), 16);
        assert_eq!(aes_decrypt(AesMode::Cbc, &key, &iv, Padding::Pkcs7, &ct).unwrap(), pt);
        // A wrong key almost always yields invalid padding.
        let wrong = aes_decrypt(AesMode::Cbc, &[8u8; 32], &iv, Padding::Pkcs7, &ct);
        assert!(wrong.is_err() || wrong.unwrap() != pt);
        assert!(aes_decrypt(AesMode::Cbc, &[1u8; 15], &iv, Padding::Pkcs7, &ct).is_err());
    }

    #[test]
    fn zero_padding() {
        let key = [1u8; 16];
        let ct = aes_encrypt(AesMode::Ecb, &key, &[0; 16], Padding::Zero, b"abc").unwrap();
        let pt = aes_decrypt(AesMode::Ecb, &key, &[0; 16], Padding::Zero, &ct).unwrap();
        assert_eq!(&pt[..3], b"abc");
        assert!(pt[3..].iter().all(|&b| b == 0));
        assert!(aes_encrypt(AesMode::Ecb, &key, &[0; 16], Padding::None, b"abc").is_err());
    }

    #[test]
    fn xor_modes_roundtrip() {
        let key = b"k3y";
        let data: Vec<u8> = (0..300u32).map(|i| (i * 31) as u8).collect();
        for mode in [RollingMode::Fixed, RollingMode::Index, RollingMode::CipherFeedback] {
            let ct = xor_encrypt(key, mode, &data).unwrap();
            assert_ne!(ct, data);
            assert_eq!(xor_decrypt(key, mode, &ct).unwrap(), data, "{mode:?}");
        }
        assert!(xor_decrypt(b"", RollingMode::Fixed, &data).is_err());
    }

    #[test]
    fn es3_roundtrip() {
        let iv = [3u8; 16];
        let plain = br#"{"Gold":{"__type":"int","value":1500}}"#;
        let enc = es3_encrypt(b"password", &iv, plain).unwrap();
        let (iv2, dec) = es3_decrypt(b"password", &enc).unwrap();
        assert_eq!(iv2, iv);
        assert_eq!(dec, plain);
        assert!(es3_decrypt(b"wrong-password", &enc).map(|(_, d)| d != plain).unwrap_or(true));
    }

    #[test]
    fn pbkdf2_hmac_sha1_rfc6070_vector() {
        // RFC 6070: P="password", S="salt", c=1, dkLen=20.
        let mut out = [0u8; 20];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(b"password", b"salt", 1, &mut out);
        assert_eq!(hex::encode(out), "0c60c80f961f0e71f3a9b524af6012062fe037a6");
    }
}
