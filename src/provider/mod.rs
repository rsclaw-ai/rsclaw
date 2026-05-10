//! LLM provider abstraction layer.
//!
//! All providers implement the `LlmProvider` trait and are registered in
//! `ProviderRegistry`. The failover manager sits on top and handles
//! 429/auth retries with exponential back-off.

pub mod anthropic;
pub mod defaults;
pub mod failover;
pub mod gemini;
pub mod model_defaults;
pub mod openai;
pub mod registry;
pub mod rsclaw;

use std::pin::Pin;

use anyhow::Result;

/// Default User-Agent for all LLM provider HTTP requests.
pub(crate) const DEFAULT_USER_AGENT: &str = concat!("rsclaw/", env!("CARGO_PKG_VERSION"));

/// Build a `reqwest::Client` with the shared User-Agent header.
pub(crate) fn http_client() -> reqwest::Client {
    http_client_with_ua(None)
}

/// Send a `reqwest::RequestBuilder` once, and retry exactly one time on
/// transport-level errors (connection refused, reset, or "closed before
/// message completed"). Uses `try_clone` to rebuild the request for the
/// retry — falls back to a single attempt if the body is non-cloneable
/// (e.g. multipart with a streaming file part).
///
/// Why: local OpenAI-compatible servers (llama-server, vLLM, ollama) cycle
/// HTTP keepalive on shorter timers than reqwest's connection pool, so the
/// first request after an idle gap intermittently lands on a half-closed
/// pooled connection. One retry papers over that race without masking
/// genuine 4xx/5xx responses, which arrive as a successful `send().await`
/// and are inspected by the caller.
pub(crate) async fn send_with_transport_retry(
    builder: reqwest::RequestBuilder,
) -> reqwest::Result<reqwest::Response> {
    let retryable = |e: &reqwest::Error| -> bool {
        use std::error::Error;
        if e.is_connect() {
            return true;
        }
        // Walk the source chain — the "closed before message completed"
        // and "Connection reset" / "Connection refused" texts live in the
        // hyper / std::io::Error layer underneath.
        let mut src: Option<&dyn Error> = e.source();
        while let Some(s) = src {
            let msg = s.to_string();
            if msg.contains("closed before message completed")
                || msg.contains("Connection reset")
                || msg.contains("Connection refused")
                || msg.contains("connection closed")
            {
                return true;
            }
            src = s.source();
        }
        false
    };

    let Some(retry_builder) = builder.try_clone() else {
        // Non-cloneable body (e.g. multipart with file stream). One shot.
        return builder.send().await;
    };

    match builder.send().await {
        Ok(resp) => Ok(resp),
        Err(e) if retryable(&e) => {
            tracing::debug!(error = %e, "http: retrying once after transport error");
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            retry_builder.send().await
        }
        Err(e) => Err(e),
    }
}

/// Build a `reqwest::Client` with a custom or default User-Agent.
///
/// Tuning notes:
/// - `pool_idle_timeout` set to 10s (down from 60s). llama-server / vLLM /
///   most local OpenAI-compatible servers default their HTTP keep-alive to
///   ~5-15s. With a 60s pool-idle window the client kept reusing pooled
///   connections that the server had already half-closed, surfacing as
///   "connection closed before message completed" on the next request.
/// - `tcp_keepalive(30s)` keeps long-prefill streaming connections alive
///   through any intermediate timeouts during a 20+ second prefill on
///   large prompts.
pub(crate) fn http_client_with_ua(user_agent: Option<&str>) -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(user_agent.unwrap_or(DEFAULT_USER_AGENT))
        .connect_timeout(std::time::Duration::from_secs(20))
        .pool_idle_timeout(std::time::Duration::from_secs(10))
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .build()
        .expect("failed to build HTTP client")
}
use futures::{Stream, future::BoxFuture};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

/// A single message in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: MessageContent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    Image {
        url: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: Option<bool>,
    },
    Reasoning {
        text: String,
    },
}

