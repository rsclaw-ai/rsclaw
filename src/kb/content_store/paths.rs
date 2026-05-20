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

/// Stable, content-addressed path for a doc's canonical markdown.
///
/// Layout: `md/<kind>/<slug>--<lsid8>--<md8>.md` where:
///   - `lsid8` = first 8 hex chars of `sha256(logical_source_id)`.
///   - `md8`  = first 8 hex chars of `body_sha256` (the canonical markdown body, post-canonicalize).
///
/// Path semantics:
///   - Re-ingesting the same source with the same content → same
///     `lsid8` AND same `md8` → same path → `write_if_new` returns
///     `false` and stage_doc reuses the file (idempotent).
///   - Same source with NEW content (v2 ingest under a stable
///     `logical_source_id_seed`) → same `lsid8`, different `md8` →
///     DIFFERENT path. Both versions coexist on disk; the old file
///     becomes a Week 4 compactor candidate once retrieval points at
///     the new version.
///   - Two different sources that slugify to the same prefix →
///     different `lsid8` → distinct files.
///
/// Suffix collision math: 32 bits each, combined 64 bits → birthday
/// collision risk is ~1 in 2^32 at ~4B docs in the same kind.
/// stage_doc still verifies body equality on `false` returns so any
/// real collision surfaces as a hard error, not silent data loss.
pub fn markdown_rel_path(
    kind: KbSourceKind,
    slug: &str,
    logical_source_id: &str,
    body_sha256_hex: &str,
) -> String {
    let lsid8 = lsid_hash8(logical_source_id);
    let md8: String = body_sha256_hex.chars().take(8).collect();
    format!("md/{}/{}--{lsid8}--{md8}.md", kind.as_str(), slug)
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
    fn markdown_rel_per_kind_carries_suffixes() {
        let body_sha = "deadbeef00000000000000000000000000000000000000000000000000000000";
        let p = markdown_rel_path(KbSourceKind::Doc, "蒙牛", "file:sha256:abc", body_sha);
        assert!(p.starts_with("md/doc/蒙牛--"), "got {p}");
        assert!(p.ends_with(".md"), "got {p}");
        let suffix = p.trim_start_matches("md/doc/蒙牛--").trim_end_matches(".md");
        // Now `--{lsid8}--{md8}`, so 8 + 2 + 8 = 18 chars.
        assert_eq!(suffix.len(), 18, "lsid+md suffix must be 18 chars, got {suffix}");

        let q = markdown_rel_path(KbSourceKind::Url, "x", "url:https://example.com/p", body_sha);
        assert!(q.starts_with("md/url/x--") && q.ends_with(".md"), "got {q}");
    }

    #[test]
    fn markdown_rel_same_lsid_and_body_same_path_idempotent() {
        let body_sha = "cafef00d00000000000000000000000000000000000000000000000000000000";
        let a = markdown_rel_path(KbSourceKind::Doc, "report", "file:sha256:abc", body_sha);
        let b = markdown_rel_path(KbSourceKind::Doc, "report", "file:sha256:abc", body_sha);
        assert_eq!(a, b);
    }

    #[test]
    fn markdown_rel_different_lsid_different_path_no_collision() {
        let body_sha = "deadbeef00000000000000000000000000000000000000000000000000000000";
        let a = markdown_rel_path(KbSourceKind::Doc, "report", "file:sha256:abc", body_sha);
        let b = markdown_rel_path(KbSourceKind::Doc, "report", "file:sha256:def", body_sha);
        assert_ne!(a, b, "same slug + different lsid must map to different paths");
    }

    #[test]
    fn markdown_rel_same_lsid_different_body_different_path() {
        // v2 ingest under a stable lsid_seed but new content must land
        // at a different file so the old version's chunks/file aren't
        // overwritten.
        let body_a = "aaaaaaaa00000000000000000000000000000000000000000000000000000000";
        let body_b = "bbbbbbbb00000000000000000000000000000000000000000000000000000000";
        let a = markdown_rel_path(KbSourceKind::Doc, "report", "file:sha256:abc", body_a);
        let b = markdown_rel_path(KbSourceKind::Doc, "report", "file:sha256:abc", body_b);
        assert_ne!(a, b, "same lsid + different body must map to different paths");
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
