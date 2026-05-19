//! Embedder trait + deterministic stub used in Week 2.
//!
//! The real BGE-M3 embedder (candle-based, ~2GB weights) is a
//! self-contained follow-up that swaps in behind `KbEmbedder` once
//! the pipeline + worker pool are proven correct. Until then,
//! `StubEmbedder` returns sha256-derived deterministic vectors so
//! handler idempotency tests are easy to write.

pub mod stub;

use anyhow::Result;

pub use stub::StubEmbedder;

pub trait KbEmbedder: Send + Sync {
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn dimension(&self) -> usize;
    fn embedder_id(&self) -> &str;
}
