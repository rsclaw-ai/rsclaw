//! Embedder trait + backends.
//!
//! - `StubEmbedder`: deterministic sha256 vectors (1024-dim), used by tests so
//!   idempotency is trivial to assert.
//! - `LocalKbEmbedder` (`local`): candle BGE adapter reusing the model loader
//!   already shipped for memory search
//!   (`crate::agent::memory::LocalBgeEmbedder`). Default in production when a
//!   model is present (bge-small-zh = 512-dim).
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
///   1. Effective embed config — `kb.embed` if set, else the shared
///      `memorySearch`. When its `provider = "openai"`, use a remote
///      OpenAI-compatible embedder (e.g. the GPU-fleet Qwen3-Embedding). A
///      `kb.embed` override lets KB use a different embedder than memory.
///   2. Local BGE model — the `embed.local.modelRepo` dir if set, then the
///      defaults under
///      `<base_dir>/models/{bge-small-zh,bge-base-zh,bge-small-en}`.
///   3. StubEmbedder (deterministic) so a fresh install works out of the box.
///
/// `kb.embed` reuses `memorySearch`'s exact shape (local or remote); a
/// `provider` KB has no adapter for (e.g. "ollama") logs a warning and falls
/// through to the local-model scan rather than failing.
///
/// Shared by the `rsclaw kb` CLI and the gateway `KnowledgeService`.
pub fn resolve_embedder(kb_root: &std::path::Path) -> std::sync::Arc<dyn KbEmbedder> {
    use std::sync::Arc;
    let embed_cfg = effective_embed_config();

    if let Some(cfg) = embed_cfg.as_ref() {
        // A `rsclaw-*` model implies the remote OpenAI-compatible path even
        // when `provider` is unset — convention over config.
        let model_is_rsclaw = cfg
            .model
            .as_deref()
            .map(crate::embed::is_rsclaw_model)
            .unwrap_or(false);
        // `rsclaw` is a semantic alias for the openai-compatible remote
        // path (the fleet speaks the OpenAI embeddings shape); writing
        // `provider: "openai"` for a first-party model reads wrong. An
        // unset provider with a `rsclaw-*` model also takes the remote
        // path — both resolve the base URL via `resolve_embed_base_url`.
        let effective_provider = match cfg.provider.as_deref() {
            Some("rsclaw") => Some("openai"),
            None if model_is_rsclaw => Some("openai"),
            other => other,
        };
        match effective_provider {
            Some("openai") => {
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
                    .unwrap_or_else(|| crate::embed::openai_model_dim(&model))
                    as usize;
                // base_url empty + rsclaw-* model → fleet API; else OpenAI.
                let base_url =
                    crate::embed::resolve_embed_base_url(cfg.base_url.as_deref(), &model);
                tracing::info!(model = %model, dim, base_url = %base_url, "kb: using remote OpenAI-compatible embedder");
                return Arc::new(LocalKbEmbedder::remote_openai(
                    base_url, model, api_key, dim,
                ));
            }
            // "local"/unset → fall through to the local-model scan below.
            None | Some("local") => {}
            Some(other) => {
                tracing::warn!(
                    provider = other,
                    "kb: no embedder adapter for this provider; falling back to local BGE scan"
                );
            }
        }
    }

    // Local BGE: honor an explicit `embed.local.modelRepo` dir first, then the
    // built-in defaults. `modelRepo` is e.g. "BAAI/bge-small-zh-v1.5"; the dir
    // name is its last path segment.
    let base_dir = kb_root.parent().unwrap_or(kb_root);
    let models_dir = base_dir.join("models");
    let preferred = embed_cfg
        .as_ref()
        .and_then(|c| c.local.as_ref())
        .and_then(|l| l.model_repo.as_deref())
        .and_then(|repo| repo.rsplit('/').next())
        .map(str::to_owned);
    let defaults = ["bge-small-zh", "bge-base-zh", "bge-small-en"];
    let candidates = preferred.iter().map(String::as_str).chain(defaults);
    for name in candidates {
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

/// The embed config KB should use: `kb.embed` override if present, else the
/// shared `memorySearch`. Centralized so embedder selection and the asymmetric
/// `queryInstruction` (in `KnowledgeService`) read the same source.
pub fn effective_embed_config() -> Option<crate::config::schema::EmbedConfig> {
    let cfg = crate::config::load().ok()?;
    cfg.raw
        .kb
        .as_ref()
        .and_then(|k| k.embed.clone())
        .or_else(|| cfg.raw.memory_search.clone())
}
