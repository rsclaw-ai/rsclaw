//! Text-layer PDF extraction via `pdf-extract`. No OCR in Week 1 —
//! scanned PDFs without an embedded text layer return `Ok(None)`.
//! Each page becomes a `## Page N` section.

use super::*;
use crate::kb::content_store::atomic::sha256_hex;

pub struct PdfCanonicalizer;

impl Canonicalizer for PdfCanonicalizer {
    fn source_kind(&self) -> KbSourceKind {
        KbSourceKind::Doc
    }

    fn supports_mime(&self, mime: &str) -> bool {
        mime == "application/pdf"
    }

    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        let pages = extract_pages(input.bytes)?;
        let mut md = String::new();
        let mut has = false;
        for (i, p) in pages.iter().enumerate() {
            let t = p.trim();
            if t.is_empty() {
                continue;
            }
            if has {
                md.push_str("\n\n");
            }
            md.push_str(&format!("## Page {}\n\n{t}", i + 1));
            has = true;
        }
        if !has {
            return Ok(None);
        }
        let lsid = input
            .logical_source_id_seed
            .clone()
            .unwrap_or_else(|| LogicalSourceId::for_file(&sha256_hex(input.bytes)));
        Ok(Some(CanonicalizedSource {
            markdown: md,
            metadata: CanonicalMetadata {
                source_kind: KbSourceKind::Doc,
                logical_source_id: lsid,
                title: input.hint_title.unwrap_or("Untitled PDF").to_string(),
                mime: input.mime.to_string(),
                created_at_ms: chrono::Utc::now().timestamp_millis(),
                tags: vec![],
                extra: serde_json::json!({ "n_pages": pages.len() }),
            },
        }))
    }
}

fn extract_pages(bytes: &[u8]) -> Result<Vec<String>> {
    let text = pdf_extract::extract_text_from_mem(bytes)
        .map_err(|e| anyhow::anyhow!("pdf-extract: {e:?}"))?;
    // PDF page separator is form-feed (U+000C)
    Ok(text.split('\u{0C}').map(|s| s.to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_handled() {
        let r = PdfCanonicalizer.canonicalize(CanonicalizeInput {
            bytes: &[],
            mime: "application/pdf",
            hint_title: None,
            logical_source_id_seed: None,
        });
        // Either Ok(None) or Err is acceptable for empty input.
        match r {
            Ok(None) | Err(_) => {}
            Ok(Some(_)) => panic!("unexpected content from empty"),
        }
    }
}
