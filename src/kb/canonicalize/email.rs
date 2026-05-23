//! Email canonicalizers.
//!
//! - `.eml` (message/rfc822): one message → one doc. Headers
//!   (From/To/Cc/Subject/Date) become a leading block + `extra` metadata; the
//!   body prefers the `text/plain` MIME part, falling back to the `text/html`
//!   part rendered through the HTML canonicalizer. Attachments are listed by
//!   name, never inlined (no base64 noise in the index).
//! - `.mbox` (application/mbox): many messages → still ONE doc, each message
//!   rendered as a `---`-separated section. (Per-message docs would need the
//!   ingest path to emit N docs from one canonicalize call; deferred.)
//!
//! RFC2047 encoded-words (e.g. CJK subjects `=?UTF-8?B?...?=`) and
//! transfer-encodings (base64 / quoted-printable) are decoded by `mailparse`.

use mailparse::{MailHeaderMap, ParsedMail, parse_mail};

use super::*;
use crate::kb::{canonicalize::html::HtmlCanonicalizer, content_store::atomic::sha256_hex};

/// `Content-Type` for a single RFC822 message (`.eml`).
pub const EML_MIME: &str = "message/rfc822";
/// `Content-Type` for an mbox archive (`.mbox`).
pub const MBOX_MIME: &str = "application/mbox";

pub struct EmlCanonicalizer;

impl Canonicalizer for EmlCanonicalizer {
    fn source_kind(&self) -> KbSourceKind {
        KbSourceKind::Doc
    }

    fn supports_mime(&self, mime: &str) -> bool {
        matches!(mime, EML_MIME)
    }

    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        let parsed = parse_mail(input.bytes).map_err(|e| anyhow::anyhow!("parse eml: {e}"))?;
        let Some(rendered) = render_message(&parsed) else {
            return Ok(None);
        };
        let title = rendered
            .subject
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| input.hint_title.map(str::to_owned))
            .unwrap_or_else(|| "Untitled email".to_string());
        let lsid = input
            .logical_source_id_seed
            .clone()
            .unwrap_or_else(|| LogicalSourceId::for_file(&sha256_hex(input.bytes)));
        Ok(Some(CanonicalizedSource {
            markdown: rendered.markdown,
            metadata: CanonicalMetadata {
                source_kind: KbSourceKind::Doc,
                logical_source_id: lsid,
                title,
                mime: input.mime.to_string(),
                created_at_ms: chrono::Utc::now().timestamp_millis(),
                tags: vec![],
                extra: rendered.extra,
            },
        }))
    }
}

pub struct MboxCanonicalizer;

impl Canonicalizer for MboxCanonicalizer {
    fn source_kind(&self) -> KbSourceKind {
        KbSourceKind::Doc
    }

    fn supports_mime(&self, mime: &str) -> bool {
        matches!(mime, MBOX_MIME)
    }

    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        let mut sections: Vec<String> = Vec::new();
        let mut count = 0usize;
        for raw in split_mbox(input.bytes) {
            let Ok(parsed) = parse_mail(raw) else {
                continue;
            };
            if let Some(r) = render_message(&parsed) {
                count += 1;
                sections.push(r.markdown);
            }
        }
        if sections.is_empty() {
            return Ok(None);
        }
        let markdown = sections.join("\n\n---\n\n");
        let lsid = input
            .logical_source_id_seed
            .clone()
            .unwrap_or_else(|| LogicalSourceId::for_file(&sha256_hex(input.bytes)));
        Ok(Some(CanonicalizedSource {
            markdown,
            metadata: CanonicalMetadata {
                source_kind: KbSourceKind::Doc,
                logical_source_id: lsid,
                title: input
                    .hint_title
                    .map(str::to_owned)
                    .unwrap_or_else(|| "Mailbox".to_string()),
                mime: input.mime.to_string(),
                created_at_ms: chrono::Utc::now().timestamp_millis(),
                tags: vec![],
                extra: serde_json::json!({ "message_count": count }),
            },
        }))
    }
}

// ----- internals -----

struct RenderedEmail {
    subject: Option<String>,
    markdown: String,
    extra: serde_json::Value,
}

