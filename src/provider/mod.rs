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

/// Warn (at most once per `(provider, session_key)` pair across the
/// process lifetime) when a non-`rsclaw` provider receives a request
/// with `kv_cache_mode=2`. Mode 2 is the rsclaw-server stateful
/// session protocol — every other provider treats the field as a no-op
/// and silently degrades to mode 0, so an operator who configured
/// `kvCacheMode: 2` against an OpenAI/Anthropic/Gemini-routed model
/// would lose every benefit of the setting without seeing an error.
/// Without this warning the misconfiguration is invisible.
///
/// Dedup is per-session so a long-running session doesn't re-warn on
/// every iteration; the `provider` segment of the key keeps openai vs
/// anthropic vs gemini distinct.
pub(crate) fn warn_unsupported_kv_cache_mode_2(provider: &str, req: &LlmRequest) {
    if req.kv_cache_mode < 2 {
        return;
    }
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let session = req.session_key.as_deref().unwrap_or("<no-session>");
    let key = format!("{provider}:{session}");
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = match seen.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if !guard.insert(key) {
        return;
    }
    drop(guard);
    tracing::warn!(
        provider,
        session = session,
        "kv_cache_mode=2 requested but {} provider does not support it; \
         degrading to mode 0 — route mode 2 traffic through the rsclaw \
         provider (RSCLAW_KEY/RSCLAW_URL) for incremental session caching",
        provider,
    );
}

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

/// Which logical endpoint family a request belongs to.
///
/// The rsclaw-server fleet exposes three sibling endpoint families
/// under `/v1/agent/*` — `sessions` for the primary agent loop,
/// `fastshot` for cheap auxiliary calls (compression, query rewrite,
/// personal-info extraction), and `vision` for VL grounding. Each is
/// filtered to a different worker pool server-side
/// (`sessions_enabled` / `fastshot_enabled` / `vision_enabled`), so
/// auxiliary traffic never competes with the primary loop's KV-cache
/// slots.
///
/// The client encodes this routing decision **explicitly** on the
/// request rather than parsing it out of the model name. The agent
/// runtime knows what kind of call it is making (primary turn vs
/// flash compression vs vision describe); this enum captures that
/// intent and the rsclaw provider maps it to the right URL prefix.
///
/// Non-rsclaw providers (OpenAI, Anthropic, Gemini, …) ignore this
/// field — they have no equivalent server-side concept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AgentEndpoint {
    /// `/v1/agent/sessions/*` — main agent conversation traffic.
    #[default]
    Primary,
    /// `/v1/agent/fastshot/*` — fastshot worker pool. Auto-selected
    /// when caller sets this variant OR `model` matches the prefix
    /// `rsclaw/rsclaw-flash-*` (e.g. `rsclaw-flash-v1`).
    Flash,
    /// `/v1/agent/vision/*` — VL grounding (image description,
    /// computer_use screenshot reasoning). Auto-selected when
    /// caller sets this variant OR `model` matches the prefix
    /// `rsclaw/rsclaw-vision-*`.
    Vision,
}

// Routing rule on the rsclaw provider (single source of truth).
// Server enforces per-route model whitelists with 400 model_slot_mismatch
// on violations, so canonical model names take priority over the endpoint
// variant. The endpoint variant is only consulted for non-canonical models.
//
//   1. model rsclaw/rsclaw-flash-*                          → /v1/agent/fastshot
//   2. model rsclaw/rsclaw-vision-*                         → /v1/agent/vision
//   3. model rsclaw/rsclaw-agent-* + session_key=None       → /v1/agent/oneshot   (stateless agent call)
//   4. model rsclaw/rsclaw-agent-* + session_key=Some       → /v1/agent/sessions  (kvCacheMode=2)
//   5. non-canonical model + endpoint=Flash                  → /v1/agent/fastshot  (server may 400)
//   6. non-canonical model + endpoint=Vision                 → /v1/agent/vision    (server may 400)
//   7. endpoint=Primary + session_key=Some                   → /v1/agent/sessions  (kvCacheMode=2)
//   8. endpoint=Primary + session_key=None                   → /v1/agent/oneshot
//
// Non-rsclaw providers (OpenAI/Anthropic/Gemini/…) ignore `endpoint` and
// route purely by `model`.

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
    /// Which rsclaw-server endpoint family this request targets.
    /// Defaults to `Primary` so existing call sites need no change.
    /// Non-rsclaw providers ignore this field.
    pub endpoint: AgentEndpoint,
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
    /// (kvCacheMode=2 only) Per-client `user_system` segment — workspace
    /// MDs, language, skills, platform info. Sent as
    /// `dynamic_prefix.user_system` and is the slot's per-session text
    /// (worker's layer-2 KV cache between `base` and `session_tail`,
    /// rsclaw-protocol §2.1.2 post-2026-05-16 rename). Other providers
    /// ignore this field.
    pub user_system: Option<String>,
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

    /// In-place compact splice (rsclaw protocol §2.4 — kvCacheMode=2 only).
    ///
    /// Asks the provider to splice an existing server-side session:
    /// preserve the first `keep_head_messages` messages' KV unchanged,
    /// drop the middle KV pages, prefill `summary` in their place, and
    /// preserve the last `keep_tail_messages` messages' KV unchanged.
    /// `session_key` is the gateway-side stable key; the provider
    /// resolves it to the wire `session_id` internally.
    ///
    /// `expected_msgs_count` is optimistic concurrency — the total
    /// message count the gateway believes the session has right now.
    /// Server returns 409 on mismatch and the caller MUST fall back to
    /// the replay path.
    ///
    /// Returns the server-reported `msgs_count` after the splice. On any
    /// `Err` callers MUST fall back to `/sessions/replay` (see
    /// `agent/compaction.rs::compact_inner`).
    ///
    /// Default implementation: `Err`. Only rsclaw implements this — it's
    /// kvCacheMode=2-specific and no stateless provider (anthropic /
    /// openai / gemini / ollama) can support it. Adding a default impl
    /// here keeps the trait small and lets callers branch on `Err`
    /// instead of feature-detecting concrete provider types.
    #[allow(unused_variables)]
    fn compact_splice<'a>(
        &'a self,
        session_key: &'a str,
        keep_head_messages: usize,
        summary: &'a str,
        keep_tail_messages: usize,
        expected_msgs_count: Option<usize>,
    ) -> BoxFuture<'a, Result<usize>> {
        let name = self.name().to_owned();
        Box::pin(async move {
            anyhow::bail!("compact splice not supported by provider {name}")
        })
    }
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
