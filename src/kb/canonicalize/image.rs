//! Image canonicalizer — OCR an image into its text body.
//!
//! Images aren't otherwise ingestable: without an `kb.ocr` endpoint this
//! returns `None` (skipped, not an error) so a mixed-directory ingest
//! doesn't fail on a stray screenshot. With OCR configured, the extracted
//! text becomes the doc markdown and the source is recorded as `Img`.

use anyhow::Context as _;
use base64::Engine as _;

use super::*;
use crate::kb::content_store::atomic::sha256_hex;

pub struct ImageCanonicalizer;

/// MIME prefixes this canonicalizer claims.
const IMAGE_MIMES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/jpg",
    "image/webp",
    "image/gif",
    "image/bmp",
    "image/tiff",
];

impl Canonicalizer for ImageCanonicalizer {
    fn source_kind(&self) -> KbSourceKind {
        KbSourceKind::Img
    }

    fn supports_mime(&self, mime: &str) -> bool {
        IMAGE_MIMES.contains(&mime)
    }

    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        let Some(client) = crate::ocr::OcrClient::from_config() else {
            // No OCR endpoint → image ingest is a no-op, not a failure.
            tracing::info!(
                mime = input.mime,
                "kb: image source skipped — no kb.ocr endpoint configured"
            );
            return Ok(None);
        };

        let b64 = base64::engine::general_purpose::STANDARD.encode(input.bytes);
        let data_uri = format!("data:{};base64,{}", input.mime, b64);
        let text = client
            .ocr(&data_uri)
            .context("kb image canonicalize: OCR request failed")?;
        let body = text.trim().to_string();
        if body.is_empty() {
            tracing::info!("kb: OCR returned empty text for image source");
            return Ok(None);
        }

        let lsid = input
            .logical_source_id_seed
            .clone()
            .unwrap_or_else(|| LogicalSourceId::for_file(&sha256_hex(input.bytes)));
        Ok(Some(CanonicalizedSource {
            markdown: body,
            metadata: CanonicalMetadata {
                source_kind: KbSourceKind::Img,
                logical_source_id: lsid,
                title: input.hint_title.unwrap_or("Image").to_string(),
                mime: input.mime.to_string(),
                created_at_ms: chrono::Utc::now().timestamp_millis(),
                tags: vec![],
                extra: serde_json::Value::Null,
            },
        }))
    }
}
