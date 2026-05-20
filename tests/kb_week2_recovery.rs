//! Crash recovery integration tests for the KB ingest + worker
//! pipeline. See spec §J 崩溃恢复矩阵.

use anyhow::Result;
use rsclaw::kb::{
    canonicalize_by_mime, ingest_canonicalized,
    store::{chunks, jobs as jobs_store},
    CanonicalizeInput, DefaultDispatcher, HandlerCtx, IngestInput, KbEmbedder, KbPaths, KbStore,
    StubEmbedder, WorkerConfig, WorkerPool,
};
use std::sync::Arc;
use tempfile::TempDir;

fn pipeline_fixture() -> (TempDir, HandlerCtx, WorkerConfig, String, String) {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb")).unwrap());
    let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
    paths.ensure_layout().unwrap();
    let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());

    let bytes = b"# Recovery test\n\nbody one.\n\nbody two.";
    let canon = canonicalize_by_mime(CanonicalizeInput {
        bytes,
        mime: "text/markdown",
        hint_title: Some("recovery"),
        logical_source_id_seed: None,
    })
    .unwrap()
    .unwrap();
    let lsid = canon.metadata.logical_source_id.0.clone();
    let out = ingest_canonicalized(
        &store,
        IngestInput {
            canon: &canon,
            raw_bytes: bytes,
            raw_ext: "md",
            visibility: None,
            owner_user_id: None,
            seen_key: None,
            source: None,
            paths: &paths,
        },
    )
    .unwrap();
    let index = Arc::new(rsclaw::kb::KbIndex::open(&paths).unwrap());
    let ctx = HandlerCtx { store, paths, embedder, index };
    let cfg = WorkerConfig {
        worker_id: "w-recovery".into(),
        claim_ttl_ms: 50,
        ..WorkerConfig::default()
    };
    (tmp, ctx, cfg, out.doc_id, lsid)
}

#[test]
fn stalled_claim_is_reclaimed_and_rerun() -> Result<()> {
    let (_tmp, ctx, cfg, _doc_id, lsid) = pipeline_fixture();

    {
        let wtx = ctx.store.begin_write()?;
        let _ = jobs_store::claim_next(&wtx, "w-zombie", 100, cfg.claim_ttl_ms)?;
        wtx.commit()?;
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    let reclaimed = {
        let wtx = ctx.store.begin_write()?;
        let r = jobs_store::reclaim_stale(&wtx, 300, 5)?;
        wtx.commit()?;
        r
    };
    assert_eq!(reclaimed.len(), 1);
    let handler = DefaultDispatcher;
    assert!(WorkerPool::run_one_blocking(&ctx, &cfg, &handler)?);
    let rtx = ctx.store.begin_read()?;
    let cs = chunks::chunks_for_logical(&rtx, &lsid)?;
    assert!(!cs.is_empty());
    let mut ids = cs.iter().map(|c| c.id.clone()).collect::<Vec<_>>();
    ids.sort();
    let len_before = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), len_before, "duplicate chunk_ids");
    Ok(())
}

#[test]
fn ingest_survives_process_restart() -> Result<()> {
    let tmp = TempDir::new()?;
    let db_path = tmp.path().join("kb.redb");
    let kb_root = tmp.path().join("kb");
    let canon_bytes = b"# Restart\n\nbody.";
    let lsid = {
        let store = Arc::new(KbStore::open(&db_path)?);
        let paths = Arc::new(KbPaths::new(&kb_root));
        paths.ensure_layout()?;
        let canon = canonicalize_by_mime(CanonicalizeInput {
            bytes: canon_bytes,
            mime: "text/markdown",
            hint_title: Some("r"),
            logical_source_id_seed: None,
        })?
        .unwrap();
        let lsid = canon.metadata.logical_source_id.0.clone();
        ingest_canonicalized(
            &store,
            IngestInput {
                canon: &canon,
                raw_bytes: canon_bytes,
                raw_ext: "md",
                visibility: None,
                owner_user_id: None,
                seen_key: None,
                source: None,
                paths: &paths,
            },
        )?;
        lsid
    };

    let store = Arc::new(KbStore::open(&db_path)?);
    let paths = Arc::new(KbPaths::new(&kb_root));
    paths.ensure_layout()?;
    let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());
    let index = Arc::new(rsclaw::kb::KbIndex::open(&paths)?);
    let ctx = HandlerCtx { store: store.clone(), paths, embedder, index };
    let cfg = WorkerConfig::default();
    let handler = DefaultDispatcher;
    assert!(WorkerPool::run_one_blocking(&ctx, &cfg, &handler)?);

    let rtx = store.begin_read()?;
    assert!(!chunks::chunks_for_logical(&rtx, &lsid)?.is_empty());
    Ok(())
}