/// A tool definition passed to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Full request to an LLM provider.
#[derive(Debug, Clone, Default)]
pub struct LlmRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub system: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub frequency_penalty: Option<f32>,
    /// If > 0, the provider should enable extended thinking with this budget.
    pub thinking_budget: Option<u32>,
    /// KV cache mode: 0=off, 1=append-only (default), 2=incremental (cache_id + delta).
    pub kv_cache_mode: u8,
    /// Session key for cache_id tracking (used when kv_cache_mode=2).
    pub session_key: Option<String>,
    /// (kvCacheMode=2 only) Pre-split shared system prefix —
    /// byte-identical across all RsClaw clients of this version. The
    /// rsclaw provider sends this as `dynamic_prefix.system` so the
    /// upstream LRU dedupes the cacheable bytes across clients. When
    /// `None`, the provider falls back to deriving everything from
    /// `system` (loses cross-client cache reuse but stays correct).
    /// Other providers ignore this field.
    pub system_shared: Option<String>,
    /// (kvCacheMode=2 only) Per-client system suffix — workspace,
    /// language, skills, platform info. Sent as
    /// `dynamic_prefix.user_suffix` and is the slot's per-session text.
    /// Other providers ignore this field.
    pub system_user: Option<String>,
}

/// Serialize an `f32` to a JSON number with 2 decimal places.
///
/// Avoids IEEE 754 precision artefacts when `f32` 0.6 becomes
/// `f64` 0.6000000238418579 during JSON round-trips.
pub fn json_f32(v: f32) -> serde_json::Value {
    let rounded = (f64::from(v) * 100.0).round() / 100.0;
    serde_json::json!(rounded)
}

/// A single streaming delta event from the LLM.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Assistant text delta
    TextDelta(String),
    /// Reasoning/thinking delta (collected separately, used as fallback if content is empty)
    ReasoningDelta(String),
    /// Tool call requested by the model
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Stream complete; includes token usage
    Done { usage: Option<TokenUsage> },
    /// Unrecoverable stream error
    Error(String),
}

#[derive(Debug, Clone)]
pub struct TokenUsage {
    pub input: u32,
    pub output: u32,
}

/// Boxed streaming response.
pub type LlmStream = Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>;

// ---------------------------------------------------------------------------
// Provider trait
// ---------------------------------------------------------------------------

// BoxFuture is required here because this trait is used as `dyn LlmProvider`
// (see ProviderRegistry, gateway/providers.rs). Native async fn in traits
// does not support dynamic dispatch.
pub trait LlmProvider: Send + Sync {
    /// Provider name, e.g. "anthropic", "openai".
    fn name(&self) -> &str;

    /// Stream a completion. The returned stream emits `StreamEvent`s until
    /// `StreamEvent::Done` or `StreamEvent::Error`.
    fn stream(&self, req: LlmRequest) -> BoxFuture<'_, Result<LlmStream>>;
}

// ---------------------------------------------------------------------------
// RetryConfig + exponential back-off  (agents.md §22)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct RetryConfig {
    pub attempts: u32,     // default 3
    pub min_delay_ms: u64, // default 400
    pub max_delay_ms: u64, // default 30_000
    pub jitter: f64,       // default 0.1
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            attempts: 3,
            min_delay_ms: 400,
            max_delay_ms: 30_000,
            jitter: 0.1,
        }
    }
}

/// Compute the back-off delay for a given attempt index (0-based).
/// Jitter is deterministic so tests can assert ordering.
pub fn backoff_delay(attempt: u32, config: &RetryConfig) -> std::time::Duration {
    let base = config.min_delay_ms as f64 * 2f64.powi(attempt as i32);
    let clamped = base.min(config.max_delay_ms as f64);
    let jitter = clamped * config.jitter * (attempt as f64 * 0.31 % 1.0);
    std::time::Duration::from_millis((clamped + jitter) as u64)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_increases_with_attempt() {
        let cfg = RetryConfig::default();
        let d0 = backoff_delay(0, &cfg);
        let d1 = backoff_delay(1, &cfg);
        let d2 = backoff_delay(2, &cfg);
        assert!(
            d0 < d1,
            "attempt 0 ({d0:?}) should be less than attempt 1 ({d1:?})"
        );
        assert!(
            d1 < d2,
            "attempt 1 ({d1:?}) should be less than attempt 2 ({d2:?})"
        );
    }

    #[test]
    fn backoff_clamped_at_max() {
        let cfg = RetryConfig::default();
        // attempt 20 would compute 400 * 2^20 = 419 430 400 ms, far above 30_000
        let d = backoff_delay(20, &cfg);
        // with 10 % jitter the upper bound is max_delay_ms * 1.1
        let max_with_jitter = (cfg.max_delay_ms as f64 * (1.0 + cfg.jitter)) as u64;
        assert!(
            d.as_millis() as u64 <= max_with_jitter,
            "delay {d:?} exceeds max+jitter bound ({max_with_jitter} ms)"
        );
    }
}
