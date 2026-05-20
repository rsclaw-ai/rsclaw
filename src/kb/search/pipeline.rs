//! kb_search pipeline: dense + sparse → filter → fuse → boost → mmr → fetch.

use crate::kb::content_store::read::read_doc_range;
use crate::kb::embedder::KbEmbedder;
use crate::kb::index::KbIndex;
use crate::kb::model::{CallerScope, KbChunk, KbDoc, KbLocator, KbSource};
use crate::kb::paths::KbPaths;
use crate::kb::search::filter::{is_latest_version, keep_doc, SearchFilter};
use crate::kb::search::mmr::{mmr_select, MmrCandidate};
use crate::kb::search::rrf::rrf_fuse;
use crate::kb::store::{chunks, docs, entities, KbStore};
use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub k: usize,
    pub filter: SearchFilter,
    pub mode: SearchMode,
    pub diversity: Diversity,
    pub mmr_lambda: f32,
    pub boost_entities: Vec<String>,
    /// Asymmetric-embedding query instruction (Qwen3), applied to the dense
    /// query only. `None` = symmetric (the query is embedded as-is).
    pub query_instruction: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Auto,
    Dense,
    Bm25,
    Hybrid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Diversity {
    Off,
    Mmr,
}

#[derive(Debug, Clone, Serialize)]
pub struct RetrievalHit {
    pub chunk_id: String,
    pub doc_id: String,
    pub doc_title: String,
    pub text: String,
    pub heading_path: Vec<String>,
    pub score: f32,
    pub citation: Citation,
    pub entities: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Citation {
    pub source: String,
    pub locator_human: String,
    pub locator_machine: KbLocator,
}

pub struct SearchCtx {
    pub store: Arc<KbStore>,
    pub index: Arc<KbIndex>,
    pub paths: Arc<KbPaths>,
    pub embedder: Arc<dyn KbEmbedder>,
}

impl SearchCtx {
    pub fn search(&self, req: &SearchRequest, scope: &CallerScope) -> Result<Vec<RetrievalHit>> {
        let recall_k = (req.k * 3).max(10);

        // 1. Dense recall.
        let dense = match req.mode {
            SearchMode::Bm25 => Vec::new(),
            _ => {
                // Dense side applies the asymmetric query instruction if set;
                // BM25 (below) always uses the raw query.
                let dense_query =
                    crate::embed::format_query(req.query_instruction.as_deref(), &req.query);
                let qv = self.embedder.embed_batch(&[dense_query])?;
                match qv.first() {
                    Some(qvec) => self.index.hnsw.search(qvec, recall_k),
                    // Embedder returned no vector — skip dense recall rather
                    // than panic; sparse recall still runs in Auto/Hybrid.
                    None => Vec::new(),
                }
            }
        };

        // 2. Sparse recall.
        let sparse = match req.mode {
            SearchMode::Dense => Vec::new(),
            _ => self.index.tantivy.search(&req.query, recall_k)?,
        };

        // 3. Filter (visibility + status + version + tags + source_kind + doc_ids).
        let rtx = self.store.begin_read()?;
        let mut materialised: HashMap<String, (KbChunk, KbDoc)> = HashMap::new();

        let keep = |cid: &str, materialised: &mut HashMap<String, (KbChunk, KbDoc)>|
        -> Result<bool> {
            if materialised.contains_key(cid) {
                return Ok(true);
            }
            let c = match chunks::get(&rtx, cid)? {
                Some(c) => c,
                None => return Ok(false),
            };
            let d = match docs::get(&rtx, &c.doc_id)? {
                Some(d) => d,
                None => return Ok(false),
            };
            if !keep_doc(&d, scope, &req.filter) {
                return Ok(false);
            }
            if !is_latest_version(&rtx, &d)? {
                return Ok(false);
            }
            materialised.insert(cid.to_string(), (c, d));
            Ok(true)
        };

        let mut kept_dense: Vec<(String, f32)> = Vec::new();
        for (cid, score) in &dense {
            if keep(cid, &mut materialised)? {
                kept_dense.push((cid.clone(), *score));
            }
        }
        let mut kept_sparse: Vec<(String, f32)> = Vec::new();
        for (cid, score) in &sparse {
            if keep(cid, &mut materialised)? {
                kept_sparse.push((cid.clone(), *score));
            }
        }

        // 4. Fuse.
        let mut fused = match req.mode {
            SearchMode::Dense => kept_dense,
            SearchMode::Bm25 => kept_sparse,
            _ => rrf_fuse(&[&kept_dense, &kept_sparse]),
        };

        // 5a. require_entities: drop fused hits whose chunk_id is
        //     not in EVERY required entity's chunk set.
        //     (Entity edges are populated by the chunk_embed handler's
        //     regex extractor — `KbEntityIndex` rows keyed
        //     `entity_id\0chunk_id`.)
        if !req.filter.require_entities.is_empty() {
            let mut required_sets: Vec<std::collections::HashSet<String>> = Vec::new();
            for eid in &req.filter.require_entities {
                let set: std::collections::HashSet<String> = entities::chunks_for_entity(&rtx, eid)?
                    .into_iter()
                    .map(|e| e.chunk_id)
                    .collect();
                required_sets.push(set);
            }
            fused.retain(|(cid, _)| required_sets.iter().all(|s| s.contains(cid)));
        }

        // 5b. boost_entities: multiply each fused hit's score by
        //     `BOOST_FACTOR` for every boost entity it mentions.
        //     Bounded so adding more boost entities has diminishing
        //     impact (1.0 + Σ min(boost_factor, ...)). Re-sort.
        const BOOST_FACTOR: f32 = 0.2;
        if !req.boost_entities.is_empty() {
            let mut boost_sets: Vec<std::collections::HashSet<String>> = Vec::new();
            for eid in &req.boost_entities {
                let set: std::collections::HashSet<String> = entities::chunks_for_entity(&rtx, eid)?
                    .into_iter()
                    .map(|e| e.chunk_id)
                    .collect();
                boost_sets.push(set);
            }
            for (cid, score) in fused.iter_mut() {
                let bonus: f32 = boost_sets
                    .iter()
                    .map(|s| if s.contains(cid) { BOOST_FACTOR } else { 0.0 })
                    .sum();
                *score *= 1.0 + bonus;
            }
            fused.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.0.cmp(&b.0))
            });
        }

        // 6. MMR.
        let mut final_ids: Vec<(String, f32)> = match req.diversity {
            Diversity::Off => fused.into_iter().take(req.k).collect(),
            Diversity::Mmr => {
                let candidates: Vec<MmrCandidate> = fused
                    .iter()
                    .filter_map(|(id, sc)| {
                        materialised.get(id).map(|(c, _)| MmrCandidate {
                            chunk_id: id.clone(),
                            relevance: *sc,
                            vector: c.vector.as_slice(),
                        })
                    })
                    .collect();
                mmr_select(candidates, req.k, req.mmr_lambda)
            }
        };
        // Spec §3 KV-cache friendliness: stable order across calls.
        // RRF already breaks ties deterministically; MMR's greedy
        // picker can produce equal-score ties — apply (score desc,
        // chunk_id asc) one more time so the same inputs always
        // produce the same on-the-wire byte sequence.
        final_ids.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });

        // 7. Lazy text fetch + build hits.
        let mut hits = Vec::with_capacity(final_ids.len());
        for (chunk_id, score) in final_ids {
            let (c, d) = match materialised.get(&chunk_id) {
                Some(p) => p,
                None => continue,
            };
            let abs = self.paths.root.join(&d.markdown_path);
            // Body-read failures degrade to empty text rather than
            // erroring the whole search, but log them — usually means
            // the file was reaped by the compactor in a race, which
            // should be rare and worth knowing about.
            let text = match read_doc_range(&abs, c.byte_offset.0, c.byte_offset.1) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(
                        chunk = %crate::kb::redact(&chunk_id),
                        path = %abs.display(),
                        "kb search: chunk body read failed: {e}"
                    );
                    String::new()
                }
            };
            hits.push(RetrievalHit {
                chunk_id,
                doc_id: d.id.clone(),
                doc_title: d.title.clone(),
                text,
                heading_path: c.heading_path.clone(),
                score,
                citation: Citation {
                    source: render_source(d),
                    locator_human: c.locator.human(),
                    locator_machine: c.locator.clone(),
                },
                entities: Vec::new(),
            });
        }
        Ok(hits)
    }
}

