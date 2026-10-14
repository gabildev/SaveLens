//! Chunk files: FastCDC content-defined chunking, zstd compression, blake3
//! addressing, atomic writes and verified reads.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{CasError, CasResult};

/// FastCDC parameters (bytes).
pub const MIN_CHUNK: u32 = 2 * 1024;
pub const AVG_CHUNK: u32 = 8 * 1024;
pub const MAX_CHUNK: u32 = 64 * 1024;
/// zstd level for chunk files.
pub const ZSTD_LEVEL: i32 = 3;

/// A chunk boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkSpan {
    pub offset: usize,
    pub length: usize,
}

/// Splits data into content-defined chunks.
pub fn split(data: &[u8]) -> Vec<ChunkSpan> {
    if data.is_empty() {
        return Vec::new();
    }
    fastcdc::v2020::FastCDC::new(data, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK)
        .map(|c| ChunkSpan { offset: c.offset, length: c.length })
        .collect()
}

/// blake3 hex digest.
pub fn hash_hex(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

/// On-disk chunk directory.
#[derive(Debug, Clone)]
pub struct ChunkDir {
    root: PathBuf,
    staging: PathBuf,
}

impl ChunkDir {
    /// Opens (creating) the chunk directory.
    pub fn open(root: &Path, staging: &Path) -> CasResult<ChunkDir> {
        fs::create_dir_all(root).map_err(|e| CasError::io(root, e))?;
        fs::create_dir_all(staging).map_err(|e| CasError::io(staging, e))?;
        Ok(ChunkDir { root: root.to_path_buf(), staging: staging.to_path_buf() })
    }

    /// Path of a chunk file.
    pub fn path_of(&self, hash: &str) -> PathBuf {
        let prefix = hash.get(..2).unwrap_or("00");
        self.root.join(prefix).join(format!("{hash}.zst"))
    }

    /// True if the chunk file exists.
    pub fn exists(&self, hash: &str) -> bool {
        self.path_of(hash).is_file()
    }

    /// Writes a chunk (idempotent). Returns the stored (compressed) size.
    pub fn write(&self, hash: &str, data: &[u8]) -> CasResult<u64> {
        let dest = self.path_of(hash);
        if let Ok(meta) = fs::metadata(&dest) {
            return Ok(meta.len());
        }
        let compressed = zstd::bulk::compress(data, ZSTD_LEVEL).map_err(|e| CasError::io(&dest, e))?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| CasError::io(parent, e))?;
        }
        let tmp = self.staging.join(format!("chunk-{hash}-{}.tmp", std::process::id()));
        {
            let mut f = fs::File::create(&tmp).map_err(|e| CasError::io(&tmp, e))?;
            f.write_all(&compressed).map_err(|e| CasError::io(&tmp, e))?;
            f.sync_all().map_err(|e| CasError::io(&tmp, e))?;
        }
        fs::rename(&tmp, &dest).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            CasError::io(&dest, e)
        })?;
        Ok(compressed.len() as u64)
    }

    /// Reads and verifies a chunk.
    pub fn read(&self, hash: &str, expected_len: usize) -> CasResult<Vec<u8>> {
        let path = self.path_of(hash);
        let compressed = fs::read(&path).map_err(|e| CasError::io(&path, e))?;
        let data = zstd::bulk::decompress(&compressed, expected_len.max(1))
            .map_err(|e| CasError::Integrity(format!("chunk {hash} cannot be decompressed: {e}")))?;
        if data.len() != expected_len {
            return Err(CasError::Integrity(format!("chunk {hash} has {} bytes, expected {expected_len}", data.len())));
        }
        let actual = hash_hex(&data);
        if actual != hash {
            return Err(CasError::Integrity(format!("chunk {hash} content hash is {actual}")));
        }
        Ok(data)
    }

    /// Deletes a chunk file (missing files are fine).
    pub fn remove(&self, hash: &str) -> CasResult<()> {
        let p = self.path_of(hash);
        match fs::remove_file(&p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CasError::io(p, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_covers_input_and_is_content_defined() {
        let mut data: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        let spans = split(&data);
        assert!(spans.len() > 4);
        let mut pos = 0;
        for s in &spans {
            assert_eq!(s.offset, pos);
            pos += s.length;
        }
        assert_eq!(pos, data.len());
        // Inserting bytes at the front only changes the first chunks.
        let before: Vec<String> = spans.iter().map(|s| hash_hex(&data[s.offset..s.offset + s.length])).collect();
        data.splice(0..0, [1u8, 2, 3, 4, 5]);
        let spans2 = split(&data);
        let after: Vec<String> = spans2.iter().map(|s| hash_hex(&data[s.offset..s.offset + s.length])).collect();
        let shared = before.iter().filter(|h| after.contains(h)).count();
        assert!(shared >= before.len() - 2, "only {shared}/{} chunks shared", before.len());
        assert!(split(&[]).is_empty());
    }

    #[test]
    fn write_read_verify() {
        let dir = tempfile::tempdir().unwrap();
        let cd = ChunkDir::open(&dir.path().join("chunks"), &dir.path().join("staging")).unwrap();
        let data = b"hello chunk".repeat(100);
        let h = hash_hex(&data);
        cd.write(&h, &data).unwrap();
        assert!(cd.exists(&h));
        assert_eq!(cd.read(&h, data.len()).unwrap(), data);
        // Corrupt the file.
        let p = cd.path_of(&h);
        fs::write(&p, zstd::bulk::compress(b"tampered", 3).unwrap()).unwrap();
        assert!(matches!(cd.read(&h, data.len()), Err(CasError::Integrity(_))));
        cd.remove(&h).unwrap();
        cd.remove(&h).unwrap();
        assert!(!cd.exists(&h));
    }
}
