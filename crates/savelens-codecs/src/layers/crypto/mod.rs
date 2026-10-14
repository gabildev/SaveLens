//! Auto-Crypt layer: transparent decryption / re-encryption using the
//! profiles of `game_keys.toml`, plus single-byte XOR brute forcing.

pub mod cipher;
pub mod keys;

use savelens_core::{CryptoScheme, LayerDescriptor};

use self::cipher::{
    aes_decrypt, aes_encrypt, es3_decrypt, es3_encrypt, xor_decrypt, xor_encrypt, Padding, RollingMode,
};
use self::keys::{IvSource, KeyProfile, SchemeName};
use super::{Identity, Layer};
use crate::error::{CodecError, CodecResult};

/// Parameters recovered while decrypting.
#[derive(Debug, Clone)]
enum CipherState {
    Aes { mode: cipher::AesMode, key: Vec<u8>, iv: [u8; 16], iv_prefix: bool, padding: Padding },
    Xor { key: Vec<u8>, rolling: RollingMode },
    Es3 { password: Vec<u8>, iv: [u8; 16] },
}

/// Crypto layer.
#[derive(Debug, Clone)]
pub struct CryptoLayer {
    profile_name: String,
    scheme: CryptoScheme,
    state: CipherState,
    identity: Identity,
}

impl CryptoLayer {
    /// Decrypts `data` with `profile`.
    pub fn open(profile: &KeyProfile, data: &[u8]) -> CodecResult<(CryptoLayer, Vec<u8>)> {
        let (state, plain) = match profile.scheme {
            SchemeName::Es3 => {
                let password = profile.password_bytes()?;
                let (iv, plain) = es3_decrypt(&password, data)?;
                (CipherState::Es3 { password, iv }, plain)
            }
            SchemeName::Xor | SchemeName::XorRolling => {
                let key = profile.key_bytes()?;
                let rolling = if profile.scheme == SchemeName::Xor { RollingMode::Fixed } else { profile.rolling };
                let plain = xor_decrypt(&key, rolling, data)?;
                (CipherState::Xor { key, rolling }, plain)
            }
            s => {
                let (mode, _) = s
                    .aes()
                    .ok_or_else(|| CodecError::KeyDatabase(format!("profile `{}`: unknown scheme", profile.name)))?;
                let key = profile.key_bytes()?;
                let (iv, iv_prefix, body) = match profile.iv {
                    IvSource::Prefix => {
                        if data.len() < 16 {
                            return Err(CodecError::Crypto {
                                scheme: format!("{s:?}"),
                                reason: "payload shorter than the IV prefix".into(),
                            });
                        }
                        let mut iv = [0u8; 16];
                        iv.copy_from_slice(&data[..16]);
                        (iv, true, &data[16..])
                    }
                    _ => (profile.fixed_iv()?, false, data),
                };
                let plain = aes_decrypt(mode, &key, &iv, profile.padding, body)?;
                (CipherState::Aes { mode, key, iv, iv_prefix, padding: profile.padding }, plain)
            }
        };
        Ok((
            CryptoLayer {
                profile_name: profile.name.clone(),
                scheme: profile.scheme.descriptor(),
                state,
                identity: Identity::new(data, &plain),
            },
            plain,
        ))
    }

    /// Single-byte XOR with a brute-forced key.
    pub fn open_single_byte_xor(key: u8, data: &[u8]) -> (CryptoLayer, Vec<u8>) {
        let plain: Vec<u8> = data.iter().map(|b| b ^ key).collect();
        (
            CryptoLayer {
                profile_name: format!("auto-xor-{key:#04x}"),
                scheme: CryptoScheme::XorFixed,
                state: CipherState::Xor { key: vec![key], rolling: RollingMode::Fixed },
                identity: Identity::new(data, &plain),
            },
            plain,
        )
    }

    /// Name of the profile used.
    pub fn profile_name(&self) -> &str {
        &self.profile_name
    }
}

impl Layer for CryptoLayer {
    fn descriptor(&self) -> LayerDescriptor {
        LayerDescriptor::Crypto { scheme: self.scheme, profile: self.profile_name.clone() }
    }