fn render_source(d: &KbDoc) -> String {
    match &d.source {
        KbSource::Doc { path } => format!("file://{}", path.display()),
        KbSource::Url { url, .. } => url.clone(),
        KbSource::Img { path } => format!("file://{}", path.display()),
        _ => d.title.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::canonicalize::{canonicalize_by_mime, CanonicalizeInput};
    use crate::kb::embedder::{KbEmbedder, StubEmbedder};
    use crate::kb::model::KbVisibility;
    use crate::kb::pipeline::{ingest_canonicalized, IngestInput};
    use crate::kb::worker::handlers::HandlerCtx;
    use crate::kb::worker::{DefaultDispatcher, WorkerConfig, WorkerPool};
    use tempfile::TempDir;

    fn ctx_with_ingested(body: &str) -> (TempDir, SearchCtx) {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb")).unwrap());
        let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
        paths.ensure_layout().unwrap();
        let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());
        let index = Arc::new(KbIndex::open(&paths).unwrap());

        let canon = canonicalize_by_mime(CanonicalizeInput {
            bytes: body.as_bytes(),
            mime: "text/markdown",
            hint_title: Some("t"),
            logical_source_id_seed: None,
        })
        .unwrap()
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
        )
        .unwrap();
        let hctx = HandlerCtx {
            store: store.clone(),
            paths: paths.clone(),
            embedder: embedder.clone(),
            index: index.clone(),
        };
        let cfg = WorkerConfig { worker_id: "w".into(), ..WorkerConfig::default() };
        WorkerPool::run_one_blocking(&hctx, &cfg, &DefaultDispatcher).unwrap();
        (
            tmp,
            SearchCtx { store, index, paths, embedder },
        )
    }

    #[test]
    fn search_returns_hits_for_indexed_body() {
        let (_tmp, ctx) = ctx_with_ingested("# Greeting\n\nThe quick brown fox jumps over.");
        let req = SearchRequest {
            query: "brown fox".into(),
            k: 5,
            filter: SearchFilter::default(),
            mode: SearchMode::Hybrid,
            diversity: Diversity::Mmr,
            mmr_lambda: 0.5,
            boost_entities: vec![],
            query_instruction: None,
        };
        let hits = ctx.search(&req, &CallerScope::default()).unwrap();
        assert!(!hits.is_empty(), "expected at least one hit");
    }

    #[test]
    fn search_output_is_deterministic_across_calls() {
        // Spec §3 KV-cache friendliness: same inputs MUST produce
        // the same chunk_id sequence on the wire. MMR's greedy
        // picker can tie on score; the post-MMR `(score desc,
        // chunk_id asc)` sort makes the tiebreak stable.
        let (_tmp, ctx) = ctx_with_ingested(
            "# A\n\npara one.\n\npara two.\n\npara three.\n\npara four.\n\npara five.",
        );
        let req = SearchRequest {
            query: "para".into(),
            k: 3,
            filter: SearchFilter::default(),
            mode: SearchMode::Hybrid,
            diversity: Diversity::Mmr,
            mmr_lambda: 0.5,
            boost_entities: vec![],
            query_instruction: None,
        };
        let first: Vec<String> = ctx
            .search(&req, &CallerScope::default())
            .unwrap()
            .into_iter()
            .map(|h| h.chunk_id)
            .collect();
        for _ in 0..3 {
            let again: Vec<String> = ctx
                .search(&req, &CallerScope::default())
                .unwrap()
                .into_iter()
                .map(|h| h.chunk_id)
                .collect();
            assert_eq!(first, again, "search order not stable across calls");
        }
    }

    #[test]
    fn search_filter_by_visibility_hides_private() {
        let (_tmp, ctx) = ctx_with_ingested("# Secret\n\nclassified info goes here.");
        // Re-tag the existing doc as Private + owner u1.
        let rtx = ctx.store.begin_read().unwrap();
        let all: Vec<KbDoc> = {
            use crate::kb::store::codec::decode;
            use crate::kb::store::schema::KB_DOCS;
            use redb::ReadableTable;
            let tbl = rtx.open_table(KB_DOCS).unwrap();
            let mut out = Vec::new();
            for e in tbl.iter().unwrap() {
                let (_, v) = e.unwrap();
                out.push(decode(v.value()).unwrap());
            }
            out
        };
        drop(rtx);
        let mut d = all.into_iter().next().unwrap();
        d.visibility = KbVisibility::Private;
        d.owner_user_id = Some("u1".into());
        {
            let wtx = ctx.store.begin_write().unwrap();
            crate::kb::store::docs::put(&wtx, &d).unwrap();
            wtx.commit().unwrap();
        }
        let req = SearchRequest {
            query: "classified".into(),
            k: 5,
            filter: SearchFilter::default(),
            mode: SearchMode::Hybrid,
            diversity: Diversity::Off,
            mmr_lambda: 0.5,
            boost_entities: vec![],
            query_instruction: None,
        };
        let scope_other = CallerScope { user_id: Some("u2".into()), ..Default::default() };
        let hits = ctx.search(&req, &scope_other).unwrap();
        assert!(hits.is_empty(), "Private doc must not leak to other user");
        let scope_owner = CallerScope { user_id: Some("u1".into()), ..Default::default() };
        let hits = ctx.search(&req, &scope_owner).unwrap();
        assert!(!hits.is_empty(), "owner must see their own Private doc");
    }
}
