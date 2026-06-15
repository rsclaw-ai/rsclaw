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

use std::{collections::HashMap, path::Path, sync::RwLock};

use anyhow::{Context, Result};
use hnsw_rs::{
    api::AnnT,
    hnswio::HnswIo,
    prelude::{DistCosine, Hnsw},
};
use serde::{Deserialize, Serialize};

use crate::store::KbStore;

/// Default vector dimension when none is supplied (matches
/// `StubEmbedder`). Real embedders pass their own: bge-small-zh=512,
/// bge-base-zh=768, bge-m3 / Qwen3-Embedding-0.6B / remote=1024.
pub const DEFAULT_DIMENSION: usize = 1024;
const M: usize = 16;
const EF_CONSTRUCTION: usize = 200;
const MAX_NB_LAYER: usize = 16;
const EF_SEARCH: usize = 64;
const INITIAL_CAPACITY: usize = 10_000;

/// Basename used for hnsw_rs's two-file snapshot (`.hnsw.graph` +
/// `.hnsw.data`). Lives alongside the JSON meta sidecar at
/// `<dir>/<SNAPSHOT_NAME>.meta.json`.
const SNAPSHOT_NAME: &str = "snapshot";

/// Bumped on incompatible snapshot format changes. v1 = the layout
/// shipped at KB MVP. Future versions can fall back to rebuild from
/// redb on mismatch rather than panicking.
const SNAPSHOT_SCHEMA_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct HnswMeta {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    dimension: usize,
    id_to_chunk: Vec<String>,
}

fn default_schema_version() -> u32 {
    SNAPSHOT_SCHEMA_VERSION
}

pub struct HnswCache {
    inner: RwLock<HnswInner>,
    /// Vector dimension this cache accepts. Set at construction from
    /// the active embedder; all insert / search / restore paths
    /// validate against it.
    dimension: usize,
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
    /// Empty cache at the given vector dimension. Use `rebuild` to
    /// populate from redb.
    pub fn new(dimension: usize) -> Self {
        Self {
            inner: RwLock::new(HnswInner::empty()),
            dimension,
        }
    }

    /// Empty cache at `DEFAULT_DIMENSION` (1024). Back-compat shim for
    /// stub-embedder callers + existing tests.
    pub fn empty() -> Self {
        Self::new(DEFAULT_DIMENSION)
    }

    /// The vector dimension this cache was built for.
    pub fn dim(&self) -> usize {
        self.dimension
    }

