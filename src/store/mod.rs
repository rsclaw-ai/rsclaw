//! Storage layer: redb (hot KV) + tantivy (BM25 FTS) + hnsw_rs (vector).
//!
//! Hot path  — redb: session meta, messages, pairing, KV.
//! FTS path  — tantivy: BM25 full-text search over document corpus.
//! Cold path — hnsw_rs + BGE-Small: semantic vector memory (see agent::memory).
//!
//! Architecture: AGENTS.md §8 "Storage Architecture" + §31 "Memory System"

pub mod redb_store;
pub mod search;

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
pub use redb_store::RedbStore;
pub use search::SearchIndex;
use tracing::info;

use crate::MemoryTier;

/// Unified storage facade — combines hot KV (redb) and BM25 FTS (tantivy).
pub struct Store {
    pub db: Arc<RedbStore>,
    pub search: SearchIndex,
}

impl Store {
    /// Open (or create) both stores under `data_dir`.
    ///
    /// Directories created if they don't exist:
    ///   `data_dir/redb/`   — redb database files
    ///   `data_dir/search/` — tantivy index files
    pub fn open(data_dir: &Path, tier: MemoryTier) -> Result<Self> {
        let redb_dir = data_dir.join("redb");
        let search_dir = data_dir.join("search");

        std::fs::create_dir_all(&redb_dir)?;
        std::fs::create_dir_all(&search_dir)?;

        let db = Arc::new(RedbStore::open(&redb_dir.join("data.redb"), tier)?);
        let search = SearchIndex::open(&search_dir, tier)?;

        info!(
            db_size = ?redb_dir, search_size = ?search_dir, tier = ?tier,
            "store opened"
        );

        Ok(Self { db, search })
    }
}
