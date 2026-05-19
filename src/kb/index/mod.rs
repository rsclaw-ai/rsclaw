//! KbIndex — composite of dense (hnsw) + sparse (tantivy) caches.
//! Both layers are caches over redb; rebuild from redb is the
//! canonical recovery path.

pub mod hnsw;
pub mod rebuild;
pub mod tantivy;

pub use hnsw::HnswCache;
pub use tantivy::TantivyIndex;

use crate::kb::paths::KbPaths;
use crate::kb::store::KbStore;
use anyhow::Result;

pub struct KbIndex {
    pub hnsw: HnswCache,
    pub tantivy: TantivyIndex,
}

impl KbIndex {
    /// Open both layers. Tantivy lives on disk under `<paths.root>/idx/tantivy/`;
    /// hnsw starts empty and must be populated via `rebuild::from_redb`
    /// or per-chunk `upsert_chunk`.
    pub fn open(paths: &KbPaths) -> Result<Self> {
        let tantivy = TantivyIndex::open_or_create(&paths.root.join("idx/tantivy"))?;
        Ok(Self {
            hnsw: HnswCache::empty(),
            tantivy,
        })
    }

    /// Open + rebuild from redb. Use at startup.
    pub fn open_and_rebuild(paths: &KbPaths, store: &KbStore) -> Result<Self> {
        let idx = Self::open(paths)?;
        rebuild::from_redb(&idx, store)?;
        Ok(idx)
    }

    /// Upsert a chunk into both indexes. Caller wraps multiple upserts
    /// in `commit()` to batch tantivy IO.
    pub fn upsert_chunk(&self, c: &crate::kb::model::KbChunk) -> Result<()> {
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
    use super::*;
    use crate::kb::canonicalize::{canonicalize_by_mime, CanonicalizeInput};
    use crate::kb::embedder::{KbEmbedder, StubEmbedder};
    use crate::kb::pipeline::{ingest_canonicalized, IngestInput};
    use crate::kb::worker::{handlers::HandlerCtx, DefaultDispatcher, WorkerConfig, WorkerPool};
    use std::sync::Arc;
    use tempfile::TempDir;

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

        // Now rebuild a fresh KbIndex from redb.
        let idx = KbIndex::open_and_rebuild(&paths, &store).unwrap();
        assert!(idx.hnsw.len() > 0, "hnsw should have chunks after rebuild");
        let bm25 = idx.tantivy.search("body", 5).unwrap();
        assert!(!bm25.is_empty(), "tantivy should find at least one body match");
    }
}