/// Render one parsed message to a markdown section: a header block followed
/// by the best available body part and an attachment list. Returns `None`
/// when there is no usable body and no headers worth keeping.
fn render_message(mail: &ParsedMail<'_>) -> Option<RenderedEmail> {
    let subject = mail.headers.get_first_value("Subject");
    let from = mail.headers.get_first_value("From");
    let to = mail.headers.get_first_value("To");
    let cc = mail.headers.get_first_value("Cc");
    let date = mail.headers.get_first_value("Date");

    let mut header_lines = Vec::new();
    if let Some(s) = &subject {
        header_lines.push(format!("# {s}"));
    }
    if let Some(f) = &from {
        header_lines.push(format!("From: {f}"));
    }
    if let Some(t) = &to {
        header_lines.push(format!("To: {t}"));
    }
    if let Some(c) = &cc {
        header_lines.push(format!("Cc: {c}"));
    }
    if let Some(d) = &date {
        header_lines.push(format!("Date: {d}"));
    }

    let mut attachments: Vec<String> = Vec::new();
    let body = extract_body(mail, &mut attachments);

    if body.trim().is_empty() && header_lines.is_empty() {
        return None;
    }

    let mut md = header_lines.join("\n");
    if !body.trim().is_empty() {
        if !md.is_empty() {
            md.push_str("\n\n");
        }
        md.push_str(body.trim());
    }
    if !attachments.is_empty() {
        md.push_str("\n\nAttachments: ");
        md.push_str(&attachments.join(", "));
    }

    Some(RenderedEmail {
        subject,
        markdown: md,
        extra: serde_json::json!({
            "from": from,
            "to": to,
            "cc": cc,
            "date": date,
            "attachments": attachments,
        }),
    })
}

/// Walk the MIME tree and return the best body text. Prefers `text/plain`;
/// renders `text/html` through the HTML canonicalizer when no plain part
/// exists. Collects attachment filenames into `attachments` along the way.
fn extract_body(mail: &ParsedMail<'_>, attachments: &mut Vec<String>) -> String {
    if mail.subparts.is_empty() {
        let mime = mail.ctype.mimetype.to_ascii_lowercase();
        // A part with a filename (or attachment disposition) is an attachment,
        // not body text — record its name and skip its bytes.
        if let Some(name) = attachment_name(mail) {
            attachments.push(name);
            return String::new();
        }
        return match mime.as_str() {
            "text/plain" => mail.get_body().unwrap_or_default(),
            "text/html" => html_to_markdown(&mail.get_body().unwrap_or_default()),
            _ => String::new(),
        };
    }
    // Multipart: collect the plain and html alternatives separately, prefer plain.
    let mut plain = String::new();
    let mut html = String::new();
    for part in &mail.subparts {
        let mime = part.ctype.mimetype.to_ascii_lowercase();
        if !part.subparts.is_empty() {
            let nested = extract_body(part, attachments);
            if plain.is_empty() {
                plain = nested;
            }
        } else if let Some(name) = attachment_name(part) {
            attachments.push(name);
        } else if mime == "text/plain" && plain.is_empty() {
            plain = part.get_body().unwrap_or_default();
        } else if mime == "text/html" && html.is_empty() {
            html = part.get_body().unwrap_or_default();
        }
    }
    if !plain.trim().is_empty() {
        plain
    } else {
        html_to_markdown(&html)
    }
}

