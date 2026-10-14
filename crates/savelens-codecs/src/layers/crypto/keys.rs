//! `game_keys.toml`: database of known encryption / obfuscation profiles.

use std::path::{Path, PathBuf};

use savelens_core::CryptoScheme;
use serde::{Deserialize, Serialize};

use super::cipher::{AesMode, Padding, RollingMode};
use crate::document::DetectContext;
use crate::error::{CodecError, CodecResult};

/// The built-in profile file.
pub const BUILTIN_KEYS: &str = include_str!("../../../data/game_keys.toml");

/// Scheme names accepted in `game_keys.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SchemeName {
    #[serde(rename = "aes-128-cbc")]
    Aes128Cbc,
    #[serde(rename = "aes-192-cbc")]
    Aes192Cbc,
    #[serde(rename = "aes-256-cbc")]
    Aes256Cbc,
    #[serde(rename = "aes-128-ecb")]
    Aes128Ecb,
    #[serde(rename = "aes-192-ecb")]
    Aes192Ecb,
    #[serde(rename = "aes-256-ecb")]
    Aes256Ecb,
    #[serde(rename = "aes-128-ctr")]
    Aes128Ctr,
    #[serde(rename = "aes-192-ctr")]
    Aes192Ctr,
    #[serde(rename = "aes-256-ctr")]
    Aes256Ctr,
    #[serde(rename = "xor")]
    Xor,
    #[serde(rename = "xor-rolling")]
    XorRolling,
    #[serde(rename = "es3")]
    Es3,
}

impl SchemeName {
    /// AES mode and key length, if this is an AES scheme.
    pub fn aes(self) -> Option<(AesMode, usize)> {
        Some(match self {
            SchemeName::Aes128Cbc => (AesMode::Cbc, 16),
            SchemeName::Aes192Cbc => (AesMode::Cbc, 24),
            SchemeName::Aes256Cbc => (AesMode::Cbc, 32),
            SchemeName::Aes128Ecb => (AesMode::Ecb, 16),
            SchemeName::Aes192Ecb => (AesMode::Ecb, 24),
            SchemeName::Aes256Ecb => (AesMode::Ecb, 32),
            SchemeName::Aes128Ctr => (AesMode::Ctr, 16),
            SchemeName::Aes192Ctr => (AesMode::Ctr, 24),
            SchemeName::Aes256Ctr => (AesMode::Ctr, 32),
            _ => return None,
        })
    }

    /// Envelope descriptor scheme.
    pub fn descriptor(self) -> CryptoScheme {
        match self {
            SchemeName::Aes128Cbc | SchemeName::Aes192Cbc => CryptoScheme::Aes128Cbc,
            SchemeName::Aes256Cbc => CryptoScheme::Aes256Cbc,
            SchemeName::Aes128Ecb | SchemeName::Aes192Ecb => CryptoScheme::Aes128Ecb,
            SchemeName::Aes256Ecb => CryptoScheme::Aes256Ecb,
            SchemeName::Aes128Ctr | SchemeName::Aes192Ctr => CryptoScheme::Aes128Ctr,
            SchemeName::Aes256Ctr => CryptoScheme::Aes256Ctr,
            SchemeName::Xor => CryptoScheme::XorFixed,
            SchemeName::XorRolling => CryptoScheme::XorRolling,
            SchemeName::Es3 => CryptoScheme::Es3,
        }
    }
}

/// Where the AES IV comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IvSource {
    /// First 16 bytes of the file (kept verbatim on re-encryption).
    #[default]
    Prefix,
    /// All-zero IV.
    Zero,
    /// `iv_hex` field.
    Fixed,
}

/// One profile of `game_keys.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyProfile {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub scheme: SchemeName,
    /// Steam AppIDs this profile applies to.
    #[serde(default)]
    pub app_ids: Vec<u32>,
    /// File-name glob patterns (`*.sav`, `SaveFile.txt`).
    #[serde(default)]
    pub file_patterns: Vec<String>,
    /// Key as hex.
    #[serde(default)]
    pub key_hex: Option<String>,
    /// Key as UTF-8 text.
    #[serde(default)]
    pub key_text: Option<String>,
    /// Password (ES3).
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub iv: IvSource,
    #[serde(default)]
    pub iv_hex: Option<String>,
    #[serde(default)]
    pub padding: Padding,
    #[serde(default)]
    pub rolling: RollingMode,
    /// Try this profile on any high-entropy file.
    #[serde(default)]
    pub generic: bool,
}

