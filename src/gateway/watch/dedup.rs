//! Source-string dedup key.

/// Dedup key for the watch registry: `(channel, peer, normalized_source)`.
pub type DedupKey = (String, String, String);

/// Collapse all runs of whitespace (space, tab, CR, LF) into a single space,
/// trim leading/trailing whitespace. Used **only** for the dedup HashMap key;
/// the source is executed with the original (untouched) string so that
/// quoted-internal-whitespace is preserved at execution time.
pub fn normalize_source(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Build the `(channel, peer, normalize(source))` triple used as the
/// HashMap key in WatchRegistry.
pub fn dedup_key(channel: &str, peer: &str, source: &str) -> DedupKey {
    (channel.to_owned(), peer.to_owned(), normalize_source(source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_runs_of_whitespace() {
        assert_eq!(normalize_source("tail -f x"), "tail -f x");
        assert_eq!(normalize_source("tail  -f  x"), "tail -f x");
        assert_eq!(normalize_source("tail\t-f\tx"), "tail -f x");
        assert_eq!(normalize_source("  tail -f x  "), "tail -f x");
        assert_eq!(normalize_source("tail\n-f\nx"), "tail -f x");
    }

    #[test]
    fn normalize_collapses_quoted_internal_spaces_too() {
        // This is OK — quoted preservation is the executor's job (we keep the
        // original `source` field for execution); normalization is only for dedup.
        assert_eq!(normalize_source(r#"echo "a  b""#), r#"echo "a b""#);
    }

    #[test]
    fn normalize_empty_input() {
        assert_eq!(normalize_source(""), "");
        assert_eq!(normalize_source("   "), "");
    }

    #[test]
    fn dedup_key_differs_by_channel() {
        let a = dedup_key("feishu", "u1", "tail -f x");
        let b = dedup_key("wechat", "u1", "tail -f x");
        assert_ne!(a, b);
    }

    #[test]
    fn dedup_key_differs_by_peer() {
        let a = dedup_key("feishu", "u1", "tail -f x");
        let b = dedup_key("feishu", "u2", "tail -f x");
        assert_ne!(a, b);
    }

    #[test]
    fn dedup_key_normalizes_whitespace_in_source() {
        let a = dedup_key("feishu", "u1", "tail -f x");
        let b = dedup_key("feishu", "u1", "tail  -f  x");
        assert_eq!(a, b);
    }
}
