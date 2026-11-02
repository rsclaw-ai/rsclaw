//! KbIndex — composite of dense (hnsw) + sparse (tantivy) caches.
//! Both layers are caches over redb; rebuild from redb is the
//! canonical recovery path.

pub mod cjk;
pub mod hnsw;
pub mod rebuild;
pub mod tantivy;

use std::collections::HashSet;

use anyhow::Result;
pub use cjk::JiebaTokenizer;
pub use hnsw::{HnswCache, IndexFingerprint};
pub use tantivy::TantivyIndex;

use crate::{paths::KbPaths, store::KbStore};

pub struct KbIndex {
    pub hnsw: HnswCache,
    pub tantivy: TantivyIndex,
}

impl KbIndex {
    /// Open both layers at `DEFAULT_DIMENSION` (1024 — stub embedder).
    /// Existing callers + tests use this. Real-embedder callers use
    /// `open_with_dim`.
    pub fn open(paths: &KbPaths) -> Result<Self> {
        Self::open_with_dim(paths, hnsw::DEFAULT_DIMENSION)
    }

    /// Open both layers at the active embedder's vector dimension.
    /// `dim` MUST equal `embedder.dimension()` or chunk inserts /
    /// query searches will be rejected by the dim check.
    pub fn open_with_dim(paths: &KbPaths, dim: usize) -> Result<Self> {
        Self::open_for(paths, dim, None)
    }

    /// Open both layers at `dim`; the dense layer only admits chunks
    /// embedded by `embedder_id` (see `HnswCache::with_embedder`).
    pub fn open_for(paths: &KbPaths, dim: usize, embedder_id: Option<String>) -> Result<Self> {
        let tantivy = TantivyIndex::open_or_create(&paths.root.join("idx/tantivy"))?;
        Ok(Self {
            hnsw: HnswCache::with_embedder(dim, embedder_id),
            tantivy,
        })
    }

    /// Open + populate the dense layer at `DEFAULT_DIMENSION`.
    pub fn open_and_rebuild(paths: &KbPaths, store: &KbStore) -> Result<Self> {
        Self::open_and_rebuild_with_dim(paths, store, hnsw::DEFAULT_DIMENSION)
    }

    /// Open + populate both layers at `dim`. See `open_and_rebuild_for`.
    pub fn open_and_rebuild_with_dim(paths: &KbPaths, store: &KbStore, dim: usize) -> Result<Self> {
        Self::open_and_rebuild_for(paths, store, dim, None).map(|(idx, _)| idx)
    }

    /// Open + populate both layers, reusing on-disk state only when it is
    /// provably consistent with redb (the source of truth):
    ///   - HNSW: the snapshot is restored only if its manifest (chunk-set
    ///     fingerprint + embedder id) matches what redb holds now; otherwise
    ///     rebuilt. A stale snapshot used to be restored unconditionally,
    ///     silently missing every chunk ingested after the last compact.
    ///   - tantivy: rebuilt only when its live doc count differs from the
    ///     number of chunk rows (the full re-tokenization is the expensive
    ///     part of startup).
    ///
    /// Undecodable chunk rows are skipped with a warning instead of failing
    /// the whole KB. Returns the scan so the caller can re-embed docs whose
    /// chunks the dense layer rejected (`ChunkScan::stale_docs`).
    pub fn open_and_rebuild_for(
        paths: &KbPaths,
        store: &KbStore,
        dim: usize,
        embedder_id: Option<String>,
    ) -> Result<(Self, ChunkScan)> {
        let idx = Self::open_for(paths, dim, embedder_id)?;
        let scan = scan_chunks(store, &idx.hnsw)?;
        let snapshot_dir = paths.root.join("hnsw");
        let restored = match idx.hnsw.restore_validated(&snapshot_dir, &scan.dense) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("kb hnsw: snapshot restore failed, rebuilding from redb: {e:#}");
                false
            }
        };
        if !restored {
            idx.hnsw.rebuild(store)?;
        }
        let live = idx.tantivy.num_docs();
        if live == scan.total {
            tracing::info!(n = live, "kb tantivy: index consistent with redb, skipping rebuild");
        } else {
            tracing::info!(
                indexed = live,
                chunks = scan.total,
                "kb tantivy: index out of sync with redb, rebuilding"
            );
            idx.tantivy.rebuild(store)?;
        }
        Ok((idx, scan))
    }

    /// Drop purged chunks from both layers after a compactor tick: tantivy
    /// deletes by id, HNSW (no native delete) is rebuilt from redb, which
    /// also reaps orphaned vertices from re-inserts.
    pub fn apply_purge(&self, store: &KbStore, purged_chunk_ids: &[String]) -> Result<()> {
        for id in purged_chunk_ids {
            self.tantivy.delete(id);
        }
        self.tantivy.commit()?;
        self.hnsw.rebuild(store)?;
        Ok(())
    }

    /// Write a snapshot of the HNSW state under `<paths.root>/hnsw/`.
    /// Cheap to call; idempotent.
    pub fn snapshot_hnsw(&self, paths: &KbPaths) -> Result<()> {
        self.hnsw.snapshot(&paths.root.join("hnsw"))
    }

    /// Upsert a chunk into both indexes. Caller wraps multiple upserts
    /// in `commit()` to batch tantivy IO.
    pub fn upsert_chunk(&self, c: &crate::model::KbChunk) -> Result<()> {
        self.hnsw.insert(&c.id, &c.vector)?;
        self.tantivy.upsert(&c.id, &c.doc_id, &c.indexed_text)?;
        Ok(())
    }

    pub fn commit(&self) -> Result<()> {
        self.tantivy.commit()?;
        // HnswCache writes are in-memory; nothing to commit.
        Ok(())
    }
}

