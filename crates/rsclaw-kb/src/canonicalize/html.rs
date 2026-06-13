//! HTML → plain markdown via lol-html. Strips `<script>` / `<style>`,
//! converts `<h1>`..`<h6>` to `#`-prefixed lines, inserts newlines
//! before `<p>` / `<br>` / `<li>`, then strips any remaining tags
//! and collapses whitespace.

use lol_html::{HtmlRewriter, Settings, element};

use super::*;
use crate::content_store::atomic::sha256_hex;

pub struct HtmlCanonicalizer;

impl Canonicalizer for HtmlCanonicalizer {
    fn source_kind(&self) -> KbSourceKind {
        KbSourceKind::Doc
    }

    fn supports_mime(&self, mime: &str) -> bool {
        matches!(mime, "text/html" | "application/xhtml+xml")
    }

    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        let stripped = strip_to_text(input.bytes)?;
        let trimmed = stripped.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        let title = extract_title(input.bytes)
            .unwrap_or_else(|| input.hint_title.unwrap_or("Untitled").to_string());
        let lsid = input
            .logical_source_id_seed
            .clone()
            .unwrap_or_else(|| LogicalSourceId::for_file(&sha256_hex(input.bytes)));
        Ok(Some(CanonicalizedSource {
            markdown: trimmed.to_string(),
            metadata: CanonicalMetadata {
                source_kind: KbSourceKind::Doc,
                logical_source_id: lsid,
                title,
                mime: input.mime.to_string(),
                created_at_ms: chrono::Utc::now().timestamp_millis(),
                tags: vec![],
                extra: serde_json::Value::Null,
            },
        }))
    }
}

fn strip_to_text(html: &[u8]) -> Result<String> {
    let mut sink = Vec::<u8>::new();
    {
        let mut r = HtmlRewriter::new(
            Settings {
                element_content_handlers: vec![
                    element!("script", |el| {
                        el.remove();
                        Ok(())
                    }),
                    element!("style", |el| {
                        el.remove();
                        Ok(())
                    }),
                    element!("h1, h2, h3, h4, h5, h6", |el| {
                        let level = el
                            .tag_name()
                            .as_str()
                            .strip_prefix('h')
                            .and_then(|n| n.parse::<usize>().ok())
                            .unwrap_or(1);
                        let prefix = "#".repeat(level);
                        el.before(
                            &format!("\n{prefix} "),
                            lol_html::html_content::ContentType::Text,
                        );
                        el.after("\n", lol_html::html_content::ContentType::Text);
                        Ok(())
                    }),
                    element!("p, br, li", |el| {
                        el.before("\n", lol_html::html_content::ContentType::Text);
                        Ok(())
                    }),
                ],
                ..Settings::default()
            },
            |chunk: &[u8]| sink.extend_from_slice(chunk),
        );
        r.write(html)?;
        r.end()?;
    }
    let s = String::from_utf8(sink).map_err(|e| anyhow::anyhow!(e))?;
    // lol-html doesn't drop the tag content of unhandled elements;
    // strip remaining tags + collapse whitespace.
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    Ok(out.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn extract_title(html: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(html).ok()?;
    let lower = s.to_ascii_lowercase();
    let start = lower.find("<title>")? + "<title>".len();
    let end = lower[start..].find("</title>")? + start;
    Some(s[start..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_scripts_styles() {
        let r = HtmlCanonicalizer
            .canonicalize(CanonicalizeInput {
                bytes: b"<html><body><script>alert(1)</script><p>Hi</p><style>x{}</style></body></html>",
                mime: "text/html",
                hint_title: None,
                logical_source_id_seed: None,
            })
            .unwrap()
            .unwrap();
        assert!(!r.markdown.contains("alert"));
        assert!(!r.markdown.contains("x{}"));
        assert!(r.markdown.contains("Hi"));
    }

    #[test]
    fn extract_title_from_head() {
        let r = HtmlCanonicalizer
            .canonicalize(CanonicalizeInput {
                bytes: b"<html><head><title>Page</title></head><body><p>X</p></body></html>",
                mime: "text/html",
                hint_title: None,
                logical_source_id_seed: None,
            })
            .unwrap()
            .unwrap();
        assert_eq!(r.metadata.title, "Page");
    }

    #[test]
    fn empty_returns_none() {
        let r = HtmlCanonicalizer
            .canonicalize(CanonicalizeInput {
                bytes: b"<html><body></body></html>",
                mime: "text/html",
                hint_title: None,
                logical_source_id_seed: None,
            })
            .unwrap();
        assert!(r.is_none());
    }
}