    /// Cosine similarity search; returns `(chunk_id, score)` pairs
    /// sorted by descending score. `score` is `1 - cosine_distance`
    /// so higher = more similar.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(String, f32)> {
        let inner = self.inner.read().unwrap_or_else(|p| p.into_inner());
        if inner.id_to_chunk.is_empty() || query.len() != self.dimension {
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
        if vector.len() != self.dimension {
            return Err(anyhow::anyhow!(
                "hnsw insert: expected dim={}, got {}",
                self.dimension,
                vector.len()
            ));
        }
        let mut inner = self.inner.write().unwrap_or_else(|p| p.into_inner());
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
        let mut dim_skipped = 0usize;
        {
            use redb::ReadableTable;

            use crate::{
                model::KbChunk,
                store::{codec::decode, schema::KB_CHUNKS},
            };
            let tbl = rtx.open_table(KB_CHUNKS)?;
            for entry in tbl.iter()? {
                let (_, v) = entry?;
                let c: KbChunk = decode(v.value())?;
                if c.vector.len() != self.dimension {
                    dim_skipped += 1;
                    continue;
                }
                let seq = id_to_chunk.len();
                chunk_to_id.insert(c.id.clone(), seq);
                id_to_chunk.push(c.id.clone());
                vectors.push(c.vector);
            }
        }
        // Dimension-mismatched chunks are dropped above. A WHOLESALE drop
        // means the dense leg is dead and hybrid silently degrades to
        // bm25-only — exactly what a flaky remote embedder produces
        // (zero/short vectors persisted, rebuild quietly yields n=0).
        // Surface it loudly instead of letting recall rot in silence.
        if dim_skipped > 0 {
            tracing::warn!(
                dim_skipped,
                kept = vectors.len(),
                expected_dim = self.dimension,
                "kb hnsw: dropped chunks with wrong vector dimension — dense recall degraded; \
                 likely a remote embedder returned bad vectors during ingest (re-ingest once \
                 the endpoint is healthy)"
            );
        }
        let capacity = INITIAL_CAPACITY.max(vectors.len() * 2);
        let hnsw = Hnsw::<'static, f32, DistCosine>::new(
            M,
            capacity,
            MAX_NB_LAYER,
            EF_CONSTRUCTION,
            DistCosine,
        );
        let inserts: Vec<(&Vec<f32>, usize)> =
            vectors.iter().enumerate().map(|(i, v)| (v, i)).collect();
        hnsw.parallel_insert(&inserts);
        let new_inner = HnswInner {
            hnsw,
            id_to_chunk,
            chunk_to_id,
        };
        let n = new_inner.id_to_chunk.len();
        *self.inner.write().unwrap_or_else(|p| p.into_inner()) = new_inner;
        tracing::info!(n, "kb hnsw: rebuild complete");
        Ok(())
    }

    /// Number of vectors currently in the cache. Test/debug helper.
    pub fn len(&self) -> usize {
        self.inner.read().unwrap_or_else(|p| p.into_inner()).id_to_chunk.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshot the current state to `<dir>/snapshot.hnsw.{graph,data}`
    /// + a `snapshot.meta.json` sidecar carrying the `id_to_chunk` map.
    /// Caller is responsible for ensuring `dir` exists. Empty caches
    /// still write a meta file so restore is symmetric.
    pub fn snapshot(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create_dir_all {}", dir.display()))?;
        let inner = self.inner.read().unwrap_or_else(|p| p.into_inner());
        // hnsw_rs's `file_dump` panics if there are zero data points,
        // so skip it for the empty case — we still write meta so
        // `restore` can detect an intentional empty snapshot.
        if !inner.id_to_chunk.is_empty() {
            inner
                .hnsw
                .file_dump(dir, SNAPSHOT_NAME)
                .map_err(|e| anyhow::anyhow!("hnsw file_dump: {e}"))?;
        }
        let meta = HnswMeta {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            dimension: self.dimension,
            id_to_chunk: inner.id_to_chunk.clone(),
        };
        let meta_path = dir.join(format!("{SNAPSHOT_NAME}.meta.json"));
        std::fs::write(&meta_path, serde_json::to_vec(&meta)?)
            .with_context(|| format!("write {}", meta_path.display()))?;
        tracing::info!(
            n = inner.id_to_chunk.len(),
            dir = %dir.display(),
            "kb hnsw: snapshot written"
        );
        Ok(())
    }

    /// Try to load a previously-written snapshot. Returns `Ok(true)`
    /// on success, `Ok(false)` when no snapshot exists (caller should
    /// fall back to `rebuild`). Errors when the snapshot is present
    /// but corrupt or has a different dimension.
    pub fn restore(&self, dir: &Path) -> Result<bool> {
        let meta_path = dir.join(format!("{SNAPSHOT_NAME}.meta.json"));
        let graph_path = dir.join(format!("{SNAPSHOT_NAME}.hnsw.graph"));
        let data_path = dir.join(format!("{SNAPSHOT_NAME}.hnsw.data"));
        if !meta_path.exists() {
            return Ok(false);
        }
        let meta_bytes =
            std::fs::read(&meta_path).with_context(|| format!("read {}", meta_path.display()))?;
        let meta: HnswMeta = serde_json::from_slice(&meta_bytes)
            .with_context(|| format!("decode {}", meta_path.display()))?;
        if meta.schema_version != SNAPSHOT_SCHEMA_VERSION {
            return Err(anyhow::anyhow!(
                "snapshot schema_version={} is incompatible with runtime version={SNAPSHOT_SCHEMA_VERSION} \
                 — delete the hnsw/ directory to force a rebuild from redb",
                meta.schema_version
            ));
        }
        if meta.dimension != self.dimension {
            return Err(anyhow::anyhow!(
                "snapshot dim={} does not match runtime dim={}",
                meta.dimension,
                self.dimension
            ));
        }
        let n = meta.id_to_chunk.len();
        if n == 0 {
            // Empty snapshot: clear inner state, skip hnsw load.
            let mut inner = self.inner.write().unwrap_or_else(|p| p.into_inner());
            *inner = HnswInner::empty();
            tracing::info!(dir = %dir.display(), "kb hnsw: restored empty snapshot");
            return Ok(true);
        }
        if !graph_path.exists() || !data_path.exists() {
            return Err(anyhow::anyhow!(
                "snapshot meta present but graph/data files missing in {}",
                dir.display()
            ));
        }
        // hnsw_rs's `load_hnsw` returns `Hnsw<'b, T, D>` borrowed from
        // the `HnswIo` instance. To store the loaded handle in our
        // `'static`-bounded `HnswInner`, we `Box::leak` the HnswIo so
        // the mmap'd backing data outlives any subsequent search.
        // This is a one-shot startup cost; a later `rebuild()`
        // replaces the inner hnsw with a freshly-constructed one and
        // the leaked HnswIo simply remains as unused mmap pages
        // (kernel will reclaim them when the file is unmapped on
        // process exit). Acceptable trade for wait-free restores.
        let reloader_box: Box<HnswIo> = Box::new(HnswIo::new(dir, SNAPSHOT_NAME));
        let reloader: &'static mut HnswIo = Box::leak(reloader_box);
        let hnsw: Hnsw<'static, f32, DistCosine> = reloader
            .load_hnsw::<f32, DistCosine>()
            .map_err(|e| anyhow::anyhow!("hnsw load: {e}"))?;
        let mut chunk_to_id = HashMap::with_capacity(n);
        for (i, id) in meta.id_to_chunk.iter().enumerate() {
            chunk_to_id.insert(id.clone(), i);
        }
        let new_inner = HnswInner {
            hnsw,
            id_to_chunk: meta.id_to_chunk,
            chunk_to_id,
        };
        *self.inner.write().unwrap_or_else(|p| p.into_inner()) = new_inner;
        tracing::info!(n, dir = %dir.display(), "kb hnsw: snapshot restored");
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;
    use crate::{
        canonicalize::{CanonicalizeInput, canonicalize_by_mime},
        embedder::{KbEmbedder, StubEmbedder},
        paths::KbPaths,
        pipeline::{IngestInput, ingest_canonicalized},
        store::KbStore,
        worker::{DefaultDispatcher, WorkerConfig, WorkerPool, handlers::HandlerCtx},
    };

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
        let index = Arc::new(crate::index::KbIndex::open(&paths).unwrap());
        let ctx = HandlerCtx {
            store: store.clone(),
            paths,
            embedder,
            index,
        };
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
        let q = vec![0.0_f32; DEFAULT_DIMENSION];
        let hits = cache.search(&q, 5);
        assert!(!hits.is_empty(), "expected hits, got empty");
    }

    #[test]
    fn search_on_empty_returns_empty() {
        let cache = HnswCache::empty();
        assert!(cache.search(&vec![0.0; DEFAULT_DIMENSION], 5).is_empty());
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
        let v1 = vec![1.0_f32; DEFAULT_DIMENSION];
        let mut v2 = vec![0.0_f32; DEFAULT_DIMENSION];
        v2[0] = 1.0; // pretty different
        cache.insert("c1", &v1).unwrap();
        cache.insert("c1", &v2).unwrap();
        let hits = cache.search(&v2, 1);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].0, "c1");
    }

    #[test]
    fn snapshot_roundtrip_preserves_search() {
        let dir = TempDir::new().unwrap();
        let dump_dir = dir.path().join("snap");
        let cache = HnswCache::empty();
        // Build a few orthogonal vectors so search has signal.
        let mut v_alpha = vec![0.0_f32; DEFAULT_DIMENSION];
        v_alpha[0] = 1.0;
        let mut v_beta = vec![0.0_f32; DEFAULT_DIMENSION];
        v_beta[1] = 1.0;
        cache.insert("alpha", &v_alpha).unwrap();
        cache.insert("beta", &v_beta).unwrap();
        cache.snapshot(&dump_dir).unwrap();

        // Restore into a fresh cache; search must still find alpha first.
        let restored = HnswCache::empty();
        assert!(restored.restore(&dump_dir).unwrap());
        assert_eq!(restored.len(), 2);
        let hits = restored.search(&v_alpha, 1);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].0, "alpha");
    }

    #[test]
    fn restore_returns_false_when_no_snapshot() {
        let dir = TempDir::new().unwrap();
        let cache = HnswCache::empty();
        assert!(!cache.restore(dir.path()).unwrap());
    }

    #[test]
    fn restore_rejects_incompatible_schema_version() {
        let dir = TempDir::new().unwrap();
        let dump_dir = dir.path().join("snap");
        std::fs::create_dir_all(&dump_dir).unwrap();
        // Hand-write a meta file claiming an unknown future version.
        let bad_meta = serde_json::json!({
            "schema_version": 999,
            "dimension": DEFAULT_DIMENSION,
            "id_to_chunk": ["c1"],
        });
        std::fs::write(
            dump_dir.join("snapshot.meta.json"),
            serde_json::to_vec(&bad_meta).unwrap(),
        )
        .unwrap();
        let cache = HnswCache::empty();
        let err = cache.restore(&dump_dir).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("schema_version"),
            "expected schema_version mismatch error, got: {msg}"
        );
    }

    #[test]
    fn snapshot_empty_cache_writes_meta_only() {
        let dir = TempDir::new().unwrap();
        let dump_dir = dir.path().join("empty");
        let cache = HnswCache::empty();
        cache.snapshot(&dump_dir).unwrap();
        assert!(dump_dir.join("snapshot.meta.json").exists());
        // The hnsw_rs files MUST NOT be written for empty caches
        // (file_dump panics if there's no data).
        assert!(!dump_dir.join("snapshot.hnsw.graph").exists());
        // Restoring round-trips to an empty cache.
        let restored = HnswCache::empty();
        assert!(restored.restore(&dump_dir).unwrap());
        assert_eq!(restored.len(), 0);
    }
}
