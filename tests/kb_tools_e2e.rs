//! End-to-end tests for the `tools::kb_*` surface: kb_fetch,
//! kb_similar, kb_list_docs. (kb_search has its own e2e in
//! `tests/kb_week3_search.rs`; kb_search_entities is covered by
//! `tests/kb_entities_e2e.rs`.)

use std::sync::Arc;

use anyhow::Result;
use rsclaw::kb::{
    CallerScope, KbEmbedder, KbIndex, KbPaths, KbStore, StubEmbedder,
    search::SearchCtx,
    sync::{KbSourceSyncer, ManualUploadSyncer, SyncContext, SyncReason},
    tools::{kb_fetch, kb_list_docs, kb_similar},
    worker::{DefaultDispatcher, HandlerCtx, WorkerConfig, WorkerPool},
};
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    store: Arc<KbStore>,
    index: Arc<KbIndex>,
    paths: Arc<KbPaths>,
    embedder: Arc<dyn KbEmbedder>,
}

async fn ingest(fx: &Fixture, body: &str, name: &str, tags: Vec<String>) {
    let path = fx._tmp.path().join(name);
    std::fs::write(&path, body).unwrap();
    let syncer = ManualUploadSyncer {
        source_id: format!("test:{name}"),
        file_path: path,
        tags,
    };
    let sctx = SyncContext {
        store: fx.store.clone(),
        paths: fx.paths.clone(),
        index: fx.index.clone(),
        embedder: fx.embedder.clone(),
    };
    syncer.sync(&sctx, SyncReason::Manual).await.unwrap();
    let hctx = HandlerCtx {
        store: fx.store.clone(),
        paths: fx.paths.clone(),
        embedder: fx.embedder.clone(),
        index: fx.index.clone(),
    };
    WorkerPool::run_one_blocking(&hctx, &WorkerConfig::default(), &DefaultDispatcher).unwrap();
}

fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb")).unwrap());
    let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
    paths.ensure_layout().unwrap();
    let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());
    let index = Arc::new(KbIndex::open(&paths).unwrap());
    Fixture {
        _tmp: tmp,
        store,
        index,
        paths,
        embedder,
    }
}

fn search_ctx(fx: &Fixture) -> SearchCtx {
    SearchCtx {
        store: fx.store.clone(),
        index: fx.index.clone(),
        paths: fx.paths.clone(),
        embedder: fx.embedder.clone(),
    }
}

fn first_chunk_id(fx: &Fixture, lsid: &str) -> String {
    let rtx = fx.store.begin_read().unwrap();
    let mut chunks = rsclaw::kb::store::chunks::chunks_for_logical(&rtx, lsid).unwrap();
    chunks.sort_by_key(|c| c.seq);
    chunks
        .into_iter()
        .next()
        .map(|c| c.id)
        .expect("no chunks for lsid")
}

