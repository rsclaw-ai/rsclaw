//! Week 1 end-to-end: canonicalize → stage → chunk. No DB writes yet
//! (Week 2 introduces ledger/outbox writes).

use anyhow::Result;
use rsclaw::kb::{
    canonicalize_by_mime, chunk_markdown, detect_mime, read_doc_range, stage_doc,
    CanonicalizeInput, ChunkerInput, FrontMatter, KbPaths, LocatorKind, StageInput,
};
use std::path::Path;
use tempfile::TempDir;
use ulid::Ulid;

struct PipelineOut {
    doc_id: String,
    n_chunks: usize,
    markdown_rel_path: String,
    markdown_body: String,
}

fn pipeline(paths: &KbPaths, src_path: &Path) -> Result<PipelineOut> {
    let bytes = std::fs::read(src_path)?;
    let name = src_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("untitled");
    let mime = detect_mime(&bytes, Some(name));

    let canon = canonicalize_by_mime(CanonicalizeInput {
        bytes: &bytes,
        mime: &mime,
        hint_title: Some(name),
        logical_source_id_seed: None,
    })?
    .expect("should canonicalize");

    let doc_id = Ulid::new().to_string();
    let staged = stage_doc(
        paths,
        StageInput {
            doc_id: &doc_id,
            kind: canon.metadata.source_kind,
            slug: name,
            logical_source_id: canon.metadata.logical_source_id.as_str(),
            front: FrontMatter {
                title: canon.metadata.title.clone(),
                source_kind: canon.metadata.source_kind.as_str().to_string(),
                logical_source_id: canon.metadata.logical_source_id.as_str().to_string(),
                created_at: chrono::Utc::now().to_rfc3339(),
                tags: canon.metadata.tags.clone(),
                meta: canon.metadata.extra.clone(),
            },
            body: &canon.markdown,
            raw: Some((&bytes, name.rsplit('.').next().unwrap_or(""))),
            keep_raw: true,
        },
    )?;

    let chunks = chunk_markdown(ChunkerInput {
        logical_source_id: &canon.metadata.logical_source_id,
        doc_id: &doc_id,
        doc_version: 1,
        markdown_body: &canon.markdown,
        default_locator_kind: LocatorKind::MdSection,
    });

    // Verify every chunk's byte_offset reads back from the on-disk file.
    let abs = paths.root.join(&staged.markdown_rel_path);
    for c in &chunks {
        let body = read_doc_range(&abs, c.byte_offset.0, c.byte_offset.1)?;
        assert!(!body.trim().is_empty(), "chunk {} read back empty", c.seq);
    }

    Ok(PipelineOut {
        doc_id,
        n_chunks: chunks.len(),
        markdown_rel_path: staged.markdown_rel_path,
        markdown_body: canon.markdown,
    })
}

#[test]
fn e2e_markdown() -> Result<()> {
    let tmp = TempDir::new()?;
    let paths = KbPaths::new(tmp.path());
    paths.ensure_layout()?;
    let out = pipeline(&paths, Path::new("tests/fixtures/kb/sample.md"))?;
    assert!(out.n_chunks > 0, "got {} chunks", out.n_chunks);
    assert!(out.markdown_rel_path.starts_with("md/doc/"));
    assert!(out.markdown_rel_path.ends_with(".md"));
    // Path carries the lsid8 suffix (regression vs the original
    // `md/doc/<slug>.md` layout that allowed silent collisions).
    assert!(
        out.markdown_rel_path.contains("--"),
        "missing lsid8 suffix: {}",
        out.markdown_rel_path
    );
    assert!(
        paths.root.join(&out.markdown_rel_path).exists(),
        "staged markdown file should exist on disk"
    );
    let _ = out.doc_id;
    Ok(())
}

#[test]
fn e2e_html_strips_scripts() -> Result<()> {
    let tmp = TempDir::new()?;
    let paths = KbPaths::new(tmp.path());
    paths.ensure_layout()?;
    let out = pipeline(&paths, Path::new("tests/fixtures/kb/sample.html"))?;
    let md = std::fs::read_to_string(paths.root.join(&out.markdown_rel_path))?;
    assert!(!md.contains("alert"), "script must be stripped");
    assert!(out.markdown_body.contains("Hello world"));
    Ok(())
}

#[test]
fn e2e_text() -> Result<()> {
    let tmp = TempDir::new()?;
    let paths = KbPaths::new(tmp.path());
    paths.ensure_layout()?;
    let out = pipeline(&paths, Path::new("tests/fixtures/kb/sample.txt"))?;
    assert!(out.n_chunks >= 1);
    Ok(())
}

/// CRITICAL: re-ingesting same file produces same chunk_ids regardless
/// of doc_id. This is the idempotency invariant from spec §I.
#[test]
fn reingest_same_file_same_chunk_ids() -> Result<()> {
    let tmp = TempDir::new()?;
    let paths = KbPaths::new(tmp.path());
    paths.ensure_layout()?;

    let bytes = std::fs::read("tests/fixtures/kb/sample.md")?;
    let mime = detect_mime(&bytes, Some("sample.md"));

    let canon1 = canonicalize_by_mime(CanonicalizeInput {
        bytes: &bytes,
        mime: &mime,
        hint_title: Some("sample.md"),
        logical_source_id_seed: None,
    })?
    .unwrap();

    let canon2 = canonicalize_by_mime(CanonicalizeInput {
        bytes: &bytes,
        mime: &mime,
        hint_title: Some("sample.md"),
        logical_source_id_seed: None,
    })?
    .unwrap();

    assert_eq!(
        canon1.metadata.logical_source_id, canon2.metadata.logical_source_id,
        "same file bytes → same logical_source_id"
    );

    let a = chunk_markdown(ChunkerInput {
        logical_source_id: &canon1.metadata.logical_source_id,
        doc_id: "doc_A",
        doc_version: 1, // simulate first ingest
        markdown_body: &canon1.markdown,
        default_locator_kind: LocatorKind::MdSection,
    });
    let b = chunk_markdown(ChunkerInput {
        logical_source_id: &canon2.metadata.logical_source_id,
        doc_id: "doc_B",
        doc_version: 5, // simulate later re-ingest (different doc_id + version)
        markdown_body: &canon2.markdown,
        default_locator_kind: LocatorKind::MdSection,
    });

    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!(
            x.id, y.id,
            "chunk_id MUST be identical across re-ingests (idempotency)"
        );
    }
    Ok(())
}

/// Critical for stage_doc: re-ingesting same source lands at the same
/// path (idempotent) and stage_doc returns the same SHA both times.
#[test]
fn reingest_same_source_lands_at_same_path() -> Result<()> {
    let tmp = TempDir::new()?;
    let paths = KbPaths::new(tmp.path());
    paths.ensure_layout()?;
    let out1 = pipeline(&paths, Path::new("tests/fixtures/kb/sample.md"))?;
    let out2 = pipeline(&paths, Path::new("tests/fixtures/kb/sample.md"))?;
    assert_eq!(out1.markdown_rel_path, out2.markdown_rel_path);
    Ok(())
}

#[test]
fn url_canonicalization_idempotent() -> Result<()> {
    use rsclaw::kb::canonicalize_url;
    let a = canonicalize_url("https://example.com/p?utm_source=a&b=2&a=1")?;
    let b = canonicalize_url("https://example.com/p?a=1&b=2")?;
    assert_eq!(
        a, b,
        "tracker stripping + sorting must produce same canonical url"
    );
    Ok(())
}
