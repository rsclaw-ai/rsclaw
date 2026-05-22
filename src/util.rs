//! Small shared helpers.

/// Truncate `s` to at most `max_bytes`, snapping DOWN to the nearest UTF-8 char
/// boundary so the returned slice never falls inside a multi-byte character.
///
/// `&s[..n]` PANICS when `n` lands inside a char (e.g. a 3-byte CJK character)
/// or past the end. Use this for every log/error preview of a string that may
/// contain non-ASCII text — `&s[..s.len().min(n)]` is NOT safe for CJK.
pub fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_never_splits_a_cjk_char() {
        // "创建一个完整..." — each Chinese char is 3 UTF-8 bytes. Truncating at
        // byte 200 (the real panic offset) must snap back to a char boundary.
        let s = "创建一个完整的进销存管理系统".repeat(20);
        for max in [0, 1, 2, 3, 4, 50, 80, 199, 200, 500] {
            let t = truncate_str(&s, max);
            assert!(t.len() <= max.min(s.len()) || max == 0);
            assert!(s.starts_with(t)); // valid prefix, no panic, no garbage
        }
    }

    #[test]
    fn truncate_shorter_than_max_is_identity() {
        assert_eq!(truncate_str("hi", 100), "hi");
        assert_eq!(truncate_str("中文", 100), "中文");
    }

    #[test]
    fn truncate_ascii_exact() {
        assert_eq!(truncate_str("hello world", 5), "hello");
    }
}
