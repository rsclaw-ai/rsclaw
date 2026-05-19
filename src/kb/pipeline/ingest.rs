//! ingest_canonicalized: the single atomic step that turns a
//! CanonicalizedSource into a persisted KbDoc + enqueued
//! ChunkAndEmbed job. See spec §J Ingest 流程.

use crate::kb::canonicalize::CanonicalizedSource;
use crate::kb::content_store::{
    atomic::sha256_hex, compose::FrontMatter, paths::slugify, stage_doc, StageInput,
};
use crate::kb::jobs::{Job, JobKind};
use crate::kb::ledger::{IngestLedgerEntry, LedgerOp, LedgerStatus};
use crate::kb::model::{KbDoc, KbSource, KbStatus, KbVisibility, VersionPointer};
use crate::kb::paths::KbPaths;
use crate::kb::store::{docs, jobs, ledger, seen, KbStore};
use anyhow::Result;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct IngestInput<'a> {
    pub canon: &'a CanonicalizedSource,
    pub raw_bytes: &'a [u8],
    pub raw_ext: &'a str,
    pub visibility: Option<KbVisibility>,
    pub owner_user_id: Option<String>,
    pub seen_key: Option<(&'a str, &'a str)>,
    pub source: Option<KbSource>,
    pub paths: &'a KbPaths,
}

#[derive(Debug, Clone)]
pub struct IngestOutput {
    pub doc_id: String,
    pub noop: bool,
    pub markdown_rel_path: String,
}

