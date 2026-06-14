//! KbIndex — composite of dense (hnsw) + sparse (tantivy) caches.
//! Both layers are caches over redb; rebuild from redb is the
//! canonical recovery path.

pub mod cjk;
pub mod hnsw;
pub mod rebuild;
pub mod tantivy;

use anyhow::Result;
pub use cjk::JiebaTokenizer;
pub use hnsw::HnswCache;
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
        let tantivy = TantivyIndex::open_or_create(&paths.root.join("idx/tantivy"))?;
        Ok(Self {
            hnsw: HnswCache::new(dim),
            tantivy,
        })
    }

    /// Open + populate the dense layer at `DEFAULT_DIMENSION`.
    pub fn open_and_rebuild(paths: &KbPaths, store: &KbStore) -> Result<Self> {
        Self::open_and_rebuild_with_dim(paths, store, hnsw::DEFAULT_DIMENSION)
    }

    /// Open + populate the dense layer at `dim`. Tries the on-disk
    /// HNSW snapshot first (cheap, no embedder work); falls back to a
    /// full rebuild from redb. A snapshot whose dimension doesn't
    /// match `dim` (e.g. embedder changed) fails restore and triggers
    /// a clean rebuild. Tantivy is always rebuilt from redb.
    pub fn open_and_rebuild_with_dim(paths: &KbPaths, store: &KbStore, dim: usize) -> Result<Self> {
        let idx = Self::open_with_dim(paths, dim)?;
        let snapshot_dir = paths.root.join("hnsw");
        let restored = idx.hnsw.restore(&snapshot_dir).unwrap_or(false);
        if !restored {
            idx.hnsw.rebuild(store)?;
        }
        idx.tantivy.rebuild(store)?;
        Ok(idx)
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
}
