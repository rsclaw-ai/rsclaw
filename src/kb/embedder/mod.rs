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