    fn wrap(&self, inner: &[u8]) -> CodecResult<Vec<u8>> {
        if let Some(outer) = self.identity.lookup(inner) {
            return Ok(outer);
        }
        match &self.state {
            CipherState::Es3 { password, iv } => es3_encrypt(password, iv, inner),
            CipherState::Xor { key, rolling } => xor_encrypt(key, *rolling, inner),
            CipherState::Aes { mode, key, iv, iv_prefix, padding } => {
                let ct = aes_encrypt(*mode, key, iv, *padding, inner)?;
                if *iv_prefix {
                    let mut out = iv.to_vec();
                    out.extend_from_slice(&ct);
                    Ok(out)
                } else {
                    Ok(ct)
                }
            }
        }
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
    use super::cipher::AesMode;
    use super::*;

    fn profile(toml_text: &str) -> KeyProfile {
        keys::KeyDatabase::from_toml(toml_text, "t").unwrap().profiles()[0].clone()
    }

    #[test]
    fn aes_cbc_prefix_iv_roundtrip_and_edit() {
        let p = profile(
            r#"
            [[profile]]
            name = "t"
            scheme = "aes-256-cbc"
            key_hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
        "#,
        );
        let iv = [0x42u8; 16];
        let plain = b"{\"gold\": 100}".to_vec();
        let mut file = iv.to_vec();
        file.extend(aes_encrypt(AesMode::Cbc, &p.key_bytes().unwrap(), &iv, Padding::Pkcs7, &plain).unwrap());
        let (layer, inner) = CryptoLayer::open(&p, &file).unwrap();
        assert_eq!(inner, plain);
        assert_eq!(layer.wrap(&inner).unwrap(), file);
        let edited = b"{\"gold\": 999999}".to_vec();
        let out = layer.wrap(&edited).unwrap();
        assert_eq!(&out[..16], &iv);
        let (_, back) = CryptoLayer::open(&p, &out).unwrap();
        assert_eq!(back, edited);
    }

    #[test]
    fn es3_profile() {
        let p = profile(
            r#"
            [[profile]]
            name = "t"
            scheme = "es3"
            password = "secret"
        "#,
        );
        let file = es3_encrypt(b"secret", &[5; 16], b"{\"a\":1}").unwrap();
        let (layer, inner) = CryptoLayer::open(&p, &file).unwrap();
        assert_eq!(inner, b"{\"a\":1}");
        assert_eq!(layer.wrap(&inner).unwrap(), file);
        assert!(matches!(layer.descriptor(), LayerDescriptor::Crypto { scheme: CryptoScheme::Es3, .. }));
    }

    #[test]
    fn xor_rolling_profile() {
        let p = profile(
            r#"
            [[profile]]
            name = "t"
            scheme = "xor-rolling"
            key_text = "KEY"
            rolling = "index"
        "#,
        );
        let plain = b"<save><gold>5</gold></save>".to_vec();
        let file = xor_encrypt(b"KEY", RollingMode::Index, &plain).unwrap();
        let (layer, inner) = CryptoLayer::open(&p, &file).unwrap();
        assert_eq!(inner, plain);
        let out = layer.wrap(b"<save><gold>6</gold></save>").unwrap();
        assert_eq!(xor_decrypt(b"KEY", RollingMode::Index, &out).unwrap(), b"<save><gold>6</gold></save>");
    }

    #[test]
    fn fixed_iv_ctr() {
        let p = profile(
            r#"
            [[profile]]
            name = "t"
            scheme = "aes-128-ctr"
            key_text = "0123456789abcdef"
            iv = "zero"
        "#,
        );
        let file = aes_encrypt(AesMode::Ctr, b"0123456789abcdef", &[0; 16], Padding::Pkcs7, b"hello world").unwrap();
        let (layer, inner) = CryptoLayer::open(&p, &file).unwrap();
        assert_eq!(inner, b"hello world");
        assert_eq!(layer.wrap(b"hello there").unwrap().len(), 11);
    }

    #[test]
    fn single_byte_xor() {
        let data: Vec<u8> = b"{\"x\":1}".iter().map(|b| b ^ 0x5a).collect();
        let (layer, plain) = CryptoLayer::open_single_byte_xor(0x5a, &data);
        assert_eq!(plain, b"{\"x\":1}");
        assert_eq!(layer.profile_name(), "auto-xor-0x5a");
        assert_eq!(layer.wrap(&plain).unwrap(), data);
    }
}
