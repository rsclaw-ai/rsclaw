//! Embedder trait + backends.
//!
//! - `StubEmbedder`: deterministic sha256 vectors (1024-dim), used by
//!   tests so idempotency is trivial to assert.
//! - `LocalKbEmbedder` (`local`): candle BGE adapter reusing the
//!   model loader already shipped for memory search
//!   (`crate::agent::memory::LocalBgeEmbedder`). Default in production
//!   when a model is present (bge-small-zh = 512-dim).
//!
//! Remote (OpenAI-compatible `/v1/embeddings` against the GPU fleet
//! running Qwen3-Embedding) is the next backend — same trait, just an
//! HTTP client.

pub mod local;
pub mod stub;

use anyhow::Result;

pub use local::LocalKbEmbedder;
pub use stub::StubEmbedder;

pub trait KbEmbedder: Send + Sync {
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn dimension(&self) -> usize;
    fn embedder_id(&self) -> &str;
}

/// Resolve the KB embedder for a `kb_root`. Precedence:
///   1. Remote OpenAI-compatible (`memorySearch.provider = "openai"`) — reuse
///      the shared embedding config so KB and memory hit the same GPU-fleet
///      `/v1/embeddings` (e.g. Qwen3-Embedding).
///   2. Local BGE model under `<base_dir>/models/{bge-small-zh,bge-base-zh,
///      bge-small-en}`.
///   3. StubEmbedder (deterministic) so a fresh install works out of the box.
///
/// Shared by the `rsclaw kb` CLI and the gateway `KnowledgeService`.
pub fn resolve_embedder(kb_root: &std::path::Path) -> std::sync::Arc<dyn KbEmbedder> {
    use std::sync::Arc;
    if let Some(cfg) = crate::config::load()
        .ok()
        .and_then(|c| c.raw.memory_search.clone())
    {
        if cfg.provider.as_deref() == Some("openai") {
            let model = cfg
                .model
                .clone()
                .unwrap_or_else(|| crate::embed::OPENAI_DEFAULT_MODEL.to_owned());
            let api_key = cfg
                .api_key
                .as_ref()
                .and_then(|s| s.resolve_early())
                .or_else(|| std::env::var("OPENAI_API_KEY").ok());
            let dim = cfg
                .dimensions
                .unwrap_or_else(|| crate::embed::openai_model_dim(&model)) as usize;
            let base_url = cfg
                .base_url
                .clone()
                .unwrap_or_else(|| crate::embed::OPENAI_DEFAULT_BASE_URL.to_owned());
            tracing::info!(model = %model, dim, base_url = %base_url, "kb: using remote OpenAI-compatible embedder");
            return Arc::new(LocalKbEmbedder::remote_openai(base_url, model, api_key, dim));
        }
    }

    let base_dir = kb_root.parent().unwrap_or(kb_root);
    let models_dir = base_dir.join("models");
    for name in ["bge-small-zh", "bge-base-zh", "bge-small-en"] {
        let dir = models_dir.join(name);
        if dir.join("model.safetensors").exists() {
            match LocalKbEmbedder::load(&dir) {
                Ok(e) => {
                    let dim = KbEmbedder::dimension(&e);
                    tracing::info!(model = name, dim, "kb: using local BGE embedder");
                    return Arc::new(e);
                }
                Err(e) => {
                    tracing::warn!(model = name, "kb: BGE load failed, trying next: {e:#}");
                }
            }
        }
    }
    tracing::info!("kb: no local BGE model found, using StubEmbedder (1024-dim)");
    Arc::new(StubEmbedder::default())
}
