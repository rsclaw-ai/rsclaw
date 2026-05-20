//! Adapter from the shared `crate::embed` backends to the KB's
//! `KbEmbedder` trait. One adapter wraps any `EmbedderBackend`
//! (Local BGE / OpenAi-compatible REST / Ollama), so the KB gets all
//! of them — including the remote-API path (point an OpenAi backend
//! at a GPU fleet serving Qwen3-Embedding).
//!
//! The shared `Embedder` trait is single-text (`embed(&str)`); KB's
//! `KbEmbedder` is batch (`embed_batch(&[String])`). This adapter
//! maps the batch over the single-text backend. True server-side
//! batching is a follow-up (the OpenAi `/v1/embeddings` endpoint
//! accepts an array, so a batched remote path is a clean upgrade).

use super::KbEmbedder;
use crate::embed::{Embedder, EmbedderBackend, LocalBgeEmbedder, OpenAiEmbedder};
use anyhow::{Context, Result};
use std::path::Path;

pub struct LocalKbEmbedder {
    backend: EmbedderBackend,
    dim: usize,
    id: String,
}

impl LocalKbEmbedder {
    /// Load a local BGE model directory (`config.json` +
    /// `model.safetensors` + `tokenizer.json`). `id` is derived from
    /// the dir name so `KbChunk.embedder_id` records the model.
    pub fn load(model_dir: &Path) -> Result<Self> {
        let inner = LocalBgeEmbedder::load(model_dir)
            .with_context(|| format!("load BGE model from {}", model_dir.display()))?;
        let dim = Embedder::dimension(&inner) as usize;
        let name = model_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("bge");
        Ok(Self {
            backend: EmbedderBackend::Local(inner),
            dim,
            id: format!("local-{name}-{dim}"),
        })
    }

    /// Remote OpenAI-compatible embedder. Point `base_url` at any
    /// `/v1/embeddings` server — e.g. a GPU fleet running
    /// Qwen3-Embedding via vLLM / SGLang / TEI / infinity. `dim` is
    /// the model's output dimension (must match the KB's HNSW dim).
    pub fn remote_openai(
        base_url: String,
        model: String,
        api_key: Option<String>,
        dim: usize,
    ) -> Self {
        let inner = OpenAiEmbedder::new(
            api_key.unwrap_or_default(),
            Some(model.clone()),
            Some(base_url),
            Some(dim as i32),
        );
        Self {
            backend: EmbedderBackend::OpenAi(inner),
            dim,
            id: format!("remote-{model}-{dim}"),
        }
    }
}

impl KbEmbedder for LocalKbEmbedder {
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.backend.embed(t)).collect())
    }

    fn dimension(&self) -> usize {
        self.dim
    }

    fn embedder_id(&self) -> &str {
        &self.id
    }
}
