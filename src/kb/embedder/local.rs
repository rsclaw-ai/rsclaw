//! Local BGE embedder for the KB — a thin adapter over the candle
//! BGE loader already shipped for memory search
//! (`crate::agent::memory::LocalBgeEmbedder`). Reuses the same model
//! files under `<base_dir>/models/bge-small-{zh,en}/` so the KB and
//! memory don't each ship their own copy.
//!
//! bge-small-zh-v1.5 → hidden_size 512; bge-small-en-v1.5 → 384. The
//! KB's HNSW dimension is taken from `dimension()` at index-open time,
//! so swapping models just works as long as the snapshot is rebuilt.

use super::KbEmbedder;
use crate::embed::{Embedder as MemEmbedder, LocalBgeEmbedder};
use anyhow::{Context, Result};
use std::path::Path;

pub struct LocalKbEmbedder {
    inner: LocalBgeEmbedder,
    dim: usize,
    id: String,
}

impl LocalKbEmbedder {
    /// Load a BGE model directory (`config.json` + `model.safetensors`
    /// + `tokenizer.json`). The `id` is derived from the dir name so
    /// `KbChunk.embedder_id` records which model produced each vector.
    pub fn load(model_dir: &Path) -> Result<Self> {
        let inner = LocalBgeEmbedder::load(model_dir)
            .with_context(|| format!("load BGE model from {}", model_dir.display()))?;
        let dim = inner.dimension() as usize;
        let name = model_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("bge");
        Ok(Self {
            inner,
            dim,
            id: format!("local-{name}-{dim}"),
        })
    }
}

impl KbEmbedder for LocalKbEmbedder {
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        // LocalBgeEmbedder embeds one text at a time (candle BERT
        // forward per call). Batching is a Week 6 optimisation; for
        // now map sequentially. Output is already L2-normalised by
        // the inner embedder so cosine == dot.
        Ok(texts.iter().map(|t| self.inner.embed(t)).collect())
    }

    fn dimension(&self) -> usize {
        self.dim
    }

    fn embedder_id(&self) -> &str {
        &self.id
    }
}
