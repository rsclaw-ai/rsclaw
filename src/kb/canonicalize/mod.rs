//! Canonicalizers turn a raw `(bytes, mime)` source into Markdown +
//! metadata. Each kind has its own impl (`text`, `md`, `html`, `pdf`);
//! `mime::canonicalize_by_mime` dispatches.
//!
//! Week 1 covers files only. URL fetch + HTML-from-URL canonicalizer
//! ships in Week 2 alongside the worker pool.

pub mod email;
pub mod html;
pub mod image;
pub mod legacy;
pub mod md;
pub mod mime;
pub mod ooxml;
pub mod pdf;
pub mod spreadsheet;
pub mod text;
pub mod url_canon;

use anyhow::Result;
pub use mime::{canonicalize_by_mime, detect_mime};
use serde::{Deserialize, Serialize};
pub use url_canon::canonicalize_url;

use crate::kb::model::{KbSourceKind, LogicalSourceId};

#[derive(Debug, Clone)]
pub struct CanonicalizedSource {
    pub markdown: String,
    pub metadata: CanonicalMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalMetadata {
    pub source_kind: KbSourceKind,
    pub logical_source_id: LogicalSourceId,
    pub title: String,
    pub mime: String,
    pub created_at_ms: i64,
    pub tags: Vec<String>,
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct CanonicalizeInput<'a> {
    pub bytes: &'a [u8],
    pub mime: &'a str,
    pub hint_title: Option<&'a str>,
    /// For file sources, sha256 of `bytes`. For URL sources, the
    /// canonicalized URL string. Used to seed `logical_source_id`
    /// when the canonicalizer can't compute it itself.
    pub logical_source_id_seed: Option<LogicalSourceId>,
}

pub trait Canonicalizer: Send + Sync {
    fn source_kind(&self) -> KbSourceKind;
    fn supports_mime(&self, mime: &str) -> bool;
    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::canonicalize::text::TextCanonicalizer;

    #[test]
    fn trait_dispatch() {
        let c = TextCanonicalizer;
        assert!(c.supports_mime("text/plain"));
        assert!(!c.supports_mime("application/pdf"));
    }
}
