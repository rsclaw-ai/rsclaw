//! On-disk content store. Files at `md/<kind>/<slug>--<lsid8>.md`
//! (atomic, no-clobber; the `--<lsid8>` suffix is the first 8 hex
//! chars of `sha256(logical_source_id)` so re-ingest is idempotent
//! and same-slug different-source ingests don't collide) and
//! optional raw at `raw/<doc_id>.<ext>`. DB stores relative paths +
//! sha256 + byte_offset only.

pub mod atomic;
pub mod paths;
pub mod compose;
pub mod read;

use crate::kb::model::KbSourceKind;
use crate::kb::paths::KbPaths;
use anyhow::{Context, Result};
pub use compose::{compose_doc_file, parse_doc_file, FrontMatter, Parsed};
pub use read::{read_doc_body, read_doc_range, verify_doc_sha};

#[derive(Debug, Clone)]
pub struct StagedDoc {
    pub doc_id: String,
    pub markdown_rel_path: String,
    pub markdown_sha256: String,
    pub raw_rel_path: Option<String>,
    pub body_offset_in_file: usize,
}

#[derive(Debug)]
pub struct StageInput<'a> {
    pub doc_id: &'a str,
    pub kind: KbSourceKind,
    pub slug: &'a str,
    /// REQUIRED — seeds the path suffix so re-ingest of the same
    /// source lands at the same file and different sources with the
    /// same slug get different files. See `paths::markdown_rel_path`
    /// for why.
    pub logical_source_id: &'a str,
    pub front: FrontMatter,
    pub body: &'a str,
    pub raw: Option<(&'a [u8], &'a str)>, // (bytes, ext)
    pub keep_raw: bool,
}

