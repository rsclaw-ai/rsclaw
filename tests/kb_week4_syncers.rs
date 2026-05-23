//! Week 4 syncer e2e: ManualUploadSyncer file → ingest → chunks landed.

use std::sync::Arc;

use anyhow::Result;
use redb::ReadableTable;
use rsclaw::kb::{
    KbEmbedder, KbIndex, KbPaths, KbStore, StubEmbedder,
    sync::{KbSourceSyncer, ManualUploadSyncer, SyncContext, SyncReason},
    worker::{DefaultDispatcher, HandlerCtx, WorkerConfig, WorkerPool},
};
use tempfile::TempDir;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_syncer_ingests_then_searchable() -> Result<()> {
    let tmp = TempDir::new()?;
    let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb"))?);
    let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
    paths.ensure_layout()?;
    let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());
    let index = Arc::new(KbIndex::open(&paths)?);

    let fixture = tmp.path().join("hello.md");
    std::fs::write(&fixture, "# Hello\n\nThe quick brown fox.")?;

    let syncer = ManualUploadSyncer {
        source_id: "test:hello".into(),
        file_path: fixture.clone(),
        tags: vec!["test".into()],
    };
    let ctx = SyncContext {
        store: store.clone(),
        paths: paths.clone(),
        index: index.clone(),
        embedder: embedder.clone(),
    };
    let outcome = syncer.sync(&ctx, SyncReason::Manual).await.unwrap();
    assert_eq!(outcome.docs_added, 1);

    let hctx = HandlerCtx {
        store: store.clone(),
        paths,
        embedder,
        index,
    };
    let cfg = WorkerConfig::default();
    WorkerPool::run_one_blocking(&hctx, &cfg, &DefaultDispatcher)?;

    let rtx = store.begin_read()?;
    let mut any = false;
    let tbl = rtx.open_table(rsclaw::kb::store::schema::KB_DOCS)?;
    for entry in tbl.iter()? {
        let (_, v) = entry?;
        let d: rsclaw::kb::model::KbDoc = rsclaw::kb::store::codec::decode(v.value())?;
        let cs = rsclaw::kb::store::chunks::chunks_for_logical(&rtx, &d.logical_source_id)?;
        if !cs.is_empty() {
            any = true;
            assert!(d.tags.contains(&"test".to_string()));
        }
    }
    assert!(any, "expected chunks after ManualUploadSyncer ingest");
    Ok(())
}

#[tokio::test]
async fn manual_syncer_dedupes_identical_bytes() -> Result<()> {
    let tmp = TempDir::new()?;
    let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb"))?);
    let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
    paths.ensure_layout()?;
    let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());
    let index = Arc::new(KbIndex::open(&paths)?);

    let fixture = tmp.path().join("doc.md");
    std::fs::write(&fixture, "# Hi\n\nbody.")?;
    let syncer = ManualUploadSyncer {
        source_id: "test:doc".into(),
        file_path: fixture,
        tags: vec![],
    };
    let ctx = SyncContext {
        store: store.clone(),
        paths,
        index,
        embedder,
    };
    let a = syncer.sync(&ctx, SyncReason::Manual).await.unwrap();
    let b = syncer.sync(&ctx, SyncReason::Manual).await.unwrap();
    assert_eq!(a.docs_added, 1);
    assert_eq!(b.docs_added, 0);
    assert_eq!(b.docs_skipped, 1, "second sync should noop");
    Ok(())
}