/// The attachment filename for a part, if it is an attachment (has a
/// `filename` content-type param or a `Content-Disposition: attachment`).
fn attachment_name(part: &ParsedMail<'_>) -> Option<String> {
    if let Some(name) = part.ctype.params.get("name") {
        return Some(name.clone());
    }
    let disp = part.headers.get_first_value("Content-Disposition")?;
    let disp_l = disp.to_ascii_lowercase();
    if disp_l.contains("attachment") || disp_l.contains("filename") {
        // crude filename= extraction
        if let Some(idx) = disp_l.find("filename=") {
            let raw = disp[idx + "filename=".len()..].trim().trim_matches('"');
            let name = raw
                .split(';')
                .next()
                .unwrap_or(raw)
                .trim()
                .trim_matches('"');
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
        return Some("attachment".to_string());
    }
    None
}

/// Reuse the HTML canonicalizer for the html body part.
fn html_to_markdown(html: &str) -> String {
    if html.trim().is_empty() {
        return String::new();
    }
    HtmlCanonicalizer
        .canonicalize(CanonicalizeInput {
            bytes: html.as_bytes(),
            mime: "text/html",
            hint_title: None,
            logical_source_id_seed: None,
        })
        .ok()
        .flatten()
        .map(|c| c.markdown)
        .unwrap_or_default()
}

/// Split an mbox into individual message byte-slices. mbox delimits messages
/// with a line beginning `From ` (the envelope sender line). This is the
/// common-case heuristic; `>From `-quoting is not un-escaped (v1).
fn split_mbox(bytes: &[u8]) -> Vec<&[u8]> {
    let text = bytes;
    let mut starts: Vec<usize> = Vec::new();
    let mut i = 0;
    // A "From " at the very start, or after a newline, begins a message.
    while i < text.len() {
        let at_line_start = i == 0 || text[i - 1] == b'\n';
        if at_line_start && text[i..].starts_with(b"From ") {
            starts.push(i);
        }
        i += 1;
    }
    if starts.is_empty() {
        return vec![text];
    }
    let mut out = Vec::with_capacity(starts.len());
    for (idx, &s) in starts.iter().enumerate() {
        let end = starts.get(idx + 1).copied().unwrap_or(text.len());
        out.push(&text[s..end]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const EML: &[u8] = b"From: \"Alice\" <alice@example.com>\r\n\
To: bob@example.com\r\n\
Subject: Quarterly report\r\n\
Date: Mon, 19 May 2026 10:00:00 +0800\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
The Q2 revenue grew by 12 percent over Q1.\r\n";

    #[test]
    fn eml_extracts_headers_and_body() {
        let r = EmlCanonicalizer
            .canonicalize(CanonicalizeInput {
                bytes: EML,
                mime: EML_MIME,
                hint_title: None,
                logical_source_id_seed: None,
            })
            .unwrap()
            .unwrap();
        assert_eq!(r.metadata.title, "Quarterly report");
        assert!(r.markdown.contains("# Quarterly report"));
        assert!(r.markdown.contains("From: \"Alice\" <alice@example.com>"));
        assert!(r.markdown.contains("Q2 revenue grew by 12 percent"));
        assert_eq!(r.metadata.extra["from"], "\"Alice\" <alice@example.com>");
    }

    #[test]
    fn eml_decodes_rfc2047_subject() {
        // Subject: "你好" base64-encoded as an RFC2047 encoded-word.
        let raw = b"From: a@example.com\r\n\
Subject: =?UTF-8?B?5L2g5aW9?=\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
body\r\n";
        let r = EmlCanonicalizer
            .canonicalize(CanonicalizeInput {
                bytes: raw,
                mime: EML_MIME,
                hint_title: None,
                logical_source_id_seed: None,
            })
            .unwrap()
            .unwrap();
        assert_eq!(r.metadata.title, "你好");
    }

    #[test]
    fn eml_prefers_plain_over_html_in_multipart() {
        let raw = b"From: a@example.com\r\n\
Subject: Multi\r\n\
Content-Type: multipart/alternative; boundary=BND\r\n\
\r\n\
--BND\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
plain body wins\r\n\
--BND\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p>html body</p>\r\n\
--BND--\r\n";
        let r = EmlCanonicalizer
            .canonicalize(CanonicalizeInput {
                bytes: raw,
                mime: EML_MIME,
                hint_title: None,
                logical_source_id_seed: None,
            })
            .unwrap()
            .unwrap();
        assert!(r.markdown.contains("plain body wins"));
        assert!(!r.markdown.contains("<p>"));
    }

    #[test]
    fn mbox_splits_into_sections() {
        let mbox = b"From alice@example.com Mon May 19 10:00:00 2026\r\n\
From: alice@example.com\r\n\
Subject: First\r\n\
\r\n\
first body\r\n\
From bob@example.com Mon May 19 11:00:00 2026\r\n\
From: bob@example.com\r\n\
Subject: Second\r\n\
\r\n\
second body\r\n";
        let r = MboxCanonicalizer
            .canonicalize(CanonicalizeInput {
                bytes: mbox,
                mime: MBOX_MIME,
                hint_title: Some("inbox.mbox"),
                logical_source_id_seed: None,
            })
            .unwrap()
            .unwrap();
        assert!(r.markdown.contains("First"));
        assert!(r.markdown.contains("Second"));
        assert!(r.markdown.contains("---"));
        assert_eq!(r.metadata.extra["message_count"], 2);
    }
}
