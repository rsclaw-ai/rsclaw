//! Markdown canonicalizer + `heading_path_at()` helper used by the
//! chunker to build `KbChunk.heading_path`.

use super::*;
use crate::content_store::atomic::sha256_hex;

pub struct MdCanonicalizer;

impl Canonicalizer for MdCanonicalizer {
    fn source_kind(&self) -> KbSourceKind {
        KbSourceKind::Doc
    }

    fn supports_mime(&self, mime: &str) -> bool {
        matches!(mime, "text/markdown" | "text/x-markdown")
    }

    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        let body = std::str::from_utf8(input.bytes)
            .map_err(|e| anyhow::anyhow!("not utf8: {e}"))?
            .trim()
            .to_string();
        if body.is_empty() {
            return Ok(None);
        }
        let title = first_h1(&body)
            .or_else(|| input.hint_title.map(String::from))
            .unwrap_or_else(|| "Untitled".to_string());
        let lsid = input
            .logical_source_id_seed
            .clone()
            .unwrap_or_else(|| LogicalSourceId::for_file(&sha256_hex(input.bytes)));
        Ok(Some(CanonicalizedSource {
            markdown: body,
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

fn first_h1(md: &str) -> Option<String> {
    md.lines()
        .find(|l| l.starts_with("# "))
        .map(|l| l.trim_start_matches('#').trim().to_string())
}

/// Given a byte position in markdown, return the heading-stack at
/// that point. Used by the chunker so each chunk knows its breadcrumb
/// (`["Intro", "Setup", "Steps"]`).
pub fn heading_path_at(md: &str, byte_pos: usize) -> Vec<String> {
    let mut stack: Vec<(u8, String)> = Vec::new();
    let mut offset = 0usize;
    for line in md.lines() {
        if offset > byte_pos {
            break;
        }
        if let Some((level, text)) = parse_heading_line(line) {
            while let Some(top) = stack.last() {
                if top.0 >= level as u8 {
                    stack.pop();
                } else {
                    break;
                }
            }
            stack.push((level as u8, text.trim().to_string()));
        }
        offset += line.len() + 1; // +1 for the '\n' that .lines() strips
    }
    stack.into_iter().map(|(_, t)| t).collect()
}

fn parse_heading_line(line: &str) -> Option<(usize, &str)> {
    let lead = line.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&lead) && line.as_bytes().get(lead) == Some(&b' ') {
        Some((lead, &line[lead + 1..]))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulls_h1_title() {
        let r = MdCanonicalizer
            .canonicalize(CanonicalizeInput {
                bytes: b"# Doc\n\nbody",
                mime: "text/markdown",
                hint_title: None,
                logical_source_id_seed: None,
            })
            .unwrap()
            .unwrap();
        assert_eq!(r.metadata.title, "Doc");
    }

    #[test]
    fn heading_path_basic() {
        let md = "# A\n## B\nbody1\n## C\nbody2\n### C1\nbody3";
        assert_eq!(
            heading_path_at(md, md.find("body3").unwrap()),
            vec!["A".to_string(), "C".to_string(), "C1".to_string()]
        );
    }

    #[test]
    fn heading_path_pops_correctly() {
        let md = "# A\n## B\n## C\nbody";
        assert_eq!(
            heading_path_at(md, md.find("body").unwrap()),
            vec!["A".to_string(), "C".to_string()]
        );
    }

    #[test]
    fn heading_path_at_top_is_empty() {
        let md = "no headings here\nplain content";
        assert!(heading_path_at(md, 0).is_empty());
    }
}
