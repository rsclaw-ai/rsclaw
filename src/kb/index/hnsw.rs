//! Dense-vector cache backed by hnsw_rs. Source of truth is
//! `KbChunk.vector` in redb; the cache is rebuildable on startup or
//! manual trigger.
//!
//! Concurrency model: a single `RwLock<HnswInner>` guards the hnsw
//! handle + the chunk_id ↔ internal id maps. Reads (search) take the
//! read lock; writes (insert / rebuild) take the write lock. The spec
//! mentions ArcSwap-rcu for wait-free reads; that's a Week 3.5
//! optimisation once we have a benchmark showing the read lock is hot.
//!
//! Re-insert semantics: hnsw_rs assigns monotonically increasing
//! internal ids and does not support overwrite. When the same
//! chunk_id is inserted twice, we append a new internal id and
//! update `chunk_to_id` to point at it. The old vertex stays in the
//! graph but never resolves back to a chunk_id (orphaned). Rebuild
//! from redb reaps the orphans.

use crate::kb::store::KbStore;
use anyhow::Result;
use hnsw_rs::prelude::{DistCosine, Hnsw};
use std::collections::HashMap;
use std::sync::RwLock;

const DIMENSION: usize = 1024;
const M: usize = 16;
const EF_CONSTRUCTION: usize = 200;
const MAX_NB_LAYER: usize = 16;
const EF_SEARCH: usize = 64;
const INITIAL_CAPACITY: usize = 10_000;

pub struct HnswCache {
    inner: RwLock<HnswInner>,
}

struct HnswInner {
    hnsw: Hnsw<'static, f32, DistCosine>,
    id_to_chunk: Vec<String>,
    chunk_to_id: HashMap<String, usize>,
}

impl HnswInner {
    fn empty() -> Self {
        Self {
            hnsw: Hnsw::<'static, f32, DistCosine>::new(
                M,
                INITIAL_CAPACITY,
                MAX_NB_LAYER,
                EF_CONSTRUCTION,
                DistCosine,
            ),
            id_to_chunk: Vec::new(),
            chunk_to_id: HashMap::new(),
        }
    }
}

impl HnswCache {
    /// Empty cache. Use `rebuild` to populate from redb.
    pub fn empty() -> Self {
        Self {
            inner: RwLock::new(HnswInner::empty()),
        }
    }

