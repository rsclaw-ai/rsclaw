//! Compose / parse the on-disk markdown file format:
//!
//! ```text
//! ---
//! title: ...
//! source_kind: doc
//! logical_source_id: file:sha256:abcd
//! created_at: 2026-05-19T00:00:00Z
//! tags: []
//! meta: null
//! ---
//!
//! # Body markdown
//! ```
//!
//! Body offset is preserved on parse so `read_doc_range` can index
//! into the body without re-parsing the front-matter.
//!
//! Uses `serde_yaml_ng` (the repo's YAML dep) instead of the
//! unmaintained `serde_yaml`.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FrontMatter {
    pub title: String,
    pub source_kind: String,
    pub logical_source_id: String,
    pub created_at: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub meta: serde_json::Value,
}

pub fn compose_doc_file(fm: &FrontMatter, body: &str) -> Result<String> {
    let yaml = serde_yaml_ng::to_string(fm).context("yaml encode front-matter")?;
    Ok(format!("---\n{yaml}---\n\n{body}"))
}

#[derive(Debug)]
pub struct Parsed {
    pub front: FrontMatter,
    pub body: String,
    pub body_offset: usize,
}

pub fn parse_doc_file(content: &str) -> Result<Parsed> {
    let bytes = content.as_bytes();
    if !content.starts_with("---\n") {
        return Err(anyhow!("missing front-matter open"));
    }
    let after = &bytes[4..];
    let needle = b"\n---\n";
    let pos = after
        .windows(needle.len())
        .position(|w| w == needle)
        .ok_or_else(|| anyhow!("missing front-matter close"))?;
    let yaml_end = 4 + pos;
    let yaml = std::str::from_utf8(&bytes[4..yaml_end]).context("front-matter utf8")?;
    let front: FrontMatter = serde_yaml_ng::from_str(yaml).context("yaml parse")?;
    let body_start = yaml_end + needle.len();
    // Eat one trailing newline if present so the body starts at real content.
    let body_start = if bytes.get(body_start) == Some(&b'\n') {
        body_start + 1
    } else {
        body_start
    };
    let body = std::str::from_utf8(&bytes[body_start..])
        .context("body utf8")?
        .to_string();
    Ok(Parsed { front, body, body_offset: body_start })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fm() -> FrontMatter {
        FrontMatter {
            title: "T".into(),
            source_kind: "doc".into(),
            logical_source_id: "file:sha256:abc".into(),
            created_at: "2026-05-19T00:00:00Z".into(),
            tags: vec!["a".into()],
            meta: serde_json::Value::Null,
        }
    }

    #[test]
    fn roundtrip() {
        let body = "# Hi\n\nWorld.";
        let composed = compose_doc_file(&fm(), body).unwrap();
        let parsed = parse_doc_file(&composed).unwrap();
        assert_eq!(parsed.body, body);
        assert_eq!(parsed.front.title, "T");
        assert_eq!(parsed.front.tags, vec!["a"]);
    }

    #[test]
    fn body_offset_correct() {
        let composed = compose_doc_file(&fm(), "BODY").unwrap();
        let parsed = parse_doc_file(&composed).unwrap();
        assert_eq!(&composed.as_bytes()[parsed.body_offset..], b"BODY");
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse_doc_file("no front matter").is_err());
        assert!(parse_doc_file("---\nfoo\nbody").is_err());
    }

    #[test]
    fn empty_body_ok() {
        let composed = compose_doc_file(&fm(), "").unwrap();
        let parsed = parse_doc_file(&composed).unwrap();
        assert_eq!(parsed.body, "");
    }

    #[test]
    fn fm_serde_default_tags_and_meta() {
        // Older docs may not have tags/meta — defaults kick in.
        let yaml = "---\ntitle: T\nsource_kind: doc\nlogical_source_id: x\ncreated_at: now\n---\n\nbody";
        let parsed = parse_doc_file(yaml).unwrap();
        assert!(parsed.front.tags.is_empty());
        assert!(parsed.front.meta.is_null());
    }
}
