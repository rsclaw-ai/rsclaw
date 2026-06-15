//! Approximate token count for chunk sizing.
//!
//! CJK-calibrated: BERT-family Chinese tokenizers (bge-*) emit ~1 token
//! per CJK char, so CJK counts 1:1 and only ASCII/Latin gets the 4-chars
//! heuristic. The previous flat chars/4 was English-calibrated — for
//! Chinese it produced ~2048-char "512-token" chunks, 4x the target:
//! bge-small (512-token input cap) embedded only the first quarter of
//! every chunk, and the oversized Qwen3 chunk vectors turned into hubs
//! that crowded short docs out of dense recall on mixed corpora
//! (benchmarked at 0.5% hit@1 before this fix).

/// Is `c` in a CJK / fullwidth range (counts as one token)?
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x9FFF      // CJK radicals, ideographs, kana, hangul jamo, punct
        | 0xF900..=0xFAFF    // CJK compatibility ideographs
        | 0xFF00..=0xFFEF    // fullwidth forms
        | 0x20000..=0x2FA1F  // CJK extension B+
    )
}

pub fn approx_token_count(text: &str) -> u32 {
    let mut cjk = 0u32;
    let mut other = 0u32;
    for c in text.chars() {
        if is_cjk(c) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.saturating_add(3) / 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear() {
        assert_eq!(approx_token_count(""), 0);
        assert_eq!(approx_token_count("abcd"), 1);
        assert_eq!(approx_token_count("abcde"), 2);
        assert_eq!(approx_token_count(&"x".repeat(400)), 100);
    }

    #[test]
    fn cjk_counts_one_token_per_char() {
        // BERT-family zh tokenizers ≈ 1 token/char: "中文" → 2 tokens.
        assert_eq!(approx_token_count("中文"), 2);
        // Mixed: 4 ASCII (1 token) + 2 CJK (2 tokens).
        assert_eq!(approx_token_count("abcd中文"), 3);
        // Fullwidth punctuation counts as CJK.
        assert_eq!(approx_token_count("，。"), 2);
    }
}
