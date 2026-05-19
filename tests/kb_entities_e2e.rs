//! Entity extraction e2e: ingest a doc with URLs/emails/hashtags →
//! worker drains → kb_search_entities returns the extracted mentions.

use anyhow::Result;
use rsclaw::kb::sync::{KbSourceSyncer, ManualUploadSyncer, SyncContext, SyncReason};
use rsclaw::kb::tools::kb_search_entities;
use rsclaw::kb::worker::{DefaultDispatcher, HandlerCtx, WorkerConfig, WorkerPool};
use rsclaw::kb::{CallerScope, KbEmbedder, KbIndex, KbPaths, KbStore, StubEmbedder};
use rsclaw::kb::search::SearchCtx;
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entities_extracted_and_queryable() -> Result<()> {
    let tmp = TempDir::new()?;
    let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb"))?);
    let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
    paths.ensure_layout()?;
    let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());
    let index = Arc::new(KbIndex::open(&paths)?);

    let fixture = tmp.path().join("contacts.md");
    std::fs::write(
        &fixture,
        "# Contacts\n\nVisit https://example.com or email jane@example.com. \
         #rust #编程 by @alice and @bob_42.",
    )?;

    let syncer = ManualUploadSyncer {
        source_id: "test:contacts".into(),
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
        embedder: embedder.clone(),
        index: index.clone(),
    };
    WorkerPool::run_one_blocking(&hctx, &WorkerConfig::default(), &DefaultDispatcher)?;

    let ctx = SearchCtx { store, index, paths, embedder };
    let url_hits = kb_search_entities::run(
        &ctx,
        kb_search_entities::KbSearchEntitiesInput {
            query: "https://example.com".into(),
            kind: Some("url".into()),
            limit: 10,
        },
        &CallerScope::default(),
    )?;
    assert!(
        url_hits.matches.iter().any(|m| m.aliases.iter().any(|s| s.contains("example.com"))),
        "expected URL entity, got: {:?}",
        url_hits.matches
    );

    let email_hits = kb_search_entities::run(
        &ctx,
        kb_search_entities::KbSearchEntitiesInput {
            query: "jane@example.com".into(),
            kind: Some("email".into()),
            limit: 10,
        },
        &CallerScope::default(),
    )?;
    assert!(
        !email_hits.matches.is_empty(),
        "expected email entity, got: {:?}",
        email_hits.matches
    );

    let tag_hits = kb_search_entities::run(
        &ctx,
        kb_search_entities::KbSearchEntitiesInput {
            query: "编程".into(),
            kind: Some("hashtag".into()),
            limit: 10,
        },
        &CallerScope::default(),
    )?;
    assert!(
        !tag_hits.matches.is_empty(),
        "expected CJK hashtag entity, got: {:?}",
        tag_hits.matches
    );
    Ok(())
}
