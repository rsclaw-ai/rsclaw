//! Week 4 compactor integration: real ingest + worker drain + compactor
//! tick + verify ledger advanced from IndexingComplete → CleanupPending.

use anyhow::Result;
use rsclaw::kb::compactor::run_compactor_tick;
use rsclaw::kb::ledger::LedgerStatus;
use rsclaw::kb::store::ledger;
use rsclaw::kb::sync::{KbSourceSyncer, ManualUploadSyncer, SyncContext, SyncReason};
use rsclaw::kb::worker::{DefaultDispatcher, HandlerCtx, WorkerConfig, WorkerPool};
use rsclaw::kb::{KbEmbedder, KbIndex, KbPaths, KbStore, StubEmbedder};
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compactor_advances_indexing_complete_ledger() -> Result<()> {
    let tmp = TempDir::new()?;
    let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb"))?);
    let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
    paths.ensure_layout()?;
    let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());
    let index = Arc::new(KbIndex::open(&paths)?);

    let fixture = tmp.path().join("a.md");
    std::fs::write(&fixture, "# A\n\nbody one.")?;
    let syncer = ManualUploadSyncer {
        source_id: "test:a".into(),
        file_path: fixture,
        tags: vec![],
    };
    let sctx = SyncContext {
        store: store.clone(),
        paths: paths.clone(),
        index: index.clone(),
        embedder: embedder.clone(),
    };
    syncer.sync(&sctx, SyncReason::Manual).await.unwrap();

    let hctx = HandlerCtx {
        store: store.clone(),
        paths: paths.clone(),
        embedder,
        index,
    };
    WorkerPool::run_one_blocking(&hctx, &WorkerConfig::default(), &DefaultDispatcher)?;

    let rtx = store.begin_read()?;
    let done_before = ledger::list_by_status(&rtx, LedgerStatus::IndexingComplete)?;
    assert_eq!(done_before.len(), 1, "worker should have advanced the ledger");
    drop(rtx);

    // Compactor tick should bump IndexingComplete → CleanupPending.
    let now = chrono::Utc::now().timestamp_millis();
    let stats = run_compactor_tick(&store, &paths, now)?;
    assert_eq!(stats.ledger_advanced_to_cleanup, 1);

    let rtx = store.begin_read()?;
    let pending = ledger::list_by_status(&rtx, LedgerStatus::CleanupPending)?;
    assert_eq!(pending.len(), 1, "ledger should now be CleanupPending");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compactor_snapshot_files_appear_via_kb_index() -> Result<()> {
    use rsclaw::kb::KbIndex;
    let tmp = TempDir::new()?;
    let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb"))?);
    let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
    paths.ensure_layout()?;
    let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());
    let index = Arc::new(KbIndex::open(&paths)?);

    // Drive at least one chunk into the index so file_dump has data.
    let fixture = tmp.path().join("snap.md");
    std::fs::write(&fixture, "# Snap\n\nbody for snapshot test.")?;
    let syncer = ManualUploadSyncer {
        source_id: "test:snap".into(),
        file_path: fixture,
        tags: vec![],
    };
    let sctx = SyncContext {
        store: store.clone(),
        paths: paths.clone(),
        index: index.clone(),
        embedder: embedder.clone(),
    };
    syncer.sync(&sctx, SyncReason::Manual).await.unwrap();
    let hctx = HandlerCtx {
        store: store.clone(),
        paths: paths.clone(),
        embedder,
        index: index.clone(),
    };
    WorkerPool::run_one_blocking(&hctx, &WorkerConfig::default(), &DefaultDispatcher)?;

    // Trigger snapshot via the KbIndex API (what `kb compact` calls).
    index.snapshot_hnsw(&paths)?;
    let snap_dir = paths.root.join("hnsw");
    assert!(
        snap_dir.join("snapshot.meta.json").exists(),
        "meta.json missing"
    );
    assert!(
        snap_dir.join("snapshot.hnsw.graph").exists(),
        "hnsw.graph missing"
    );
    assert!(
        snap_dir.join("snapshot.hnsw.data").exists(),
        "hnsw.data missing"
    );
    Ok(())
}
