//! Plain-text passthrough canonicalizer.

use super::*;
use crate::kb::content_store::atomic::sha256_hex;

pub struct TextCanonicalizer;

impl Canonicalizer for TextCanonicalizer {
    fn source_kind(&self) -> KbSourceKind {
        KbSourceKind::Doc
    }

    fn supports_mime(&self, mime: &str) -> bool {
        matches!(mime, "text/plain" | "text/x-log" | "text/csv")
    }

    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        let body = std::str::from_utf8(input.bytes)
            .map_err(|e| anyhow::anyhow!("not utf8: {e}"))?
            .trim()
            .to_string();
        if body.is_empty() {
            return Ok(None);
        }
        let lsid = input
            .logical_source_id_seed
            .clone()
            .unwrap_or_else(|| LogicalSourceId::for_file(&sha256_hex(input.bytes)));
        Ok(Some(CanonicalizedSource {
            markdown: body,
            metadata: CanonicalMetadata {
                source_kind: KbSourceKind::Doc,
                logical_source_id: lsid,
                title: input.hint_title.unwrap_or("Untitled").to_string(),
                mime: input.mime.to_string(),
                created_at_ms: chrono::Utc::now().timestamp_millis(),
                tags: vec![],
                extra: serde_json::Value::Null,
            },
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough() {
        let r = TextCanonicalizer
            .canonicalize(CanonicalizeInput {
                bytes: b"hello",
                mime: "text/plain",
                hint_title: Some("G"),
                logical_source_id_seed: None,
            })
            .unwrap()
            .unwrap();
        assert_eq!(r.markdown, "hello");
        assert_eq!(r.metadata.title, "G");
        assert!(
            r.metadata
                .logical_source_id
                .as_str()
                .starts_with("file:sha256:")
        );
    }

    #[test]
    fn empty_returns_none() {
        let r = TextCanonicalizer
            .canonicalize(CanonicalizeInput {
                bytes: b"  \n  ",
                mime: "text/plain",
                hint_title: None,
                logical_source_id_seed: None,
            })
            .unwrap();
        assert!(r.is_none());
    }
}
