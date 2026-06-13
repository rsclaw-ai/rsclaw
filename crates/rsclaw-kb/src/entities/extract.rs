//! Regex-based mention extractor. Returns deterministic, deduplicated
//! `(EntityKind, surface)` tuples for a single chunk's text.
//!
//! Coverage:
//!   - URLs (http/https/ftp): EntityKind::Url
//!   - Emails: EntityKind::Email
//!   - Hashtags (#word): EntityKind::Hashtag
//!   - @-mentions (@handle): EntityKind::Person (best-effort; v2 NER refines)
//!
//! Limits per chunk: ~64 mentions across all kinds (avoids
//! pathological inputs blowing memory).

use std::collections::HashSet;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::kb::model::EntityKind;

const MAX_MENTIONS_PER_CHUNK: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExtractedMention {
    pub kind: EntityKind,
    pub surface: String,
}

static URL_RE: Lazy<Regex> = Lazy::new(|| {
    // Conservative URL match — schemes we know, then a non-greedy tail
    // up to whitespace/quote/paren/bracket boundary.
    Regex::new(r#"(?i)\b(?:https?|ftp)://[^\s<>"'\(\)\[\]]+"#).unwrap()
});
static EMAIL_RE: Lazy<Regex> = Lazy::new(|| {
    // Standard mailbox; intentionally simple (no quoted local part).
    Regex::new(r"(?i)[a-z0-9._%+\-]+@[a-z0-9.\-]+\.[a-z]{2,}").unwrap()
});
static HASHTAG_RE: Lazy<Regex> = Lazy::new(|| {
    // Allow Unicode-word characters after # so Chinese hashtags work.
    Regex::new(r"#[\w一-鿿぀-ヿ]+").unwrap()
});
static MENTION_RE: Lazy<Regex> = Lazy::new(|| {
    // @-mentions: ascii handles (Slack/Twitter-style). Stop before any
    // punctuation so trailing `.` `,` `:` `;` `?` `!` aren't included.
    Regex::new(r"@[A-Za-z0-9_\-]{1,32}").unwrap()
});

pub fn extract_entities(text: &str) -> Vec<ExtractedMention> {
    let mut out: Vec<ExtractedMention> = Vec::new();
    let mut seen: HashSet<(EntityKind, String)> = HashSet::new();
    let push = |kind: EntityKind,
                surface: String,
                out: &mut Vec<ExtractedMention>,
                seen: &mut HashSet<(EntityKind, String)>| {
        if out.len() >= MAX_MENTIONS_PER_CHUNK {
            return;
        }
        let key = (kind, surface.clone());
        if seen.insert(key) {
            out.push(ExtractedMention { kind, surface });
        }
    };

    for m in URL_RE.find_iter(text) {
        push(EntityKind::Url, m.as_str().to_string(), &mut out, &mut seen);
    }
    for m in EMAIL_RE.find_iter(text) {
        push(
            EntityKind::Email,
            m.as_str().to_lowercase(),
            &mut out,
            &mut seen,
        );
    }
    for m in HASHTAG_RE.find_iter(text) {
        // Drop the leading '#' so the surface is the tag body.
        let s = &m.as_str()[1..];
        push(EntityKind::Hashtag, s.to_string(), &mut out, &mut seen);
    }
    for m in MENTION_RE.find_iter(text) {
        let s = &m.as_str()[1..];
        push(EntityKind::Person, s.to_string(), &mut out, &mut seen);
    }
    out
}

/// Produce a stable `canonical_id` for a mention's surface form.
/// Format: `ent_<kind>_<lower-surface-with-non-alnum-stripped>`.
/// Keeps within ASCII for index-key cleanliness even for CJK surfaces
/// (callers can carry the original surface in `KbEntity.surface_forms`).
pub fn canonical_id(kind: EntityKind, surface: &str) -> String {
    use sha2::{Digest, Sha256};
    let kind_tag = match kind {
        EntityKind::Url => "url",
        EntityKind::Email => "email",
        EntityKind::Hashtag => "tag",
        EntityKind::Person => "person",
        EntityKind::Brand => "brand",
        EntityKind::Org => "org",
        EntityKind::Other => "other",
    };
    // Hash to avoid escaping issues + cap key length.
    let mut h = Sha256::new();
    h.update(kind_tag.as_bytes());
    h.update([0u8]);
    h.update(surface.to_lowercase().as_bytes());
    let hex: String = h
        .finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("ent_{kind_tag}_{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_kind<'a>(
        items: &'a [ExtractedMention],
        kind: EntityKind,
        surface: &str,
    ) -> Option<&'a ExtractedMention> {
        items
            .iter()
            .find(|m| m.kind == kind && m.surface == surface)
    }

    #[test]
    fn extracts_urls() {
        let r = extract_entities("see https://example.com/page?x=1 for more");
        assert!(
            find_kind(&r, EntityKind::Url, "https://example.com/page?x=1").is_some(),
            "{r:?}"
        );
    }

    #[test]
    fn extracts_emails_lowercased() {
        let r = extract_entities("contact JANE@Example.COM about Q4");
        assert!(
            find_kind(&r, EntityKind::Email, "jane@example.com").is_some(),
            "{r:?}"
        );
    }

    #[test]
    fn extracts_hashtags_including_cjk() {
        let r = extract_entities("#rust and #编程 are great");
        assert!(
            find_kind(&r, EntityKind::Hashtag, "rust").is_some(),
            "{r:?}"
        );
        assert!(
            find_kind(&r, EntityKind::Hashtag, "编程").is_some(),
            "{r:?}"
        );
    }

    #[test]
    fn extracts_mentions() {
        let r = extract_entities("ask @alice or @bob_42");
        assert!(
            find_kind(&r, EntityKind::Person, "alice").is_some(),
            "{r:?}"
        );
        assert!(
            find_kind(&r, EntityKind::Person, "bob_42").is_some(),
            "{r:?}"
        );
    }

    #[test]
    fn dedupes_identical_mentions() {
        let r = extract_entities("https://x.io and https://x.io again");
        let urls: Vec<_> = r.iter().filter(|m| m.kind == EntityKind::Url).collect();
        assert_eq!(urls.len(), 1);
    }

    #[test]
    fn canonical_id_stable_across_case() {
        assert_eq!(
            canonical_id(EntityKind::Email, "Jane@Example.com"),
            canonical_id(EntityKind::Email, "jane@example.com")
        );
    }

    #[test]
    fn canonical_id_separates_kinds() {
        assert_ne!(
            canonical_id(EntityKind::Url, "abc"),
            canonical_id(EntityKind::Hashtag, "abc")
        );
    }

    #[test]
    fn empty_text_returns_empty() {
        assert!(extract_entities("").is_empty());
        assert!(extract_entities("no mentions here").is_empty());
    }
}
