//! Approximate token count (4-char heuristic). Week 2 will swap in
//! the actual BGE-M3 tokenizer once the embedder is wired.

pub fn approx_token_count(text: &str) -> u32 {
    let chars = text.chars().count() as u32;
    chars.saturating_add(3) / 4
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
    fn cjk_counts_chars_not_bytes() {
        // "中文" is 6 bytes but 2 chars → 1 approx-token.
        assert_eq!(approx_token_count("中文"), 1);
    }
}
