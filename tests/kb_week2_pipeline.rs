//! Week 2 end-to-end: ingest_canonicalized → WorkerPool drains job
//! asynchronously → chunks land in redb. Verifies the production
//! async path (not just `run_one_blocking`).

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use rsclaw::kb::{
    CanonicalizeInput, HandlerCtx, IngestInput, KbEmbedder, KbPaths, KbStore, StubEmbedder,
    WorkerConfig, WorkerPool, canonicalize_by_mime, ingest_canonicalized, store::chunks,
};
use tempfile::TempDir;
use tokio::time::timeout;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_to_end_ingest_then_worker_drains_async() -> Result<()> {
    let tmp = TempDir::new()?;
    let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb"))?);
    let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
    paths.ensure_layout()?;
    let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());

    let index = Arc::new(rsclaw::kb::KbIndex::open(&paths)?);
    let ctx = HandlerCtx {
        store: store.clone(),
        paths: paths.clone(),
        embedder,
        index,
    };
    let cfg = WorkerConfig {
        worker_id: "w-e2e".into(),
        poll_idle: Duration::from_millis(20),
        ..WorkerConfig::default()
    };
    let pool = WorkerPool::start(ctx, cfg);

    let bytes = b"# E2E\n\nfirst.\n\nsecond.\n\nthird.";
    let canon = canonicalize_by_mime(CanonicalizeInput {
        bytes,
        mime: "text/markdown",
        hint_title: Some("e2e"),
        logical_source_id_seed: None,
    })?
    .unwrap();
    let lsid = canon.metadata.logical_source_id.0.clone();
    ingest_canonicalized(
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
    )?;

    let wait = timeout(Duration::from_secs(2), async {
        loop {
            let rtx = store.begin_read().unwrap();
            let cs = chunks::chunks_for_logical(&rtx, &lsid).unwrap();
            if !cs.is_empty() {
                return cs;
            }
            drop(rtx);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert!(!wait.is_empty(), "worker never produced chunks");
    for c in &wait {
        assert_eq!(c.vector.len(), 1024);
    }
    pool.shutdown().await;
    Ok(())
}
