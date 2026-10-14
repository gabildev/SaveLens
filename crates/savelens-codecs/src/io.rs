//! Bounds-checked binary reader and writer helpers shared by binary codecs.

use crate::error::{CodecError, CodecResult};

/// A cursor over a byte slice that never panics on short input.
#[derive(Debug, Clone)]
pub struct ByteReader<'a> {
    data: &'a [u8],
    pos: usize,
    codec: &'static str,
}

macro_rules! read_int {
    ($le:ident, $be:ident, $t:ty) => {
        /// Reads a little-endian value.
        pub fn $le(&mut self, ctx: &str) -> CodecResult<$t> {
            let b = self.take(std::mem::size_of::<$t>(), ctx)?;
            let mut arr = [0u8; std::mem::size_of::<$t>()];
            arr.copy_from_slice(b);
            Ok(<$t>::from_le_bytes(arr))
        }
        /// Reads a big-endian value.
        pub fn $be(&mut self, ctx: &str) -> CodecResult<$t> {
            let b = self.take(std::mem::size_of::<$t>(), ctx)?;
            let mut arr = [0u8; std::mem::size_of::<$t>()];
            arr.copy_from_slice(b);
            Ok(<$t>::from_be_bytes(arr))
        }
    };
}

impl<'a> ByteReader<'a> {
    /// New reader at offset 0.
    pub fn new(data: &'a [u8], codec: &'static str) -> Self {
        ByteReader { data, pos: 0, codec }
    }

    /// Current offset.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Moves to an absolute offset (must be within bounds).
    pub fn seek(&mut self, pos: usize) -> CodecResult<()> {
        if pos > self.data.len() {
            return Err(self.truncated("seek", pos - self.data.len()));
        }
        self.pos = pos;
        Ok(())
    }

    /// Remaining bytes.
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// True at end of input.
    pub fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    /// Underlying data.
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// Unread tail.
    pub fn rest(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    /// Codec name used in errors.
    pub fn codec(&self) -> &'static str {
        self.codec
    }

    fn truncated(&self, ctx: &str, needed: usize) -> CodecError {
        CodecError::Truncated { codec: self.codec, context: ctx.to_string(), offset: self.pos, needed }
    }

    /// Builds a malformed-data error at the current offset.
    pub fn malformed(&self, reason: impl Into<String>) -> CodecError {
        CodecError::malformed(self.codec, self.pos, reason)
    }

    /// Takes `n` bytes.
    pub fn take(&mut self, n: usize, ctx: &str) -> CodecResult<&'a [u8]> {
        if self.remaining() < n {
            return Err(self.truncated(ctx, n - self.remaining()));
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// Peeks `n` bytes without consuming them.
    pub fn peek(&self, n: usize) -> Option<&'a [u8]> {
        self.data.get(self.pos..self.pos + n)
    }

    /// Reads one byte.
    pub fn u8(&mut self, ctx: &str) -> CodecResult<u8> {
        Ok(self.take(1, ctx)?[0])
    }

    /// Reads one signed byte.
    pub fn i8(&mut self, ctx: &str) -> CodecResult<i8> {
        Ok(self.take(1, ctx)?[0] as i8)
    }

    read_int!(u16_le, u16_be, u16);
    read_int!(i16_le, i16_be, i16);
    read_int!(u32_le, u32_be, u32);
    read_int!(i32_le, i32_be, i32);
    read_int!(u64_le, u64_be, u64);
    read_int!(i64_le, i64_be, i64);
    read_int!(f32_le, f32_be, f32);
    read_int!(f64_le, f64_be, f64);

    /// Reads a fixed array.
    pub fn array<const N: usize>(&mut self, ctx: &str) -> CodecResult<[u8; N]> {
        let b = self.take(N, ctx)?;
        let mut arr = [0u8; N];
        arr.copy_from_slice(b);
        Ok(arr)
    }

    /// Checks that a declared element count can possibly fit in the remaining
    /// input (each element needs at least `min_elem_size` bytes). Prevents
    /// huge allocations from corrupt counts.
    pub fn check_count(&self, count: usize, min_elem_size: usize, ctx: &str) -> CodecResult<()> {
        let need = count.saturating_mul(min_elem_size.max(1));
        if need > self.remaining() {
            return Err(self.malformed(format!(
                "{ctx}: declared count {count} needs at least {need} bytes but only {} remain",
                self.remaining()
            )));
        }
        Ok(())
    }
}

/// Little/big-endian write helpers on `Vec<u8>`.
pub trait WriteExt {
    fn put_u8(&mut self, v: u8);
    fn put_u16_le(&mut self, v: u16);
    fn put_u32_le(&mut self, v: u32);
    fn put_i32_le(&mut self, v: i32);
    fn put_u64_le(&mut self, v: u64);
    fn put_i64_le(&mut self, v: i64);
    fn put_u16_be(&mut self, v: u16);
    fn put_u32_be(&mut self, v: u32);
    fn put_u64_be(&mut self, v: u64);
}

impl WriteExt for Vec<u8> {
    fn put_u8(&mut self, v: u8) {
        self.push(v);
    }
    fn put_u16_le(&mut self, v: u16) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    fn put_u32_le(&mut self, v: u32) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    fn put_i32_le(&mut self, v: i32) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    fn put_u64_le(&mut self, v: u64) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    fn put_i64_le(&mut self, v: i64) {
        self.extend_from_slice(&v.to_le_bytes());
    }
    fn put_u16_be(&mut self, v: u16) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn put_u32_be(&mut self, v: u32) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn put_u64_be(&mut self, v: u64) {
        self.extend_from_slice(&v.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_reports_truncation() {
        let data = [1u8, 0, 0, 0, 0xff];
        let mut r = ByteReader::new(&data, "test");
        assert_eq!(r.u32_le("x").unwrap(), 1);
        assert_eq!(r.u8("y").unwrap(), 0xff);
        let err = r.u16_le("z").unwrap_err();
        assert!(matches!(err, CodecError::Truncated { needed: 2, .. }));
    }

    #[test]
    fn count_guard() {
        let data = [0u8; 8];
        let r = ByteReader::new(&data, "test");
        assert!(r.check_count(2, 4, "c").is_ok());
        assert!(r.check_count(3, 4, "c").is_err());
        assert!(r.check_count(usize::MAX, 4, "c").is_err());
    }
}