impl KeyProfile {
    /// Raw key bytes (`key_hex` or `key_text`).
    pub fn key_bytes(&self) -> CodecResult<Vec<u8>> {
        if let Some(h) = &self.key_hex {
            let cleaned: String = h.chars().filter(|c| !c.is_whitespace()).collect();
            return hex::decode(cleaned)
                .map_err(|e| CodecError::KeyDatabase(format!("profile `{}`: invalid key_hex: {e}", self.name)));
        }
        if let Some(t) = &self.key_text {
            return Ok(t.as_bytes().to_vec());
        }
        Err(CodecError::KeyDatabase(format!("profile `{}` needs key_hex or key_text", self.name)))
    }

    /// Password bytes for ES3.
    pub fn password_bytes(&self) -> CodecResult<Vec<u8>> {
        self.password
            .as_ref()
            .map(|p| p.as_bytes().to_vec())
            .ok_or_else(|| CodecError::KeyDatabase(format!("profile `{}` (es3) needs a password", self.name)))
    }

    /// Fixed IV bytes.
    pub fn fixed_iv(&self) -> CodecResult<[u8; 16]> {
        match self.iv {
            IvSource::Zero => Ok([0; 16]),
            IvSource::Prefix => {
                Err(CodecError::KeyDatabase(format!("profile `{}` takes its IV from the data", self.name)))
            }
            IvSource::Fixed => {
                let h = self.iv_hex.as_ref().ok_or_else(|| {
                    CodecError::KeyDatabase(format!("profile `{}`: iv = \"fixed\" needs iv_hex", self.name))
                })?;
                let v = hex::decode(h)
                    .map_err(|e| CodecError::KeyDatabase(format!("profile `{}`: invalid iv_hex: {e}", self.name)))?;
                v.try_into()
                    .map_err(|_| CodecError::KeyDatabase(format!("profile `{}`: iv_hex must be 16 bytes", self.name)))
            }
        }
    }

    /// Validates the profile fields.
    pub fn validate(&self) -> CodecResult<()> {
        match self.scheme {
            SchemeName::Es3 => {
                self.password_bytes()?;
            }
            SchemeName::Xor | SchemeName::XorRolling => {
                if self.key_bytes()?.is_empty() {
                    return Err(CodecError::KeyDatabase(format!("profile `{}`: empty XOR key", self.name)));
                }
            }
            s => {
                let (_, len) = s
                    .aes()
                    .ok_or_else(|| CodecError::KeyDatabase(format!("profile `{}`: unknown scheme", self.name)))?;
                let key = self.key_bytes()?;
                if key.len() != len {
                    return Err(CodecError::KeyDatabase(format!(
                        "profile `{}`: {:?} needs a {len}-byte key, got {}",
                        self.name,
                        s,
                        key.len()
                    )));
                }
                if self.iv != IvSource::Prefix {
                    self.fixed_iv()?;
                }
            }
        }
        for p in &self.file_patterns {
            glob::Pattern::new(p)
                .map_err(|e| CodecError::KeyDatabase(format!("profile `{}`: bad pattern `{p}`: {e}", self.name)))?;
        }
        Ok(())
    }

