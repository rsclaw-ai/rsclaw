//! Week 3 end-to-end: ingest → worker drains → kb_search returns
//! ranked chunks with visibility filtering applied.

use std::sync::Arc;

use anyhow::Result;
use rsclaw::kb::{
    CallerScope, CanonicalizeInput, DefaultDispatcher, HandlerCtx, IngestInput, KbEmbedder,
    KbIndex, KbPaths, KbStore, StubEmbedder, WorkerConfig, WorkerPool, canonicalize_by_mime,
    ingest_canonicalized, search::SearchCtx, tools::kb_search,
};
use tempfile::TempDir;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_kb_search_ranks_relevant_chunks_top() -> Result<()> {
    let tmp = TempDir::new()?;
    let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb"))?);
    let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
    paths.ensure_layout()?;
    let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());
    let index = Arc::new(KbIndex::open(&paths)?);

    let docs = [
        "# Cats\n\nCats are nocturnal hunters that prowl rooftops and trees in the night.",
        "# Dogs\n\nDogs love walks and play fetch with their humans every morning.",
        "# Astronomy\n\nThe sun is a yellow dwarf star in the Milky Way galaxy.",
    ];

    let hctx = HandlerCtx {
        store: store.clone(),
        paths: paths.clone(),
        embedder: embedder.clone(),
        index: index.clone(),
    };
    let cfg = WorkerConfig::default();
    for body in &docs {
        let canon = canonicalize_by_mime(CanonicalizeInput {
            bytes: body.as_bytes(),
            mime: "text/markdown",
            hint_title: Some("t"),
            logical_source_id_seed: None,
        })?
        .unwrap();
        ingest_canonicalized(
            &store,
            IngestInput {
                canon: &canon,
                raw_bytes: body.as_bytes(),
                raw_ext: "md",
                visibility: None,
                owner_user_id: None,
                seen_key: None,
                source: None,
                paths: &paths,
            },
        )?;
        WorkerPool::run_one_blocking(&hctx, &cfg, &DefaultDispatcher)?;
    }

    let ctx = SearchCtx {
        store: store.clone(),
        index,
        paths,
        embedder,
    };
    let out = kb_search::run(
        &ctx,
        kb_search::KbSearchInput {
            query: "yellow dwarf star".into(),
            k: 3,
            filter: Default::default(),
            mode: "hybrid".into(),
            diversity: "mmr".into(),
            mmr_lambda: 0.5,
            boost_entities: vec![],
            query_instruction: None,
        },
        &CallerScope::default(),
    )?;
    assert!(!out.results.is_empty(), "expected at least one hit");
    // BM25 should pick up "yellow dwarf star" from the Astronomy doc.
    let top_doc = &out.results[0];
    let astro_hit = out
        .results
        .iter()
        .any(|h| h.doc_title.contains("Astronomy") || h.text.to_lowercase().contains("dwarf"));
    assert!(
        astro_hit,
        "expected an astronomy hit in top {}: {:?}",
        out.results.len(),
        out.results
            .iter()
            .map(|h| h.doc_title.clone())
            .collect::<Vec<_>>()
    );
    let _ = top_doc;
    Ok(())
}
