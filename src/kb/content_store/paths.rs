//! Slug + path helpers for the on-disk content store. Path layout
//! is `md/<kind>/<slug>--<lsid8>.md` and `raw/<doc_id>.<ext>`.
//!
//! The `--<lsid8>` suffix is load-bearing — see `markdown_rel_path`
//! for the idempotency + no-collision rationale.

use crate::kb::model::KbSourceKind;
use sha2::Digest;

/// Slugify a title for path use. Keeps alphanumerics + CJK, lowercases
/// Latin, collapses runs of non-keepers into single dashes, trims
/// trailing dashes, caps to 80 characters.
pub fn slugify(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut prev_dash = false;
    for c in title.chars() {
        let keep = c.is_alphanumeric() || is_cjk(c);
        if keep {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out.chars().take(80).collect()
}

fn is_cjk(c: char) -> bool {
    let cp = c as u32;
    (0x4E00..=0x9FFF).contains(&cp)
        || (0x3040..=0x30FF).contains(&cp)
        || (0xAC00..=0xD7AF).contains(&cp)
}

/// Stable, collision-resistant path for a doc's canonical markdown.
///
/// Layout: `md/<kind>/<slug>--<lsid8>.md` where `lsid8` is the first
/// 8 hex chars of `sha256(logical_source_id)`. The hash suffix is
/// load-bearing:
///   - Re-ingesting the same source → same `lsid8` → same path →
///     `write_if_new` correctly returns `false` and stage_doc reuses
///     the existing file.
///   - Two different sources that slugify to the same prefix (e.g.
///     `蒙牛 报告.md` from two different folders) → different
///     `logical_source_id` → different `lsid8` → distinct files,
///     no collision.
///
/// 8 hex chars = 32 bits = ~4B distinct paths per slug; the
/// birthday-collision risk is ~1 in 2^16 at ~65k same-slug docs,
/// which is acceptable for v1 (and stage_doc still verifies body
/// equality on `false` returns, so a collision surfaces as a hard
/// error, not silent data loss).
pub fn markdown_rel_path(kind: KbSourceKind, slug: &str, logical_source_id: &str) -> String {
    let lsid8 = lsid_hash8(logical_source_id);
    format!("md/{}/{}--{lsid8}.md", kind.as_str(), slug)
}

pub fn lsid_hash8(logical_source_id: &str) -> String {
    let mut h = sha2::Sha256::new();
    h.update(logical_source_id.as_bytes());
    let d = h.finalize();
    let mut s = String::with_capacity(8);
    for b in d.iter().take(4) {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub fn raw_rel_path(doc_id: &str, ext: &str) -> String {
    let ext = ext.trim_start_matches('.');
    if ext.is_empty() {
        format!("raw/{doc_id}")
    } else {
        format!("raw/{doc_id}.{ext}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello World"), "hello-world");
    }

    #[test]
    fn slugify_cjk() {
        assert_eq!(slugify("蒙牛 奶粉 冲泡指南"), "蒙牛-奶粉-冲泡指南");
    }

    #[test]
    fn slugify_max_len() {
        assert!(slugify(&"x".repeat(200)).chars().count() <= 80);
    }

    #[test]
    fn slugify_trims_trailing_dashes() {
        assert_eq!(slugify("hello---"), "hello");
        assert_eq!(slugify("...hello..."), "hello");
    }

    #[test]
    fn markdown_rel_per_kind_carries_lsid_suffix() {
        let p = markdown_rel_path(KbSourceKind::Doc, "蒙牛", "file:sha256:abc");
        assert!(p.starts_with("md/doc/蒙牛--"), "got {p}");
        assert!(p.ends_with(".md"), "got {p}");
        let suffix = p.trim_start_matches("md/doc/蒙牛--").trim_end_matches(".md");
        assert_eq!(suffix.len(), 8, "lsid suffix must be 8 hex chars, got {suffix}");
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()), "got {suffix}");

        let q = markdown_rel_path(KbSourceKind::Url, "x", "url:https://example.com/p");
        assert!(q.starts_with("md/url/x--") && q.ends_with(".md"), "got {q}");
    }

    #[test]
    fn markdown_rel_same_lsid_same_path_idempotent() {
        // Re-ingesting the same source MUST produce the same path so
        // write_if_new can correctly say "already on disk".
        let a = markdown_rel_path(KbSourceKind::Doc, "report", "file:sha256:abc");
        let b = markdown_rel_path(KbSourceKind::Doc, "report", "file:sha256:abc");
        assert_eq!(a, b);
    }

    #[test]
    fn markdown_rel_different_lsid_different_path_no_collision() {
        // Regression: previously `md/doc/<slug>.md` would collide on
        // slug alone, causing the second ingest to either silently
        // overwrite or (with write_if_new) hold a dangling pointer
        // to the first doc's bytes.
        let a = markdown_rel_path(KbSourceKind::Doc, "report", "file:sha256:abc");
        let b = markdown_rel_path(KbSourceKind::Doc, "report", "file:sha256:def");
        assert_ne!(a, b, "same slug + different lsid must map to different paths");
    }

    #[test]
    fn lsid_hash8_is_8_hex_chars() {
        let h = lsid_hash8("file:sha256:abc");
        assert_eq!(h.len(), 8);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn raw_rel_with_ext() {
        assert_eq!(raw_rel_path("01HXY", "pdf"), "raw/01HXY.pdf");
        assert_eq!(raw_rel_path("01HXY", ""), "raw/01HXY");
        assert_eq!(raw_rel_path("01HXY", ".pdf"), "raw/01HXY.pdf");
    }
}
