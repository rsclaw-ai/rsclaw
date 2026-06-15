//! Cross-encoder rerank client for the KB search pipeline.
//!
//! Speaks the Jina/Cohere-compatible `/v1/rerank` shape that llama.cpp
//! serves under `--reranking` (e.g. bge-reranker-v2-m3 GGUF):
//!
//! ```json
//! POST {base_url}/rerank
//! {"model": "...", "query": "...", "documents": ["...", ...]}
//! → {"results": [{"index": 0, "relevance_score": 1.23}, ...]}
//! ```
//!
//! Sync-callable from any context: the search pipeline runs under
//! `spawn_blocking` on the agent-tool path but directly on the async
//! handler thread on the HTTP path, so this mirrors `OpenAiEmbedder`'s
//! transport pattern exactly — an async reqwest client driven via
//! `block_in_place` when a runtime is present, or a temp runtime when
//! not. A `reqwest::blocking` client is NOT safe here: its inner
//! runtime panics with "Cannot drop a runtime in a context where
//! blocking is not allowed" the moment it's touched from an async
//! thread (observed live, took the whole rerank stage down with it).

use anyhow::{Context, Result};

/// Default fused-candidate window sent to the reranker.
pub const DEFAULT_RERANK_TOP_N: usize = 20;
/// Request deadline — reranking 20 chunks on a GPU is sub-second; a
/// long-chunk window on an old card can take a few seconds. Past this
/// the endpoint is wedged and the fused order is the better answer.
const RERANK_TIMEOUT_SECS: u64 = 30;

pub struct KbReranker {
    client: reqwest::Client,
    url: String,
    model: Option<String>,
    pub top_n: usize,
}

impl KbReranker {
    /// Build from the effective `kb.rerank` config block. Returns `None`
    /// when the block is absent or explicitly disabled.
    pub fn from_config() -> Option<std::sync::Arc<Self>> {
        let cfg = rsclaw_config::load().ok()?;
        let rr = cfg.raw.kb.as_ref()?.rerank.clone()?;
        if !rr.enabled.unwrap_or(true) {
            return None;
        }
        // base_url empty + rsclaw-* model → fleet API; mirrors the
        // embedder's convention-over-config base resolution.
        let model_is_rsclaw = rr
            .model
            .as_deref()
            .map(rsclaw_embed::is_rsclaw_model)
            .unwrap_or(false);
        let base_raw = rr.base_url.trim();
        let base = if base_raw.is_empty() {
            if model_is_rsclaw {
                rsclaw_embed::RSCLAW_API_BASE_URL.to_owned()
            } else {
                return None;
            }
        } else {
            base_raw.trim_end_matches('/').to_owned()
        };
        // Disable idle-pool reuse for the same reason as the embedder: a
        // slow rerank endpoint's keep-alive connection gets reaped between
        // calls and reqwest would otherwise hand back the dead socket.
        let client = rsclaw_embed::build_remote_client(RERANK_TIMEOUT_SECS);
        Some(std::sync::Arc::new(Self {
            client,
            url: format!("{base}/rerank"),
            model: rr.model,
            top_n: rr.top_n.unwrap_or(DEFAULT_RERANK_TOP_N).clamp(2, 100),
        }))
    }

    /// Score `docs` against `query`. Returns one relevance score per input
    /// index (input order preserved); higher is more relevant. Errors
    /// bubble up so the caller can fall back to the fused order.
    pub fn rerank(&self, query: &str, docs: &[&str]) -> Result<Vec<f32>> {
        let mut body = serde_json::json!({
            "query": query,
            "documents": docs,
            // Cohere-style hints: cap the returned set and skip echoing the
            // documents back (we only consume scores by index). Harmless to
            // endpoints that ignore them.
            "top_n": self.top_n.min(docs.len()).max(1),
            "return_documents": false,
        });
        if let Some(m) = &self.model {
            body["model"] = serde_json::json!(m);
        }

        let send = || async {
            self.client
                .post(self.url.as_str())
                .json(&body)
                .send()
                .await?
                .error_for_status()?
                .json::<serde_json::Value>()
                .await
        };
        let resp: serde_json::Value = match tokio::runtime::Handle::try_current() {
            Ok(handle) => tokio::task::block_in_place(|| handle.block_on(send()))
                .context("rerank request failed")?,
            Err(_) => {
                let tmp_rt = tokio::runtime::Runtime::new()
                    .context("failed to create temp runtime for rerank")?;
                tmp_rt.block_on(send()).context("rerank request failed")?
            }
        };

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