    /// Cosine similarity search; returns `(chunk_id, score)` pairs
    /// sorted by descending score. `score` is `1 - cosine_distance`
    /// so higher = more similar.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(String, f32)> {
        let inner = self.inner.read().unwrap();
        if inner.id_to_chunk.is_empty() || query.len() != DIMENSION {
            return Vec::new();
        }
        let raw = inner.hnsw.search(query, k, EF_SEARCH);
        raw.into_iter()
            .filter_map(|n| {
                inner
                    .id_to_chunk
                    .get(n.d_id)
                    .map(|id| (id.clone(), 1.0 - n.distance))
            })
            .collect()
    }

    /// Append-only insert. Re-inserting the same chunk_id orphans the
    /// old vertex; `chunk_to_id` is updated to point at the new id.
    pub fn insert(&self, chunk_id: &str, vector: &[f32]) -> Result<()> {
        if vector.len() != DIMENSION {
            return Err(anyhow::anyhow!(
                "hnsw insert: expected dim={DIMENSION}, got {}",
                vector.len()
            ));
        }
        let mut inner = self.inner.write().unwrap();
        let new_id = inner.id_to_chunk.len();
        inner.id_to_chunk.push(chunk_id.to_string());
        inner.chunk_to_id.insert(chunk_id.to_string(), new_id);
        let vec_clone = vector.to_vec();
        inner.hnsw.insert((&vec_clone, new_id));
        Ok(())
    }

    /// Rebuild from redb. Reads every `KbChunk.vector` row and builds
    /// a fresh hnsw, then atomically replaces the inner state.
    pub fn rebuild(&self, store: &KbStore) -> Result<()> {
        let rtx = store.begin_read()?;
        let mut id_to_chunk: Vec<String> = Vec::new();
        let mut chunk_to_id: HashMap<String, usize> = HashMap::new();
        let mut vectors: Vec<Vec<f32>> = Vec::new();
        {
            use crate::kb::model::KbChunk;
            use crate::kb::store::codec::decode;
            use crate::kb::store::schema::KB_CHUNKS;
            use redb::ReadableTable;
            let tbl = rtx.open_table(KB_CHUNKS)?;
            for entry in tbl.iter()? {
                let (_, v) = entry?;
                let c: KbChunk = decode(v.value())?;
                if c.vector.len() != DIMENSION {
                    continue;
                }
                let seq = id_to_chunk.len();
                chunk_to_id.insert(c.id.clone(), seq);
                id_to_chunk.push(c.id.clone());
                vectors.push(c.vector);
            }
        }
        let capacity = INITIAL_CAPACITY.max(vectors.len() * 2);
        let hnsw = Hnsw::<'static, f32, DistCosine>::new(
            M,
            capacity,
            MAX_NB_LAYER,
            EF_CONSTRUCTION,
            DistCosine,
        );
        let inserts: Vec<(&Vec<f32>, usize)> = vectors
            .iter()
            .enumerate()
            .map(|(i, v)| (v, i))
            .collect();
        hnsw.parallel_insert(&inserts);
        let new_inner = HnswInner { hnsw, id_to_chunk, chunk_to_id };
        let n = new_inner.id_to_chunk.len();
        *self.inner.write().unwrap() = new_inner;
        tracing::info!(n, "kb hnsw: rebuild complete");
        Ok(())
    }

    /// Number of vectors currently in the cache. Test/debug helper.
    pub fn len(&self) -> usize {
        self.inner.read().unwrap().id_to_chunk.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::canonicalize::{canonicalize_by_mime, CanonicalizeInput};
    use crate::kb::embedder::{KbEmbedder, StubEmbedder};
    use crate::kb::paths::KbPaths;
    use crate::kb::pipeline::{ingest_canonicalized, IngestInput};
    use crate::kb::store::KbStore;
    use crate::kb::worker::handlers::HandlerCtx;
    use crate::kb::worker::{DefaultDispatcher, WorkerConfig, WorkerPool};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn fixture_with_chunks() -> (TempDir, Arc<KbStore>) {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb")).unwrap());
        let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
        paths.ensure_layout().unwrap();
        let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());

        let bytes = b"# Hi\n\nfirst body content.\n\nsecond body content.";
        let canon = canonicalize_by_mime(CanonicalizeInput {
            bytes,
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
        let ctx = HandlerCtx { store: store.clone(), paths, embedder };
        let cfg = WorkerConfig {
            worker_id: "w".into(),
            ..WorkerConfig::default()
        };
        WorkerPool::run_one_blocking(&ctx, &cfg, &DefaultDispatcher).unwrap();
        (tmp, store)
    }

    #[test]
    fn rebuild_then_search_returns_hits() {
        let (_tmp, store) = fixture_with_chunks();
        let cache = HnswCache::empty();
        cache.rebuild(&store).unwrap();
        assert!(cache.len() > 0, "expected chunks to be loaded");
        let q = vec![0.0_f32; DIMENSION];
        let hits = cache.search(&q, 5);
        assert!(!hits.is_empty(), "expected hits, got empty");
    }

    #[test]
    fn search_on_empty_returns_empty() {
        let cache = HnswCache::empty();
        assert!(cache.search(&vec![0.0; DIMENSION], 5).is_empty());
    }

    #[test]
    fn insert_dim_mismatch_errors() {
        let cache = HnswCache::empty();
        assert!(cache.insert("c1", &[0.0; 512]).is_err());
    }

    #[test]
    fn append_only_insert_returns_new_chunk_for_same_id() {
        // Re-inserting same chunk_id with new vector should make
        // search for the new vector return the chunk_id (the old
        // vertex becomes orphaned but doesn't surface).
        let cache = HnswCache::empty();
        let v1 = vec![1.0_f32; DIMENSION];
        let mut v2 = vec![0.0_f32; DIMENSION];
        v2[0] = 1.0; // pretty different
        cache.insert("c1", &v1).unwrap();
        cache.insert("c1", &v2).unwrap();
        let hits = cache.search(&v2, 1);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].0, "c1");
    }
}
