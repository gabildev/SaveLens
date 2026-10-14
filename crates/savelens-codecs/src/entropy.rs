//! Shannon entropy and payload classification helpers.

/// Shannon entropy in bits per byte (0.0 ..= 8.0).
pub fn shannon(data: &[u8]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let mut counts = [0u64; 256];
    for &b in data {
        counts[b as usize] += 1;
    }
    let len = data.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// Entropy above which data is considered encrypted or compressed.
pub const HIGH_ENTROPY: f64 = 7.2;

/// True when the data looks encrypted/compressed.
///
/// Small samples cannot reach 8 bits/byte (at most `log2(len)`), so the
/// threshold scales down for inputs shorter than a few hundred bytes.
pub fn looks_random(data: &[u8]) -> bool {
    if data.len() < 32 {
        return false;
    }
    let ceiling = (data.len().min(256) as f64).log2();
    shannon(data) >= HIGH_ENTROPY.min(0.85 * ceiling)
}

/// Fraction of bytes that are printable ASCII or common whitespace.
pub fn printable_ratio(data: &[u8]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let n =
        data.iter().filter(|&&b| (0x20..0x7f).contains(&b) || matches!(b, b'\n' | b'\r' | b'\t') || b >= 0x80).count();
    n as f64 / data.len() as f64
}

/// Human description of an entropy value.
pub fn describe(entropy: f64) -> &'static str {
    if entropy >= 7.5 {
        "very high (encrypted or compressed)"
    } else if entropy >= HIGH_ENTROPY {
        "high (probably encrypted or compressed)"
    } else if entropy >= 5.0 {
        "medium (binary structured data)"
    } else {
        "low (text or sparse binary)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entropy_bounds() {
        assert_eq!(shannon(&[]), 0.0);
        assert_eq!(shannon(&[7; 100]), 0.0);
        let all: Vec<u8> = (0..=255u8).collect();
        assert!((shannon(&all) - 8.0).abs() < 1e-9);
        let text = b"hello hello hello world";
        assert!(shannon(text) < 4.0);
    }

    #[test]
    fn pseudo_random_is_flagged() {
        // xorshift noise
        let mut x = 0x12345678u32;
        let data: Vec<u8> = (0..4096)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x & 0xff) as u8
            })
            .collect();
        assert!(looks_random(&data));
        assert!(!looks_random(b"{\"gold\": 1500, \"name\": \"hero\"} {\"gold\": 1500, \"name\": \"hero\"}"));
        assert_eq!(describe(7.9), "very high (encrypted or compressed)");
    }

    #[test]
    fn printable() {
        assert_eq!(printable_ratio(b"abc"), 1.0);
        assert!(printable_ratio(&[0, 1, 2, b'a']) < 0.5);
    }
}
