//! Entity extraction e2e: ingest a doc with URLs/emails/hashtags →
//! worker drains → kb_search_entities returns the extracted mentions.

use std::sync::Arc;

use anyhow::Result;
use rsclaw::kb::{
    CallerScope, KbEmbedder, KbIndex, KbPaths, KbStore, StubEmbedder,
    entities::extract::canonical_id,
    model::EntityKind,
    search::SearchCtx,
    sync::{KbSourceSyncer, ManualUploadSyncer, SyncContext, SyncReason},
    tools::{kb_search, kb_search_entities},
    worker::{DefaultDispatcher, HandlerCtx, WorkerConfig, WorkerPool},
};
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

    let ctx = SearchCtx {
        store,
        index,
        paths,
        embedder,
        reranker: None
    };
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
        url_hits
            .matches
            .iter()
            .any(|m| m.aliases.iter().any(|s| s.contains("example.com"))),
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

    // entity_alignment + warnings: query containing a known
    // canonical-id entity should report it in `entity_alignment`.
    let alignment_out = kb_search::run(
        &ctx,
        kb_search::KbSearchInput {
            query: "jane@example.com missing keyword".into(),
            k: 5,
            filter: Default::default(),
            mode: "hybrid".into(),
            diversity: "off".into(),
            mmr_lambda: 0.5,
            boost_entities: vec![],
            query_instruction: None,
        },
        &CallerScope::default(),
    )?;
    assert!(
        alignment_out
            .entity_alignment
            .iter()
            .any(|a| a.canonical_id.contains("email")),
        "expected an email entity in alignment, got: {:?}",
        alignment_out.entity_alignment
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn require_entities_filters_to_chunks_with_mention() -> Result<()> {
    let tmp = TempDir::new()?;
    let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb"))?);
    let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
    paths.ensure_layout()?;
    let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());
    let index = Arc::new(KbIndex::open(&paths)?);

    // Two docs: one mentions @alice, one doesn't.
    let f1 = tmp.path().join("a.md");
    std::fs::write(&f1, "# A\n\nMessage from @alice about the project.")?;
    let f2 = tmp.path().join("b.md");
    std::fs::write(&f2, "# B\n\nAnnouncement about the project status.")?;

    let hctx = HandlerCtx {
        store: store.clone(),
        paths: paths.clone(),
        embedder: embedder.clone(),
        index: index.clone(),
    };
    for path in [f1, f2] {
        let syncer = ManualUploadSyncer {
            source_id: format!("test:{}", path.display()),
            file_path: path,
            tags: vec![],
        };
        let sctx = SyncContext {
            store: store.clone(),
            paths: paths.clone(),
            index: index.clone(),
            embedder: embedder.clone(),
        };
        syncer.sync(&sctx, SyncReason::Manual).await.unwrap();
        WorkerPool::run_one_blocking(&hctx, &WorkerConfig::default(), &DefaultDispatcher)?;
    }

    let ctx = SearchCtx {
        store,
        index,
        paths,
        embedder,
        reranker: None
    };
    let alice_id = canonical_id(EntityKind::Person, "alice");

    // Baseline: no require_entities → both docs match "project".
    let baseline = kb_search::run(
        &ctx,
        kb_search::KbSearchInput {
            query: "project".into(),
            k: 10,
            filter: Default::default(),
            mode: "hybrid".into(),
            diversity: "off".into(),
            mmr_lambda: 0.5,
            boost_entities: vec![],
            query_instruction: None,
        },
        &CallerScope::default(),
    )?;
    let baseline_docs: std::collections::HashSet<String> = baseline
        .results
        .iter()
        .map(|h| h.doc_title.clone())
        .collect();
    assert!(
        baseline_docs.len() >= 2,
        "expected ≥2 docs in baseline, got {baseline_docs:?}"
    );

    // With require_entities=alice → only doc A matches.
    let filtered = kb_search::run(
        &ctx,
        kb_search::KbSearchInput {
            query: "project".into(),
            k: 10,
            filter: kb_search::KbSearchFilter {
                entity_ids: vec![alice_id],
                ..Default::default()
            },
            mode: "hybrid".into(),
            diversity: "off".into(),
            mmr_lambda: 0.5,
            boost_entities: vec![],
            query_instruction: None,
        },
        &CallerScope::default(),
    )?;
    assert!(
        !filtered.results.is_empty(),
        "expected at least one hit for @alice + 'project'"
    );
    for hit in &filtered.results {
        assert!(
            hit.doc_title.contains('A') || hit.text.contains("@alice"),
            "filter leaked a non-@alice doc: {hit:?}"
        );
    }
    Ok(())
}
