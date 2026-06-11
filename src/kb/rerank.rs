//! Cross-encoder rerank client for the KB search pipeline.
//!
//! Speaks the Jina/Cohere-compatible `/v1/rerank` shape that llama.cpp
//! serves under `--reranking` (e.g. Qwen3-Reranker / bge-reranker GGUFs):
//!
//! ```json
//! POST {base_url}/rerank
//! {"model": "...", "query": "...", "documents": ["...", ...]}
//! → {"results": [{"index": 0, "relevance_score": 1.23}, ...]}
//! ```
//!
//! The pipeline calls this synchronously from inside `spawn_blocking`
//! (same execution model as the embedder), so a blocking HTTP client is
//! correct here. The client is lazily constructed on first use because
//! `reqwest::blocking::Client::new()` panics when called from an async
//! runtime thread — service construction happens in async context, the
//! first search does not.

use std::sync::OnceLock;

use anyhow::{Context, Result};

/// Default fused-candidate window sent to the reranker.
pub const DEFAULT_RERANK_TOP_N: usize = 20;
/// Hard ceiling on the request timeout — reranking 20 short chunks on a
/// GPU takes well under a second; anything past this means the endpoint
/// is wedged and the fused order is the better answer.
const RERANK_TIMEOUT_SECS: u64 = 10;

pub struct KbReranker {
    client: OnceLock<reqwest::blocking::Client>,
    url: String,
    model: Option<String>,
    pub top_n: usize,
}

impl KbReranker {
    /// Build from the effective `kb.rerank` config block. Returns `None`
    /// when the block is absent or explicitly disabled.
    pub fn from_config() -> Option<std::sync::Arc<Self>> {
        let cfg = crate::config::load().ok()?;
        let rr = cfg.raw.kb.as_ref()?.rerank.clone()?;
        if !rr.enabled.unwrap_or(true) {
            return None;
        }
        let base = rr.base_url.trim_end_matches('/');
        if base.is_empty() {
            return None;
        }
        Some(std::sync::Arc::new(Self {
            client: OnceLock::new(),
            url: format!("{base}/rerank"),
            model: rr.model,
            top_n: rr.top_n.unwrap_or(DEFAULT_RERANK_TOP_N).clamp(2, 100),
        }))
    }

    /// Score `docs` against `query`. Returns one relevance score per input
    /// index (input order preserved); higher is more relevant. Errors
    /// bubble up so the caller can fall back to the fused order.
    pub fn rerank(&self, query: &str, docs: &[&str]) -> Result<Vec<f32>> {
        let client = self.client.get_or_init(|| {
            reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(RERANK_TIMEOUT_SECS))
                .build()
                .expect("failed to build rerank HTTP client")
        });
        let mut body = serde_json::json!({
            "query": query,
            "documents": docs,
        });
        if let Some(m) = &self.model {
            body["model"] = serde_json::json!(m);
        }
        let resp: serde_json::Value = client
            .post(&self.url)
            .json(&body)
            .send()
            .context("rerank request failed")?
            .error_for_status()
            .context("rerank endpoint returned error status")?
            .json()
            .context("rerank response is not JSON")?;
        let results = resp
            .get("results")
            .and_then(|v| v.as_array())
            .context("rerank response missing results array")?;
        let mut scores = vec![f32::NEG_INFINITY; docs.len()];
        for r in results {
            let idx = r.get("index").and_then(|v| v.as_u64()).unwrap_or(u64::MAX) as usize;
            let score = r
                .get("relevance_score")
                .or_else(|| r.get("score"))
                .and_then(|v| v.as_f64())
                .unwrap_or(f64::NEG_INFINITY) as f32;
            if idx < scores.len() {
                scores[idx] = score;
            }
        }
        if scores.iter().all(|s| !s.is_finite()) {
            anyhow::bail!("rerank response carried no usable scores");
        }
        Ok(scores)
    }
}

impl std::fmt::Debug for KbReranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KbReranker")
            .field("url", &self.url)
            .field("model", &self.model)
            .field("top_n", &self.top_n)
            .finish()
    }
}
