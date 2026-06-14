//! Legacy binary Office formats (.doc / .ppt) — explicitly unsupported.
//!
//! These are OLE2/CFB binary containers (not the zip-based OOXML `.docx` /
//! `.pptx`). There is no mature pure-Rust text extractor for them, and pulling
//! in an external converter (LibreOffice) conflicts with the single-binary,
//! local-first design. Rather than fall through to the generic
//! "no canonicalizer" error, surface a clear, actionable message: re-save as
//! the modern format. `.xls` is NOT here — calamine reads it (see spreadsheet).

use super::*;

pub const DOC_MIME: &str = "application/msword";
pub const PPT_MIME: &str = "application/vnd.ms-powerpoint";

pub struct LegacyOfficeCanonicalizer;

impl Canonicalizer for LegacyOfficeCanonicalizer {
    fn source_kind(&self) -> KbSourceKind {
        KbSourceKind::Doc
    }
    fn supports_mime(&self, mime: &str) -> bool {
        matches!(mime, DOC_MIME | PPT_MIME)
    }
    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        let (legacy, modern) = if input.mime == DOC_MIME {
            (".doc", ".docx")
        } else {
            (".ppt", ".pptx")
        };
        anyhow::bail!(
            "legacy {legacy} files are not supported — please open it and save as {modern}, then re-upload"
        )
    }
}