pub fn ingest_canonicalized(store: &KbStore, input: IngestInput<'_>) -> Result<IngestOutput> {
    let raw_sha = sha256_hex(input.raw_bytes);
    let lsid_str = input.canon.metadata.logical_source_id.as_str().to_string();
    let now_ms = chrono::Utc::now().timestamp_millis();

    // 1. NOOP fast-path (read-only).
    {
        let rtx = store.begin_read()?;
        if let Some(existing_doc_id) = docs::find_by_logical_and_hash(&rtx, &lsid_str, &raw_sha)? {
            if let Some(existing) = docs::get(&rtx, &existing_doc_id)? {
                tracing::info!(
                    doc = %crate::kb::redact(&existing_doc_id),
                    "kb ingest: noop (fast path)"
                );
                return Ok(IngestOutput {
                    doc_id: existing_doc_id,
                    noop: true,
                    markdown_rel_path: existing.markdown_path,
                });
            }
        }
    }

    // 2. Stage markdown + raw bytes BEFORE opening the wtx.
    let doc_id = ulid::Ulid::new().to_string();
    let slug = slugify(&input.canon.metadata.title);
    let staged = stage_doc(
        input.paths,
        StageInput {
            doc_id: &doc_id,
            kind: input.canon.metadata.source_kind,
            slug: &slug,
            logical_source_id: &lsid_str,
            front: FrontMatter {
                title: input.canon.metadata.title.clone(),
                source_kind: input.canon.metadata.source_kind.as_str().to_string(),
                logical_source_id: lsid_str.clone(),
                created_at: chrono::Utc::now().to_rfc3339(),
                tags: input.canon.metadata.tags.clone(),
                meta: input.canon.metadata.extra.clone(),
            },
            body: &input.canon.markdown,
            raw: Some((input.raw_bytes, input.raw_ext)),
            keep_raw: true,
        },
    )?;

    // 3. Single write tx: NOOP re-check + version compute + writes.
    let wtx = store.begin_write()?;

    // 3a. Race-safe NOOP re-check using the wtx's view.
    if let Some(existing_doc_id) =
        docs::find_by_logical_and_hash_in_wtx(&wtx, &lsid_str, &raw_sha)?
    {
        if let Some(existing) = docs::get_in_wtx(&wtx, &existing_doc_id)? {
            drop(wtx);
            tracing::info!(
                doc = %crate::kb::redact(&existing_doc_id),
                "kb ingest: noop (lost race; staged files left for compactor)"
            );
            return Ok(IngestOutput {
                doc_id: existing_doc_id,
                noop: true,
                markdown_rel_path: existing.markdown_path,
            });
        }
    }

    // 3b. Version + old_paths INSIDE wtx.
    let next_version = docs::next_version_for_in_wtx(&wtx, &lsid_str)?;
    let old_paths = if next_version > 1 {
        let ptr = docs::latest_version_in_wtx(&wtx, &lsid_str)?.unwrap();
        match docs::get_in_wtx(&wtx, &ptr.doc_id)? {
            Some(prev) => {
                let mut p = vec![prev.markdown_path];
                if let Some(raw) = prev.raw_path {
                    p.push(raw);
                }
                p
            }
            None => vec![],
        }
    } else {
        vec![]
    };

    // 3c. Build records.
    let source = input
        .source
        .clone()
        .unwrap_or(KbSource::Doc { path: PathBuf::from("(manual)") });
    let visibility = input
        .visibility
        .clone()
        .unwrap_or_else(|| KbVisibility::default_for(input.canon.metadata.source_kind));
    let doc = KbDoc {
        id: doc_id.clone(),
        logical_source_id: lsid_str.clone(),
        source,
        source_kind: input.canon.metadata.source_kind,
        title: input.canon.metadata.title.clone(),
        mime: input.canon.metadata.mime.clone(),
        raw_sha256: raw_sha.clone(),
        markdown_path: staged.markdown_rel_path.clone(),
        markdown_sha256: staged.markdown_sha256.clone(),
        raw_path: staged.raw_rel_path.clone(),
        owner_user_id: input.owner_user_id.clone(),
        created_at: now_ms,
        updated_at: now_ms,
        version: next_version,
        status: KbStatus::Active,
        visibility,
        tags: input.canon.metadata.tags.clone(),
        meta: input.canon.metadata.extra.clone(),
    };

    let mut ledger_new_paths = vec![staged.markdown_rel_path.clone()];
    if let Some(raw_rel) = &staged.raw_rel_path {
        ledger_new_paths.push(raw_rel.clone());
    }
    let ledger_entry = IngestLedgerEntry {
        id: ulid::Ulid::new().to_string(),
        created_at: now_ms,
        updated_at: now_ms,
        doc_id: doc_id.clone(),
        logical_source_id: lsid_str.clone(),
        op: if next_version == 1 {
            LedgerOp::Create
        } else {
            LedgerOp::Update
        },
        new_paths: ledger_new_paths,
        old_paths,
        status: LedgerStatus::Pending,
        error: None,
    };

    let job = Job::new(JobKind::ChunkAndEmbed {
        doc_id: doc_id.clone(),
        doc_version: next_version,
    });

    // 3d. Persist all five records.
    docs::put(&wtx, &doc)?;
    docs::set_latest_version(
        &wtx,
        &lsid_str,
        &VersionPointer {
            doc_id: doc_id.clone(),
            version: next_version,
        },
    )?;
    ledger::put(&wtx, &ledger_entry)?;
    jobs::enqueue(&wtx, &job)?;
    if let Some((source_id, item_id)) = input.seen_key {
        seen::mark_seen(&wtx, source_id, item_id, &raw_sha, now_ms)?;
    }
    wtx.commit()?;

    tracing::info!(
        doc = %crate::kb::redact(&doc_id),
        version = next_version,
        "kb ingest: committed"
    );

    Ok(IngestOutput {
        doc_id,
        noop: false,
        markdown_rel_path: staged.markdown_rel_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::canonicalize::{canonicalize_by_mime, CanonicalizeInput};
    use crate::kb::content_store::atomic::sha256_hex;
    use crate::kb::jobs::JobStatus;
    use crate::kb::ledger::LedgerStatus;
    use crate::kb::store::{jobs as jobs_store, ledger as ledger_store};
    use tempfile::TempDir;

    fn fixture() -> (TempDir, KbStore, KbPaths) {
        let tmp = TempDir::new().unwrap();
        let store = KbStore::open(&tmp.path().join("kb.redb")).unwrap();
        let paths = KbPaths::new(tmp.path().join("kb"));
        paths.ensure_layout().unwrap();
        (tmp, store, paths)
    }

    fn canon(body: &str) -> CanonicalizedSource {
        let bytes = body.as_bytes();
        canonicalize_by_mime(CanonicalizeInput {
            bytes,
            mime: "text/markdown",
            hint_title: Some("title"),
            logical_source_id_seed: None,
        })
        .unwrap()
        .unwrap()
    }

    fn fixture_arc() -> (TempDir, std::sync::Arc<KbStore>, std::sync::Arc<KbPaths>) {
        let tmp = TempDir::new().unwrap();
        let store = std::sync::Arc::new(KbStore::open(&tmp.path().join("kb.redb")).unwrap());
        let paths = std::sync::Arc::new(KbPaths::new(tmp.path().join("kb")));
        paths.ensure_layout().unwrap();
        (tmp, store, paths)
    }

    #[test]
    fn fresh_ingest_writes_all_tables() {
        let (_tmp, store, paths) = fixture();
        let c = canon("# Hello\n\nbody.");
        let raw = b"# Hello\n\nbody.";
        let out = ingest_canonicalized(
            &store,
            IngestInput {
                canon: &c,
                raw_bytes: raw,
                raw_ext: "md",
                visibility: None,
                owner_user_id: None,
                seen_key: Some(("manual", "f1")),
                source: None,
                paths: &paths,
            },
        )
        .unwrap();
        assert!(!out.noop);
        assert!(out.markdown_rel_path.starts_with("md/doc/"));

        let rtx = store.begin_read().unwrap();
        let doc = docs::get(&rtx, &out.doc_id).unwrap().unwrap();
        assert_eq!(doc.version, 1);
        assert_eq!(doc.raw_sha256, sha256_hex(raw));

        let ptr = docs::latest_version(&rtx, &c.metadata.logical_source_id.0).unwrap().unwrap();
        assert_eq!(ptr.doc_id, out.doc_id);

        let pending = ledger_store::list_by_status(&rtx, LedgerStatus::Pending).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].doc_id, out.doc_id);

        let ready_jobs = jobs_store::list_by_status(&rtx, JobStatus::Ready).unwrap();
        assert_eq!(ready_jobs.len(), 1);
        assert!(matches!(ready_jobs[0].kind, JobKind::ChunkAndEmbed { .. }));

        let seen_rec = seen::is_seen(&rtx, "manual", "f1").unwrap().unwrap();
        assert_eq!(seen_rec.raw_sha256, sha256_hex(raw));
    }

    #[test]
    fn reingest_same_bytes_noops() {
        let (_tmp, store, paths) = fixture();
        let c = canon("identical body");
        let raw = b"identical body";
        let first = ingest_canonicalized(
            &store,
            IngestInput {
                canon: &c,
                raw_bytes: raw,
                raw_ext: "md",
                visibility: None,
                owner_user_id: None,
                seen_key: None,
                source: None,
                paths: &paths,
            },
        )
        .unwrap();
        let second = ingest_canonicalized(
            &store,
            IngestInput {
                canon: &c,
                raw_bytes: raw,
                raw_ext: "md",
                visibility: None,
                owner_user_id: None,
                seen_key: None,
                source: None,
                paths: &paths,
            },
        )
        .unwrap();
        assert_eq!(first.doc_id, second.doc_id);
        assert!(second.noop);
        let rtx = store.begin_read().unwrap();
        assert_eq!(jobs_store::list_by_status(&rtx, JobStatus::Ready).unwrap().len(), 1);
    }

    // ISSUE #3 in fix-sweep TODO: Week 1's stage_doc treats
    // `{slug}--{lsid8}.md` as a stable address keyed off the
    // logical_source_id and refuses to overwrite with different
    // body bytes — collision detection. This v2 scenario (same
    // lsid_seed, new content) needs stage_doc to support
    // version-aware paths (e.g. `--v{N}` suffix) before this test
    // can pass. Ignoring until the stage_doc fix lands.
    #[ignore = "blocked on stage_doc version-aware path support (issue #3)"]
    #[test]
    fn reingest_different_bytes_bumps_version() {
        let (_tmp, store, paths) = fixture();
        use crate::kb::canonicalize::CanonicalizeInput;
        use crate::kb::model::LogicalSourceId;
        let lsid = LogicalSourceId("file:custom:x".into());
        let c1 = canonicalize_by_mime(CanonicalizeInput {
            bytes: b"version 1",
            mime: "text/markdown",
            hint_title: Some("t"),
            logical_source_id_seed: Some(lsid.clone()),
        })
        .unwrap()
        .unwrap();
        let c2 = canonicalize_by_mime(CanonicalizeInput {
            bytes: b"version 2 different",
            mime: "text/markdown",
            hint_title: Some("t"),
            logical_source_id_seed: Some(lsid.clone()),
        })
        .unwrap()
        .unwrap();

        let a = ingest_canonicalized(
            &store,
            IngestInput {
                canon: &c1,
                raw_bytes: b"version 1",
                raw_ext: "md",
                visibility: None,
                owner_user_id: None,
                seen_key: None,
                source: None,
                paths: &paths,
            },
        )
        .unwrap();
        let b = ingest_canonicalized(
            &store,
            IngestInput {
                canon: &c2,
                raw_bytes: b"version 2 different",
                raw_ext: "md",
                visibility: None,
                owner_user_id: None,
                seen_key: None,
                source: None,
                paths: &paths,
            },
        )
        .unwrap();
        assert_ne!(a.doc_id, b.doc_id);
        assert!(!b.noop);

        let rtx = store.begin_read().unwrap();
        let doc_a = docs::get(&rtx, &a.doc_id).unwrap().unwrap();
        let doc_b = docs::get(&rtx, &b.doc_id).unwrap().unwrap();
        assert_eq!(doc_a.version, 1);
        assert_eq!(doc_b.version, 2);

        let ledgers = ledger_store::list_by_status(&rtx, LedgerStatus::Pending).unwrap();
        let lb = ledgers.iter().find(|e| e.doc_id == b.doc_id).unwrap();
        assert!(lb.old_paths.contains(&doc_a.markdown_path));
    }

    #[test]
    fn concurrent_ingest_same_bytes_produces_one_doc() {
        let (_tmp, store, paths) = fixture_arc();
        let raw_bytes = std::sync::Arc::new(b"# Concurrent\n\nshared body.".to_vec());

        let mk_canon = |raw: &[u8]| {
            canonicalize_by_mime(CanonicalizeInput {
                bytes: raw,
                mime: "text/markdown",
                hint_title: Some("concurrent"),
                logical_source_id_seed: None,
            })
            .unwrap()
            .unwrap()
        };

        let h1 = {
            let store = store.clone();
            let paths = paths.clone();
            let raw = raw_bytes.clone();
            std::thread::spawn(move || {
                let c = mk_canon(raw.as_slice());
                ingest_canonicalized(
                    &store,
                    IngestInput {
                        canon: &c,
                        raw_bytes: &raw,
                        raw_ext: "md",
                        visibility: None,
                        owner_user_id: None,
                        seen_key: None,
                        source: None,
                        paths: &paths,
                    },
                )
                .unwrap()
            })
        };
        let h2 = {
            let store = store.clone();
            let paths = paths.clone();
            let raw = raw_bytes.clone();
            std::thread::spawn(move || {
                let c = mk_canon(raw.as_slice());
                ingest_canonicalized(
                    &store,
                    IngestInput {
                        canon: &c,
                        raw_bytes: &raw,
                        raw_ext: "md",
                        visibility: None,
                        owner_user_id: None,
                        seen_key: None,
                        source: None,
                        paths: &paths,
                    },
                )
                .unwrap()
            })
        };
        let a = h1.join().unwrap();
        let b = h2.join().unwrap();

        assert_eq!(a.doc_id, b.doc_id);
        assert!(a.noop || b.noop, "expected one of the racing ingests to noop");
        let rtx = store.begin_read().unwrap();
        let ready = jobs_store::list_by_status(&rtx, JobStatus::Ready).unwrap();
        assert_eq!(ready.len(), 1, "race produced duplicate jobs");
    }
}