pub fn stage_doc(paths: &KbPaths, input: StageInput<'_>) -> Result<StagedDoc> {
    let md_rel = paths::markdown_rel_path(input.kind, input.slug, input.logical_source_id);
    let md_abs = paths.root.join(&md_rel);
    let composed = compose_doc_file(&input.front, input.body)?;
    let parsed = parse_doc_file(&composed)?;
    let new_body_sha = atomic::sha256_hex(parsed.body.as_bytes());

    let wrote = atomic::write_if_new(&md_abs, composed.as_bytes())?;
    let md_sha = if wrote {
        new_body_sha
    } else {
        // Path already on disk — must be a re-ingest of the same
        // logical_source_id (the only way to land at this path).
        // Verify the existing body matches what we'd have written; if
        // it doesn't, that's either (a) a 32-bit lsid_hash8 collision
        // or (b) a non-deterministic canonicalizer — both worth
        // surfacing immediately instead of silently returning a sha
        // that doesn't match the bytes on disk (the original bug).
        let existing = std::fs::read(&md_abs)
            .with_context(|| format!("read existing {}", md_abs.display()))?;
        let existing_str = std::str::from_utf8(&existing)
            .with_context(|| format!("existing {} not utf8", md_abs.display()))?;
        let existing_parsed = parse_doc_file(existing_str)?;
        let existing_sha = atomic::sha256_hex(existing_parsed.body.as_bytes());
        if existing_sha != new_body_sha {
            return Err(anyhow::anyhow!(
                "stage_doc collision at {}: existing body sha {} ≠ new body sha {} \
                 (logical_source_id={}, doc_id={}). Two different sources hashed to \
                 the same lsid8 path suffix, OR the canonicalizer is non-deterministic. \
                 Refusing to silently overwrite.",
                md_abs.display(),
                existing_sha,
                new_body_sha,
                input.logical_source_id,
                input.doc_id,
            ));
        }
        existing_sha
    };

    let raw_rel = if input.keep_raw {
        if let Some((bytes, ext)) = input.raw {
            let rel = paths::raw_rel_path(input.doc_id, ext);
            atomic::write_if_new(&paths.root.join(&rel), bytes)?;
            Some(rel)
        } else {
            None
        }
    } else {
        None
    };

    Ok(StagedDoc {
        doc_id: input.doc_id.to_string(),
        markdown_rel_path: md_rel,
        markdown_sha256: md_sha,
        raw_rel_path: raw_rel,
        body_offset_in_file: parsed.body_offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fm() -> FrontMatter {
        FrontMatter {
            title: "T".into(),
            source_kind: "doc".into(),
            logical_source_id: "x".into(),
            created_at: "2026-05-19".into(),
            tags: vec![],
            meta: serde_json::Value::Null,
        }
    }

    #[test]
    fn stage_md_and_raw() {
        let tmp = TempDir::new().unwrap();
        let p = KbPaths::new(tmp.path());
        p.ensure_layout().unwrap();
        let s = stage_doc(
            &p,
            StageInput {
                doc_id: "01HXY",
                kind: KbSourceKind::Doc,
                slug: "test",
                logical_source_id: "file:sha256:aaaa",
                front: fm(),
                body: "# Hi",
                raw: Some((b"raw", "pdf")),
                keep_raw: true,
            },
        )
        .unwrap();
        assert!(s.markdown_rel_path.starts_with("md/doc/test--"));
        assert!(s.markdown_rel_path.ends_with(".md"));
        assert_eq!(s.raw_rel_path.as_deref(), Some("raw/01HXY.pdf"));
    }

    #[test]
    fn skip_raw_when_disabled() {
        let tmp = TempDir::new().unwrap();
        let p = KbPaths::new(tmp.path());
        p.ensure_layout().unwrap();
        let s = stage_doc(
            &p,
            StageInput {
                doc_id: "01H",
                kind: KbSourceKind::Doc,
                slug: "n",
                logical_source_id: "file:sha256:bbbb",
                front: fm(),
                body: "x",
                raw: Some((b"r", "txt")),
                keep_raw: false,
            },
        )
        .unwrap();
        assert!(s.raw_rel_path.is_none());
    }

    #[test]
    fn stage_then_read_range() {
        let tmp = TempDir::new().unwrap();
        let p = KbPaths::new(tmp.path());
        p.ensure_layout().unwrap();
        let s = stage_doc(
            &p,
            StageInput {
                doc_id: "01H",
                kind: KbSourceKind::Doc,
                slug: "r",
                logical_source_id: "file:sha256:cccc",
                front: fm(),
                body: "0123456789",
                raw: None,
                keep_raw: false,
            },
        )
        .unwrap();
        assert_eq!(
            read_doc_range(&p.root.join(&s.markdown_rel_path), 3, 7).unwrap(),
            "3456"
        );
    }

    #[test]
    fn stage_reingest_same_lsid_returns_consistent_sha() {
        // Re-ingesting the same logical source must land at the same
        // path, and the returned sha must match the bytes actually
        // on disk (regression: previously returned the new body's
        // sha while the file held the original body).
        let tmp = TempDir::new().unwrap();
        let p = KbPaths::new(tmp.path());
        p.ensure_layout().unwrap();
        let s1 = stage_doc(
            &p,
            StageInput {
                doc_id: "01H",
                kind: KbSourceKind::Doc,
                slug: "report",
                logical_source_id: "file:sha256:dddd",
                front: fm(),
                body: "hello world",
                raw: None,
                keep_raw: false,
            },
        )
        .unwrap();
        let s2 = stage_doc(
            &p,
            StageInput {
                doc_id: "01H",
                kind: KbSourceKind::Doc,
                slug: "report",
                logical_source_id: "file:sha256:dddd",
                front: fm(),
                body: "hello world",
                raw: None,
                keep_raw: false,
            },
        )
        .unwrap();
        assert_eq!(s1.markdown_rel_path, s2.markdown_rel_path);
        assert_eq!(s1.markdown_sha256, s2.markdown_sha256);
        // sha on disk equals returned sha
        let on_disk = std::fs::read(p.root.join(&s1.markdown_rel_path)).unwrap();
        let parsed = parse_doc_file(std::str::from_utf8(&on_disk).unwrap()).unwrap();
        assert_eq!(atomic::sha256_hex(parsed.body.as_bytes()), s1.markdown_sha256);
    }

    #[test]
    fn stage_collision_with_divergent_body_errors() {
        // If two stage_doc calls land at the same path (same lsid)
        // but with different bodies, the second MUST error rather
        // than silently return a sha that doesn't match disk. This
        // catches non-deterministic canonicalizers and 32-bit
        // lsid_hash8 collisions.
        let tmp = TempDir::new().unwrap();
        let p = KbPaths::new(tmp.path());
        p.ensure_layout().unwrap();
        stage_doc(
            &p,
            StageInput {
                doc_id: "01H",
                kind: KbSourceKind::Doc,
                slug: "x",
                logical_source_id: "file:sha256:eeee",
                front: fm(),
                body: "first body",
                raw: None,
                keep_raw: false,
            },
        )
        .unwrap();
        let err = stage_doc(
            &p,
            StageInput {
                doc_id: "01H",
                kind: KbSourceKind::Doc,
                slug: "x",
                logical_source_id: "file:sha256:eeee",
                front: fm(),
                body: "second body different",
                raw: None,
                keep_raw: false,
            },
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("collision"), "want collision error, got: {msg}");
    }

    #[test]
    fn stage_different_lsid_same_slug_no_collision() {
        // Two different sources with the same slugified title must
        // produce two distinct files.
        let tmp = TempDir::new().unwrap();
        let p = KbPaths::new(tmp.path());
        p.ensure_layout().unwrap();
        let a = stage_doc(
            &p,
            StageInput {
                doc_id: "01A",
                kind: KbSourceKind::Doc,
                slug: "report",
                logical_source_id: "file:sha256:1111",
                front: fm(),
                body: "body A",
                raw: None,
                keep_raw: false,
            },
        )
        .unwrap();
        let b = stage_doc(
            &p,
            StageInput {
                doc_id: "01B",
                kind: KbSourceKind::Doc,
                slug: "report",
                logical_source_id: "file:sha256:2222",
                front: fm(),
                body: "body B",
                raw: None,
                keep_raw: false,
            },
        )
        .unwrap();
        assert_ne!(a.markdown_rel_path, b.markdown_rel_path);
        assert!(p.root.join(&a.markdown_rel_path).exists());
        assert!(p.root.join(&b.markdown_rel_path).exists());
    }
}