fn doc_lsid(fx: &Fixture, idx: usize) -> String {
    use redb::ReadableTable;
    let rtx = fx.store.begin_read().unwrap();
    let tbl = rtx.open_table(rsclaw::kb::store::schema::KB_DOCS).unwrap();
    let mut docs: Vec<rsclaw::kb::model::KbDoc> = Vec::new();
    for entry in tbl.iter().unwrap() {
        let (_, v) = entry.unwrap();
        docs.push(rsclaw::kb::store::codec::decode(v.value()).unwrap());
    }
    docs.sort_by_key(|d| d.created_at);
    docs[idx].logical_source_id.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kb_fetch_returns_chunk_with_neighbors() -> Result<()> {
    let fx = fixture();
    ingest(
        &fx,
        "# Doc\n\nfirst paragraph.\n\nsecond paragraph.\n\nthird paragraph.",
        "doc.md",
        vec![],
    )
    .await;
    let lsid = doc_lsid(&fx, 0);
    let chunk_id = first_chunk_id(&fx, &lsid);

    let ctx = search_ctx(&fx);
    let out = kb_fetch::run(
        &ctx,
        kb_fetch::KbFetchInput {
            chunk_id: chunk_id.clone(),
            expand: "neighbor".into(),
        },
        &CallerScope::default(),
    )?;
    let out = out.expect("kb_fetch returned None");
    assert_eq!(out.chunk.chunk_id, chunk_id);
    assert!(!out.chunk.text.is_empty());
    // The first chunk's neighbors should include at least chunk 2.
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kb_fetch_full_doc_returns_whole_body() -> Result<()> {
    let fx = fixture();
    let body = "# Full Doc\n\nbody one.\n\nbody two.";
    ingest(&fx, body, "full.md", vec![]).await;
    let lsid = doc_lsid(&fx, 0);
    let chunk_id = first_chunk_id(&fx, &lsid);

    let ctx = search_ctx(&fx);
    let out = kb_fetch::run(
        &ctx,
        kb_fetch::KbFetchInput {
            chunk_id,
            expand: "full_doc".into(),
        },
        &CallerScope::default(),
    )?
    .expect("kb_fetch returned None");
    let full = out.full_doc.expect("full_doc not populated");
    assert!(full.contains("body one"));
    assert!(full.contains("body two"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kb_fetch_unknown_chunk_returns_none() -> Result<()> {
    let fx = fixture();
    let ctx = search_ctx(&fx);
    let out = kb_fetch::run(
        &ctx,
        kb_fetch::KbFetchInput {
            chunk_id: "deadbeef".repeat(8), // not a real chunk
            expand: "none".into(),
        },
        &CallerScope::default(),
    )?;
    assert!(out.is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kb_similar_returns_other_docs_chunks() -> Result<()> {
    let fx = fixture();
    ingest(
        &fx,
        "# Cats\n\ncats prowl rooftops at night.",
        "cats.md",
        vec![],
    )
    .await;
    ingest(
        &fx,
        "# Astronomy\n\nstars orbit galactic centres.",
        "astro.md",
        vec![],
    )
    .await;
    let lsid_cats = doc_lsid(&fx, 0);
    let seed_chunk = first_chunk_id(&fx, &lsid_cats);
    let ctx = search_ctx(&fx);
    let out = kb_similar::run(
        &ctx,
        kb_similar::KbSimilarInput {
            chunk_id: seed_chunk.clone(),
            k: 5,
            scope: "any".into(),
            min_score: 0.0,
            exclude_neighbors: false,
        },
        &CallerScope::default(),
    )?;
    // The seed chunk must not appear in its own neighbours.
    assert!(!out.neighbors.iter().any(|n| n.chunk_id == seed_chunk));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kb_similar_with_unknown_seed_returns_empty() -> Result<()> {
    let fx = fixture();
    let ctx = search_ctx(&fx);
    let out = kb_similar::run(
        &ctx,
        kb_similar::KbSimilarInput {
            chunk_id: "nonexistent".into(),
            k: 5,
            scope: "any".into(),
            min_score: 0.0,
            exclude_neighbors: false,
        },
        &CallerScope::default(),
    )?;
    assert!(out.neighbors.is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kb_list_docs_filters_by_tag() -> Result<()> {
    let fx = fixture();
    ingest(&fx, "# A\n\nbody a.", "a.md", vec!["work".into()]).await;
    ingest(&fx, "# B\n\nbody b.", "b.md", vec!["personal".into()]).await;
    let ctx = search_ctx(&fx);
    let out = kb_list_docs::run(
        &ctx,
        kb_list_docs::KbListDocsInput {
            tags: vec!["work".into()],
            source_kind: None,
            limit: 50,
            cursor: None,
        },
        &CallerScope::default(),
    )?;
    assert_eq!(out.docs.len(), 1);
    assert_eq!(out.docs[0].title, "A");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kb_list_docs_pagination_via_cursor() -> Result<()> {
    let fx = fixture();
    for i in 0..5 {
        ingest(
            &fx,
            &format!("# Doc {i}\n\nbody {i}."),
            &format!("d{i}.md"),
            vec![],
        )
        .await;
    }
    let ctx = search_ctx(&fx);
    let page1 = kb_list_docs::run(
        &ctx,
        kb_list_docs::KbListDocsInput {
            tags: vec![],
            source_kind: None,
            limit: 2,
            cursor: None,
        },
        &CallerScope::default(),
    )?;
    assert_eq!(page1.docs.len(), 2);
    let cursor = page1.next_cursor.expect("no next_cursor on page 1");
    let page2 = kb_list_docs::run(
        &ctx,
        kb_list_docs::KbListDocsInput {
            tags: vec![],
            source_kind: None,
            limit: 2,
            cursor: Some(cursor),
        },
        &CallerScope::default(),
    )?;
    assert_eq!(page2.docs.len(), 2);
    // Pages must not overlap.
    let p1_ids: std::collections::HashSet<_> = page1.docs.iter().map(|d| &d.doc_id).collect();
    for d in &page2.docs {
        assert!(
            !p1_ids.contains(&d.doc_id),
            "page2 leaked a doc_id from page1: {}",
            d.doc_id
        );
    }
    Ok(())
}