    /// True when one of the file patterns matches `name` (case-insensitive).
    pub fn matches_file(&self, name: &str) -> bool {
        self.file_patterns.iter().any(|p| {
            glob::Pattern::new(p)
                .map(|pat| pat.matches_with(name, glob::MatchOptions { case_sensitive: false, ..Default::default() }))
                .unwrap_or(false)
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct KeyFile {
    #[serde(default)]
    profile: Vec<KeyProfile>,
}

/// The profile database.
#[derive(Debug, Clone, Default)]
pub struct KeyDatabase {
    profiles: Vec<KeyProfile>,
    sources: Vec<String>,
}

impl KeyDatabase {
    /// Parses a TOML document.
    pub fn from_toml(text: &str, source: &str) -> CodecResult<KeyDatabase> {
        let file: KeyFile = toml::from_str(text).map_err(|e| CodecError::KeyDatabase(format!("{source}: {e}")))?;
        for p in &file.profile {
            p.validate()?;
        }
        Ok(KeyDatabase { profiles: file.profile, sources: vec![source.to_string()] })
    }

    /// Built-in profiles only.
    pub fn builtin() -> KeyDatabase {
        // The embedded file is validated by unit tests; fall back to empty on
        // the impossible error instead of panicking.
        KeyDatabase::from_toml(BUILTIN_KEYS, "builtin").unwrap_or_default()
    }

    /// Candidate user key files, highest priority first.
    pub fn user_paths() -> Vec<PathBuf> {
        let mut v = Vec::new();
        if let Ok(p) = std::env::var("SAVELENS_KEYS") {
            v.push(PathBuf::from(p));
        }
        if let Ok(home) = std::env::var("SAVELENS_HOME") {
            v.push(Path::new(&home).join("game_keys.toml"));
        }
        if let Some(c) = dirs::config_dir() {
            v.push(c.join("savelens").join("game_keys.toml"));
        }
        v
    }

    /// Built-in profiles plus every existing user file (user profiles first).
    /// Invalid user files are reported, not ignored.
    pub fn load_default() -> CodecResult<KeyDatabase> {
        let mut db = KeyDatabase::default();
        for path in Self::user_paths() {
            if path.is_file() {
                let text = std::fs::read_to_string(&path)?;
                db.merge(KeyDatabase::from_toml(&text, &path.display().to_string())?);
            }
        }
        db.merge(KeyDatabase::builtin());
        Ok(db)
    }

    /// Appends the profiles of `other` (lower priority). Profiles with a name
    /// already present are skipped (the earlier one overrides).
    pub fn merge(&mut self, other: KeyDatabase) {
        for p in other.profiles {
            if !self.profiles.iter().any(|q| q.name == p.name) {
                self.profiles.push(p);
            }
        }
        self.sources.extend(other.sources);
    }

    /// Adds one profile with the highest priority.
    pub fn add_front(&mut self, profile: KeyProfile) -> CodecResult<()> {
        profile.validate()?;
        self.profiles.retain(|p| p.name != profile.name);
        self.profiles.insert(0, profile);
        Ok(())
    }

    /// All profiles.
    pub fn profiles(&self) -> &[KeyProfile] {
        &self.profiles
    }

    /// Files the database was loaded from.
    pub fn sources(&self) -> &[String] {
        &self.sources
    }

    /// Profiles to try for a file, most specific first.
    pub fn candidates(&self, ctx: &DetectContext) -> Vec<&KeyProfile> {
        let mut by_app = Vec::new();
        let mut by_file = Vec::new();
        let mut generic = Vec::new();
        for p in &self.profiles {
            if ctx.app_id.is_some_and(|id| p.app_ids.contains(&id)) {
                by_app.push(p);
            } else if ctx.file_name.as_deref().is_some_and(|n| p.matches_file(n)) {
                by_file.push(p);
            } else if p.generic {
                generic.push(p);
            }
        }
        by_app.into_iter().chain(by_file).chain(generic).collect()
    }

    /// Looks a profile up by name.
    pub fn get(&self, name: &str) -> Option<&KeyProfile> {
        self.profiles.iter().find(|p| p.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_parses() {
        let db = KeyDatabase::from_toml(BUILTIN_KEYS, "builtin").unwrap();
        assert!(db.get("es3-default").is_some());
        assert!(db.get("phasmophobia").is_some());
    }

    #[test]
    fn candidate_order() {
        let db = KeyDatabase::builtin();
        let ctx = DetectContext { file_name: Some("SaveFile.txt".into()), app_id: Some(739630) };
        let c = db.candidates(&ctx);
        assert_eq!(c[0].name, "phasmophobia");
        let ctx = DetectContext::for_file("slot1.es3");
        assert_eq!(db.candidates(&ctx)[0].name, "es3-default");
        let ctx = DetectContext::for_file("other.bin");
        let names: Vec<_> = db.candidates(&ctx).iter().map(|p| p.name.clone()).collect();
        assert_eq!(names, vec!["es3-default"]);
    }

    #[test]
    fn validation_errors() {
        let bad_key = r#"
            [[profile]]
            name = "x"
            scheme = "aes-128-cbc"
            key_hex = "0011"
        "#;
        assert!(KeyDatabase::from_toml(bad_key, "t").is_err());
        let no_pw = r#"
            [[profile]]
            name = "x"
            scheme = "es3"
        "#;
        assert!(KeyDatabase::from_toml(no_pw, "t").is_err());
        let bad_iv = r#"
            [[profile]]
            name = "x"
            scheme = "aes-128-cbc"
            key_text = "0123456789abcdef"
            iv = "fixed"
        "#;
        assert!(KeyDatabase::from_toml(bad_iv, "t").is_err());
        let ok = r#"
            [[profile]]
            name = "x"
            scheme = "xor-rolling"
            key_text = "abc"
            rolling = "cipher-feedback"
        "#;
        assert!(KeyDatabase::from_toml(ok, "t").is_ok());
    }

    #[test]
    fn merge_and_override() {
        let mut db = KeyDatabase::from_toml(
            r#"
            [[profile]]
            name = "es3-default"
            scheme = "es3"
            password = "custom"
            generic = true
        "#,
            "user",
        )
        .unwrap();
        db.merge(KeyDatabase::builtin());
        assert_eq!(db.get("es3-default").unwrap().password.as_deref(), Some("custom"));
        assert!(db.get("phasmophobia").is_some());
    }
}
