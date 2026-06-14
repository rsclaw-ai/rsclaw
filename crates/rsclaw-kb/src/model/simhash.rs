//! SimHash-64 for chunk-level near-duplicate detection. Two chunks
//! whose simhashes differ by ≤ K bits are likely the same content
//! (we use Hamming distance ≤ ~12 in v1, tunable in v2).
//!
//! Algorithm: tokenize on whitespace (Week 1 simple split, Week 2+
//! can swap in jieba for CJK), de-duplicate tokens within the same
//! chunk, hash each with sha256, and sum/subtract per-bit votes
//! across all tokens. Bits where votes ≥ 0 become 1.

use sha2::{Digest, Sha256};

pub fn simhash64(text: &str) -> u64 {
    let mut accum = [0i32; 64];
    let mut seen = std::collections::HashSet::new();
    for tok in text.split_whitespace() {
        if !seen.insert(tok) {
            continue;
        }
        let mut h = Sha256::new();
        h.update(tok.as_bytes());
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&h.finalize()[..8]);
        let bits = u64::from_be_bytes(bytes);
        for i in 0..64 {
            if (bits >> i) & 1 == 1 {
                accum[i] += 1;
            } else {
                accum[i] -= 1;
            }
        }
    }
    let mut out: u64 = 0;
    for i in 0..64 {
        if accum[i] >= 0 {
            out |= 1u64 << i;
        }
    }
    out
}

pub fn hamming64(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_same_hash() {
        assert_eq!(simhash64("hello world"), simhash64("hello world"));
    }

    #[test]
    fn similar_close_hash() {
        let a = simhash64("the quick brown fox jumps over the lazy dog");
        let b = simhash64("the quick brown fox jumps over a lazy dog");
        assert!(
            hamming64(a, b) < 16,
            "expected close hashes, got {}",
            hamming64(a, b)
        );
    }

    #[test]
    fn different_far_hash() {
        let a = simhash64("the quick brown fox");
        let b = simhash64("completely unrelated content here");
        assert!(
            hamming64(a, b) > 16,
            "expected far hashes, got {}",
            hamming64(a, b)
        );
    }

    #[test]
    fn hamming_basic() {
        assert_eq!(hamming64(0, 0), 0);
        assert_eq!(hamming64(0, 0xFF), 8);
        assert_eq!(hamming64(u64::MAX, 0), 64);
    }
}