/// One pass over `KB_CHUNKS`, summarising what each index layer should hold.
#[derive(Debug, Default)]
pub struct ChunkScan {
    /// Decodable chunk rows (what tantivy indexes).
    pub total: u64,
    /// Fingerprint of the chunks the dense layer admits.
    pub dense: IndexFingerprint,
    /// Doc ids owning chunks the dense layer rejects (zero / wrong-dim
    /// vector, or embedded by a different model) — they need re-embedding.
    pub stale_docs: HashSet<String>,
}

fn scan_chunks(store: &KbStore, hnsw: &HnswCache) -> Result<ChunkScan> {
    use redb::ReadableTable;

    use crate::{
        model::KbChunk,
        store::{codec::decode, schema::KB_CHUNKS},
    };
    let rtx = store.begin_read()?;
    let tbl = rtx.open_table(KB_CHUNKS)?;
    let mut scan = ChunkScan::default();
    for entry in tbl.iter()? {
        let (k, v) = entry?;
        let c: KbChunk = match decode(v.value()) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(chunk = %k.value(), "kb index: skip undecodable chunk: {e:#}");
                continue;
            }
        };
        scan.total += 1;
        if hnsw.admits(&c) {
            scan.dense.add(&c.id);
        } else {
            scan.stale_docs.insert(c.doc_id.clone());
        }
    }
    Ok(scan)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;
    use crate::{
        canonicalize::{CanonicalizeInput, canonicalize_by_mime},
        embedder::{KbEmbedder, StubEmbedder},
        pipeline::{IngestInput, ingest_canonicalized},
        worker::{DefaultDispatcher, WorkerConfig, WorkerPool, handlers::HandlerCtx},
    };

    #[test]
    fn open_and_rebuild_recovers_both_layers() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb")).unwrap());
        let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
        paths.ensure_layout().unwrap();
        let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());

        // Ingest one doc, drain worker so chunks land in redb.
        let bytes = b"# Hi\n\nfirst body content here.";
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
        // Scope the worker's KbIndex so its tantivy lock is released
        // before we open a fresh one for the rebuild check (tantivy
        // takes an exclusive directory lock per process).
        {
            let pre_index = Arc::new(KbIndex::open(&paths).unwrap());
            let ctx = HandlerCtx {
                store: store.clone(),
                paths: paths.clone(),
                embedder,
                index: pre_index,
            };
            WorkerPool::run_one_blocking(
                &ctx,
                &WorkerConfig {
                    worker_id: "w".into(),
                    ..WorkerConfig::default()
                },
                &DefaultDispatcher,
            )
            .unwrap();
        }

        // Now rebuild a fresh KbIndex from redb.
        let idx = KbIndex::open_and_rebuild(&paths, &store).unwrap();
        assert!(idx.hnsw.len() > 0, "hnsw should have chunks after rebuild");
        let bm25 = idx.tantivy.search("body", 5).unwrap();
        assert!(
            !bm25.is_empty(),
            "tantivy should find at least one body match"
        );
    }

    #[test]
    fn stale_snapshot_is_not_restored() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb")).unwrap());
        let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
        paths.ensure_layout().unwrap();
        // Snapshot an EMPTY index, then ingest + index a doc. The old code
        // restored the empty snapshot forever (auto-recall disabled).
        {
            let idx = KbIndex::open(&paths).unwrap();
            idx.snapshot_hnsw(&paths).unwrap();
        }
        let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());
        let bytes = b"# Hi\n\nlate body content here.";
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
        {
            let pre_index = Arc::new(KbIndex::open(&paths).unwrap());
            let ctx = HandlerCtx {
                store: store.clone(),
                paths: paths.clone(),
                embedder,
                index: pre_index,
            };
            WorkerPool::run_one_blocking(
                &ctx,
                &WorkerConfig {
                    worker_id: "w".into(),
                    ..WorkerConfig::default()
                },
                &DefaultDispatcher,
            )
            .unwrap();
        }
        let idx = KbIndex::open_and_rebuild(&paths, &store).unwrap();
        assert!(idx.hnsw.len() > 0, "stale empty snapshot must not be restored");
    }
}
