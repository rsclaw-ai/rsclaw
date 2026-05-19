//! PII-safe logging helper. Replaces user content / file paths / URLs
//! with the first 8 hex chars of their sha256 so logs stay correlatable
//! across calls without leaking the underlying value.
//!
//! Use at every `tracing::info!` / `warn!` / `error!` site that would
//! otherwise carry user data:
//!
//! ```ignore
//! tracing::info!(doc = %redact(&doc.title), "ingested");
//! ```

use sha2::{Digest, Sha256};

/// First 8 hex chars of sha256(input). Stable across processes,
/// non-reversible.
pub fn redact(input: impl AsRef<str>) -> String {
    let mut h = Sha256::new();
    h.update(input.as_ref().as_bytes());
    let d = h.finalize();
    let mut s = String::with_capacity(8);
    for b in d.iter().take(4) {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        assert_eq!(redact("hello"), redact("hello"));
    }

    #[test]
    fn differs_for_different_inputs() {
        assert_ne!(redact("a"), redact("b"));
    }

    #[test]
    fn always_eight_hex_chars() {
        for s in ["", "x", "longer string with spaces", "中文也行"] {
            let r = redact(s);
            assert_eq!(r.len(), 8, "input {s:?} → {r}");
            assert!(r.chars().all(|c| c.is_ascii_hexdigit()), "input {s:?} → {r}");
        }
    }

    #[test]
    fn accepts_owned_and_borrowed() {
        let owned: String = "x".into();
        let borrowed: &str = "x";
        assert_eq!(redact(&owned), redact(borrowed));
    }
}
