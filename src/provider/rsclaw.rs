//! rsclaw-server kvCacheMode=2 provider — incremental session protocol.
//!
//! Wire-level contract: see `~/dev/rsclaw-llm/docs/rsclaw-protocol.md` v1.1+.
//!
//! Stateful sessions where rsclaw-server is the source of truth for
//! conversation history. Per-turn the client sends only the delta
//! (new user message OR tool_results); the server's KV cache stays
//! hot across turns.
//!
//! This provider rejects requests with `kv_cache_mode != 2` — those
//! must go through one of the regular OAI / Anthropic providers.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use futures::{StreamExt, TryStreamExt, future::BoxFuture};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    ContentPart, LlmProvider, LlmRequest, LlmStream, Message, MessageContent, Role, StreamEvent,
    TokenUsage,
};

/// Default base for an rsclaw-server running on the same host. The
/// `/v1/agent` suffix is the external API mount inside rsclaw-server —
/// the rsclaw-llm `/sessions/...` protocol paths are exposed to clients
/// under that prefix, distinct from `/v1/chat/completions` etc. Setting
/// `RSCLAW_URL` overrides this; that variable should also include the
/// `/v1/agent` segment.
pub const RSCLAW_DEFAULT_BASE: &str = "http://localhost:8090/v1/agent";

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

pub struct RsclawProvider {
    client: Client,
    base_url: String,
    bearer: Option<String>,
    sessions: Arc<Mutex<HashMap<String, SessionEntry>>>,
}

#[derive(Clone, Debug)]
struct SessionEntry {
    /// Server-issued, format `rs_<instance>_<random>`.
    session_id: String,
    /// rsclaw_version this session was opened against. Bump triggers
    /// re-open since prefix cache layout changes invalidate the session.
    rsclaw_version: String,
    /// Largest `req.messages.len()` we've observed on this session.
    /// A subsequent call with a smaller list means the runtime trimmed
    /// history (compaction, repair, reset) and the server-side KV no
    /// longer matches what the gateway thinks the conversation is —
    /// trigger a re-hydrate via /sessions/replay.
    last_seen_msgs_len: usize,
}

impl RsclawProvider {
    pub fn new(base_url: impl Into<String>, bearer: Option<String>) -> Self {
        Self::with_user_agent(base_url, bearer, None)
    }

    /// Create a provider with custom User-Agent.
    pub fn with_user_agent(
        base_url: impl Into<String>,
        bearer: Option<String>,
        user_agent: Option<String>,
    ) -> Self {
        Self {
            client: super::http_client_with_ua(user_agent.as_deref()),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            bearer,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn auth_header(&self) -> Option<(String, String)> {
        self.bearer
            .as_ref()
            .map(|k| ("authorization".to_string(), format!("Bearer {k}")))
    }

    /// Atomically look up a cached session AND validate its freshness.
    /// Returns `None` (forcing a re-hydrate) when the entry is missing,
    /// has a stale `rsclaw_version`, or its `last_seen_msgs_len` exceeds
    /// the incoming `msgs_len` (history was trimmed under our feet).
    /// On success bumps `last_seen_msgs_len` to the new value so the
    /// next call's comparison is against the most recent state.
    fn lookup_and_bump(
        &self,
        session_key: &str,
        rsclaw_version: &str,
        msgs_len: usize,
    ) -> Option<SessionEntry> {
        let mut map = self.sessions.lock().ok()?;
        let entry = map.get_mut(session_key)?;
        if entry.rsclaw_version != rsclaw_version {
            return None;
        }
        if msgs_len < entry.last_seen_msgs_len {
            return None;
        }
        entry.last_seen_msgs_len = msgs_len;
        Some(entry.clone())
    }

    fn store(&self, session_key: &str, entry: SessionEntry) {
        if let Ok(mut map) = self.sessions.lock() {
            map.insert(session_key.to_string(), entry);
        }
    }

    fn forget(&self, session_key: &str) {
        if let Ok(mut map) = self.sessions.lock() {
            map.remove(session_key);
        }
    }
}

// ---------------------------------------------------------------------------
// LlmProvider impl
// ---------------------------------------------------------------------------

impl LlmProvider for RsclawProvider {
    fn name(&self) -> &str {
        "rsclaw"
    }

    fn stream(&self, req: LlmRequest) -> BoxFuture<'_, Result<LlmStream>> {
        Box::pin(async move {
            if req.kv_cache_mode != 2 {
                anyhow::bail!(
                    "rsclaw provider only handles kv_cache_mode=2 (got {}); \
                     route mode 0/1 traffic through openai/anthropic providers",
                    req.kv_cache_mode
                );
            }
            let session_key = req
                .session_key
                .clone()
                .context("rsclaw kv_cache_mode=2 requires session_key on the request")?;

            let split = split_request(&req)?;

            // Lookup or hydrate. Cache miss / mutation happens on first
            // call, version drift, after a prior replay failure, or
            // after the runtime trimmed history (compaction, repair,
            // reset) — all cases where `req.messages` may not match
            // what the server has hydrated. open() can't hydrate, so
            // when history exists we go straight to replay; an empty
            // history list takes the cheaper open() path.
            let entry = match self.lookup_and_bump(
                &session_key,
                split.rsclaw_version,
                req.messages.len(),
            ) {
                Some(e) => e,
                None => {
                    self.forget(&session_key);
                    let history = history_for_replay(&req.messages);
                    let resp = if history.is_empty() {
                        self.open(&split).await?
                    } else {
                        self.replay(&split, history).await?
                    };
                    let entry = SessionEntry {
                        session_id: resp.session_id.clone(),
                        rsclaw_version: resp.rsclaw_version.clone(),
                        last_seen_msgs_len: req.messages.len(),
                    };
                    self.store(&session_key, entry.clone());
                    entry
                }
            };

            let delta = TurnDelta::from_request(&req)?;
            let resp = self.turn(&entry.session_id, &delta, &req).await?;
            let resp = match resp {
                TurnOutcome::Stream(s) => s,
                TurnOutcome::SessionNotFound => {
                    // Recover via /sessions/replay then retry the turn.
                    // History excludes the trailing delta — turn() below
                    // re-sends it. Including it in replay would hydrate
                    // the same message twice (once batched, once as the
                    // turn input) and confuse the model.
                    self.forget(&session_key);
                    let replay_history = history_for_replay(&req.messages);
                    let replayed = self.replay(&split, replay_history).await?;
                    let entry = SessionEntry {
                        session_id: replayed.session_id.clone(),
                        rsclaw_version: replayed.rsclaw_version.clone(),
                        last_seen_msgs_len: req.messages.len(),
                    };
                    self.store(&session_key, entry.clone());
                    match self.turn(&entry.session_id, &delta, &req).await? {
                        TurnOutcome::Stream(s) => s,
                        TurnOutcome::SessionNotFound => anyhow::bail!(
                            "rsclaw: session vanished immediately after replay (id={})",
                            entry.session_id
                        ),
                    }
                }
            };
            Ok(resp)
        })
    }
}

// ---------------------------------------------------------------------------
// Protocol operations: open / turn / replay (internal)
// ---------------------------------------------------------------------------

impl RsclawProvider {
    async fn open(&self, split: &SplitRequest<'_>) -> Result<CreateSessionResp> {
        let url = format!("{}/sessions", self.base_url);
        let body = CreateSessionReq {
            rsclaw_version: split.rsclaw_version,
            user_suffix: split.user_suffix,
            user_tools: &split.user_tools,
            plugins_system: split.plugins_system,
            skills_system: split.skills_system,
            options: Some(split.options.clone()),
        };
        // 180s caps the worst-case prefix-decode time for a fresh
        // session; without an explicit timeout reqwest hangs forever
        // on a stalled server (the 20s connect_timeout only covers TCP
        // establishment, not response wait).
        let mut builder = self
            .client
            .post(&url)
            .timeout(Duration::from_secs(180))
            .json(&body);
        if let Some((k, v)) = self.auth_header() {
            builder = builder.header(k, v);
        }
        let resp = super::send_with_transport_retry(builder)
            .await
            .with_context(|| format!("rsclaw POST {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("rsclaw open session failed {status}: {body}");
        }
        resp.json::<CreateSessionResp>()
            .await
            .context("rsclaw open: parse response")
    }

    async fn replay(
        &self,
        split: &SplitRequest<'_>,
        messages: &[Message],
    ) -> Result<CreateSessionResp> {
        let url = format!("{}/sessions/replay", self.base_url);
        let history: Vec<Value> = messages.iter().map(serialize_history_message).collect();
        let body = ReplayReq {
            rsclaw_version: split.rsclaw_version,
            user_suffix: split.user_suffix,
            user_tools: &split.user_tools,
            plugins_system: split.plugins_system,
            skills_system: split.skills_system,
            history,
            options: Some(split.options.clone()),
        };
        // 300s — replay re-decodes prefix + full history, which is
        // strictly slower than open()'s prefix-only decode (180s).
        // Without an explicit timeout reqwest hangs forever on a
        // stalled server (connect_timeout only covers TCP setup).
        let mut builder = self
            .client
            .post(&url)
            .timeout(Duration::from_secs(300))
            .json(&body);
        if let Some((k, v)) = self.auth_header() {
            builder = builder.header(k, v);
        }
        let resp = super::send_with_transport_retry(builder)
            .await
            .with_context(|| format!("rsclaw POST {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("rsclaw replay failed {status}: {body}");
        }
        resp.json::<CreateSessionResp>()
            .await
            .context("rsclaw replay: parse response")
    }

    async fn turn(
        &self,
        session_id: &str,
        delta: &TurnDelta,
        req: &LlmRequest,
    ) -> Result<TurnOutcome> {
        let url = format!("{}/sessions/{}/turn", self.base_url, session_id);
        let body = TurnReq {
            delta,
            options: Some(TurnOptions::from_request(req)),
            stream: true,
        };
        let mut builder = self
            .client
            .post(&url)
            .timeout(Duration::from_secs(180))
            .json(&body);
        if let Some((k, v)) = self.auth_header() {
            builder = builder.header(k, v);
        }
        let resp = super::send_with_transport_retry(builder)
            .await
            .with_context(|| format!("rsclaw POST {url}"))?;
        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(TurnOutcome::SessionNotFound);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            // 409 version_drift (pinned node upgraded past our
            // rsclaw_version) and 503 backend_unavailable (pinned node
            // gone via heartbeat timeout) are documented session-
            // eviction signals — same recovery path as 404, replay
            // against current rsclaw_version and retry.
            if is_session_evicted(status, &body) {
                return Ok(TurnOutcome::SessionNotFound);
            }
            anyhow::bail!("rsclaw turn failed {status}: {body}");
        }

        let byte_stream = resp.bytes_stream();
        let line_buffer = Arc::new(tokio::sync::Mutex::new(String::new()));
        let utf8_remainder = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let event_stream = byte_stream
            .map_err(|e| anyhow::anyhow!("stream read error: {e}"))
            .then(move |chunk| {
                let line_buffer = line_buffer.clone();
                let utf8_remainder = utf8_remainder.clone();
                async move { parse_sse_chunk(chunk, &line_buffer, &utf8_remainder).await }
            })
            .flat_map(futures::stream::iter);

        Ok(TurnOutcome::Stream(Box::pin(event_stream)))
    }
}

enum TurnOutcome {
    Stream(LlmStream),
    SessionNotFound,
}

// ---------------------------------------------------------------------------
// Wire types — mirror rsclaw-protocol.md §2
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct CreateSessionReq<'a> {
    rsclaw_version: &'a str,
    user_suffix: &'a str,
    user_tools: &'a [Value],
    #[serde(skip_serializing_if = "Option::is_none")]
    plugins_system: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skills_system: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<TurnOptions>,
}

#[derive(Debug, Serialize)]
struct ReplayReq<'a> {
    rsclaw_version: &'a str,
    user_suffix: &'a str,
    user_tools: &'a [Value],
    #[serde(skip_serializing_if = "Option::is_none")]
    plugins_system: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skills_system: Option<&'a str>,
    history: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<TurnOptions>,
}

#[derive(Debug, Deserialize, Clone)]
struct CreateSessionResp {
    session_id: String,
    rsclaw_version: String,
}

#[derive(Debug, Serialize)]
struct TurnReq<'a> {
    #[serde(flatten)]
    delta: &'a TurnDelta,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<TurnOptions>,
    stream: bool,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum TurnDelta {
    User { user_message: String },
    Tools { tool_results: Vec<ToolResultDelta> },
}

impl TurnDelta {
    fn from_request(req: &LlmRequest) -> Result<Self> {
        let last = req
            .messages
            .last()
            .context("rsclaw: empty messages, no delta to send")?;
        if !matches!(last.role, Role::User | Role::Tool) {
            anyhow::bail!(
                "rsclaw: last message must be User or Tool, got {:?}",
                last.role
            );
        }

        // When the assistant calls N tools in parallel, the runtime
        // queues N consecutive Role::Tool messages (one per result).
        // Protocol §2.3 requires a single turn() carry ALL of them in
        // one tool_results array, else server bails with 400
        // tool_results_incomplete. Walk back from the tail collecting
        // every consecutive Tool message; stop at the first non-Tool.
        if matches!(last.role, Role::Tool) {
            let mut tail: Vec<&Message> = Vec::new();
            for m in req.messages.iter().rev() {
                if matches!(m.role, Role::Tool) {
                    tail.push(m);
                } else {
                    break;
                }
            }
            tail.reverse();
            let mut tool_results: Vec<ToolResultDelta> = Vec::new();
            for m in tail {
                if let MessageContent::Parts(parts) = &m.content {
                    for p in parts {
                        if let ContentPart::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } = p
                        {
                            tool_results.push(ToolResultDelta {
                                tool_use_id: tool_use_id.clone(),
                                content: content.clone(),
                                is_error: is_error.unwrap_or(false),
                            });
                        }
                    }
                }
            }
            if tool_results.is_empty() {
                anyhow::bail!("rsclaw: trailing Tool message(s) carried no tool_result parts");
            }
            return Ok(TurnDelta::Tools { tool_results });
        }

        // Role::User branch — the trailing message is treated as one
        // user_message; multiple consecutive User messages aren't a
        // supported delta shape.
        let mut user_text: Option<String> = None;
        match &last.content {
            MessageContent::Text(t) => user_text = Some(t.clone()),
            MessageContent::Parts(parts) => {
                for p in parts {
                    if let ContentPart::Text { text } = p {
                        user_text.get_or_insert_with(String::new).push_str(text);
                    }
                }
            }
        }
        if let Some(t) = user_text {
            Ok(TurnDelta::User { user_message: t })
        } else {
            anyhow::bail!("rsclaw: last message has no usable content for delta")
        }
    }
}

#[derive(Debug, Serialize)]
struct ToolResultDelta {
    tool_use_id: String,
    content: String,
    #[serde(default)]
    is_error: bool,
}

#[derive(Debug, Serialize, Clone, Default)]
struct TurnOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    idle_ttl_secs: Option<u32>,
}

impl TurnOptions {
    fn from_request(req: &LlmRequest) -> Self {
        Self {
            max_tokens: req.max_tokens,
            temperature: req.temperature,
            top_p: None,
            enable_thinking: req.thinking_budget.map(|b| b > 0),
            stop: None,
            idle_ttl_secs: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Request splitting (LlmRequest → protocol fields)
// ---------------------------------------------------------------------------

/// Maps an existing `LlmRequest` (whose system prompt is one combined
/// string) onto the protocol's split fields.
///
/// TODO: once `prompt_builder` exposes the constituent pieces
/// (shared_prefix, user_suffix, plugins_system, skills_system,
/// builtin_tools vs user_tools split), wire those through directly
/// instead of reusing `req.system` as `user_suffix`. Until then, this
/// shim keeps everything functional but burns the prefix cache slot
/// since `shared_prefix == ""` means the version registry never has
/// a hit.
struct SplitRequest<'a> {
    rsclaw_version: &'a str,
    user_suffix: &'a str,
    user_tools: Vec<Value>,
    plugins_system: Option<&'a str>,
    skills_system: Option<&'a str>,
    options: TurnOptions,
}

fn split_request<'a>(req: &'a LlmRequest) -> Result<SplitRequest<'a>> {
    // The model field doubles as the rsclaw_version handle for now —
    // rsclaw-server resolves it. Once prompt_builder splits the prompt
    // we can put the proper version digest here.
    let user_tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            })
        })
        .collect();
    Ok(SplitRequest {
        rsclaw_version: &req.model,
        user_suffix: req.system.as_deref().unwrap_or(""),
        user_tools,
        plugins_system: None,
        skills_system: None,
        options: TurnOptions::from_request(req),
    })
}

/// Returns the history slice to send to `/sessions/replay`: every
/// message except the trailing delta (which `turn()` will re-send).
/// Empty input returns an empty slice — replay can still hydrate a
/// fresh session with no prior turns.
fn history_for_replay(messages: &[Message]) -> &[Message] {
    if messages.is_empty() {
        messages
    } else {
        &messages[..messages.len() - 1]
    }
}

fn serialize_history_message(msg: &Message) -> Value {
    let role = match msg.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "user",
    };
    let content = match &msg.content {
        MessageContent::Text(t) => json!(t),
        MessageContent::Parts(parts) => {
            let mapped: Vec<Value> = parts
                .iter()
                .map(|p| match p {
                    ContentPart::Text { text } => json!({"type":"text","text":text}),
                    ContentPart::Image { url } => {
                        json!({"type":"image","source":{"type":"url","url":url}})
                    }
                    ContentPart::ToolUse { id, name, input } => {
                        json!({"type":"tool_use","id":id,"name":name,"input":input})
                    }
                    ContentPart::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        let mut obj = json!({
                            "type":"tool_result",
                            "tool_use_id":tool_use_id,
                            "content":content,
                        });
                        if let Some(e) = is_error {
                            obj["is_error"] = json!(e);
                        }
                        obj
                    }
                    ContentPart::Reasoning { text } => json!({"type":"thinking","text":text}),
                })
                .collect();
            json!(mapped)
        }
    };
    json!({ "role": role, "content": content })
}

// ---------------------------------------------------------------------------
// SSE parsing — protocol §2.3 stream events
// ---------------------------------------------------------------------------

async fn parse_sse_chunk(
    chunk: Result<bytes::Bytes>,
    line_buffer: &Arc<tokio::sync::Mutex<String>>,
    utf8_remainder: &Arc<tokio::sync::Mutex<Vec<u8>>>,
) -> Vec<Result<StreamEvent>> {
    let mut events: Vec<Result<StreamEvent>> = Vec::new();
    let bytes = match chunk {
        Ok(b) => b,
        Err(e) => {
            events.push(Err(e));
            return events;
        }
    };

    // Stitch this chunk onto any partial UTF-8 left over from the
    // previous chunk; decode strict; stash the trailing invalid bytes
    // (an incomplete multi-byte sequence at the chunk boundary) for
    // the next call. from_utf8_lossy would corrupt CJK / emoji deltas
    // that straddle chunk boundaries by inserting U+FFFD.
    let mut remainder = utf8_remainder.lock().await;
    let stitched: Vec<u8> = if remainder.is_empty() {
        bytes.to_vec()
    } else {
        let mut combined = std::mem::take(&mut *remainder);
        combined.extend_from_slice(&bytes);
        combined
    };
    let decoded: String = match std::str::from_utf8(&stitched) {
        Ok(s) => s.to_owned(),
        Err(e) => {
            let valid_up_to = e.valid_up_to();
            *remainder = stitched[valid_up_to..].to_vec();
            if valid_up_to == 0 {
                return events;
            }
            std::str::from_utf8(&stitched[..valid_up_to])
                .expect("valid_up_to guarantees valid UTF-8")
                .to_owned()
        }
    };
    drop(remainder);

    let mut buf = line_buffer.lock().await;
    buf.push_str(&decoded);
    while let Some(idx) = buf.find('\n') {
        let line = buf[..idx].trim_end_matches('\r').to_string();
        buf.drain(..=idx);
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        if payload == "[DONE]" {
            continue;
        }
        let value: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(e) => {
                events.push(Err(anyhow::anyhow!("rsclaw SSE parse: {e}; line: {payload}")));
                continue;
            }
        };
        match value.get("type").and_then(|v| v.as_str()) {
            Some("delta") => {
                let s = value.get("content").and_then(|v| v.as_str()).unwrap_or("");
                if !s.is_empty() {
                    events.push(Ok(StreamEvent::TextDelta(s.to_string())));
                }
            }
            Some("thinking") => {
                let s = value.get("content").and_then(|v| v.as_str()).unwrap_or("");
                if !s.is_empty() {
                    events.push(Ok(StreamEvent::ReasoningDelta(s.to_string())));
                }
            }
            Some("tool_call") => {
                let id = value.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let name = value.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let input = value.get("input").cloned().unwrap_or(Value::Null);
                events.push(Ok(StreamEvent::ToolCall { id, name, input }));
            }
            Some("done") => {
                let usage = value.get("usage").and_then(|u| {
                    let input = u.get("input_tokens").and_then(|v| v.as_u64())? as u32;
                    let output = u.get("output_tokens").and_then(|v| v.as_u64())? as u32;
                    Some(TokenUsage { input, output })
                });
                events.push(Ok(StreamEvent::Done { usage }));
            }
            Some("error") => {
                let detail = value
                    .get("detail")
                    .and_then(|v| v.as_str())
                    .unwrap_or("rsclaw stream error");
                events.push(Ok(StreamEvent::Error(detail.to_string())));
            }
            _ => {}
        }
    }
    events
}

/// True when the (status, body) pair is a documented session-eviction
/// signal that the gateway should recover from via replay:
/// - `409 version_drift` — pinned node upgraded past our rsclaw_version
/// - `503 backend_unavailable` — pinned node gone (heartbeat timeout)
///
/// `503 no_backend_available` (capacity exhaustion) is intentionally NOT
/// recoverable here — replay would just hit the same wall.
fn is_session_evicted(status: StatusCode, body: &str) -> bool {
    let code = serde_json::from_str::<Value>(body)
        .ok()
        .as_ref()
        .and_then(|v| v.get("error"))
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    match (status, code.as_deref()) {
        (StatusCode::CONFLICT, Some("version_drift")) => true,
        (StatusCode::SERVICE_UNAVAILABLE, Some("backend_unavailable")) => true,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolDef;

    #[tokio::test]
    async fn parse_sse_chunk_recovers_split_utf8() {
        // SSE delta line carrying "你好" (U+4F60 = E4 BD A0, U+597D =
        // E5 A5 BD), with the byte split landing in the middle of the
        // first character.
        let line_full = b"data: {\"type\":\"delta\",\"content\":\"\xe4\xbd\xa0\xe5\xa5\xbd\"}\n";
        let split = 14; // "data: {\"type\":" prefix is 14 bytes
        let (a, b) = line_full.split_at(split);
        let (b, c) = b.split_at(11); // straddles the first 你 (E4 BD A0)
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));

        for piece in [a, b, c] {
            let _ = parse_sse_chunk(Ok(bytes::Bytes::copy_from_slice(piece)), &buf, &rem).await;
        }
        let evs = parse_sse_chunk(Ok(bytes::Bytes::from_static(b"")), &buf, &rem).await;

        let texts: Vec<_> = evs
            .into_iter()
            .filter_map(|e| match e {
                Ok(StreamEvent::TextDelta(t)) => Some(t),
                _ => None,
            })
            .collect();
        // Either fully delivered now or already delivered in an earlier piece.
        let all_text: String = texts.into_iter().collect();
        // The final newline-terminated event must produce 你好 verbatim, no U+FFFD.
        assert!(
            !all_text.contains('\u{FFFD}'),
            "expected no replacement char, got {all_text:?}"
        );
    }

    #[test]
    fn is_session_evicted_recognizes_version_drift() {
        let body = r#"{"error":{"code":"version_drift","detail":"node has been upgraded"}}"#;
        assert!(is_session_evicted(StatusCode::CONFLICT, body));
    }

    #[test]
    fn is_session_evicted_recognizes_backend_unavailable() {
        let body = r#"{"error":{"code":"backend_unavailable","detail":"heartbeat timeout"}}"#;
        assert!(is_session_evicted(StatusCode::SERVICE_UNAVAILABLE, body));
    }

    #[test]
    fn is_session_evicted_excludes_no_backend_available() {
        // Capacity exhaustion — replay won't help, must bail.
        let body = r#"{"error":{"code":"no_backend_available","detail":"all GPUs saturated"}}"#;
        assert!(!is_session_evicted(StatusCode::SERVICE_UNAVAILABLE, body));
    }

    #[test]
    fn is_session_evicted_rejects_status_code_mismatch() {
        // Right code, wrong status — don't recover.
        let body = r#"{"error":{"code":"version_drift","detail":"x"}}"#;
        assert!(!is_session_evicted(StatusCode::SERVICE_UNAVAILABLE, body));
        let body = r#"{"error":{"code":"backend_unavailable","detail":"x"}}"#;
        assert!(!is_session_evicted(StatusCode::CONFLICT, body));
    }

    #[test]
    fn is_session_evicted_rejects_malformed_body() {
        assert!(!is_session_evicted(StatusCode::CONFLICT, ""));
        assert!(!is_session_evicted(StatusCode::CONFLICT, "not json"));
        assert!(!is_session_evicted(
            StatusCode::CONFLICT,
            r#"{"code":"version_drift"}"#,
        ));
    }

    fn req_with(messages: Vec<Message>, mode: u8, key: Option<&str>) -> LlmRequest {
        LlmRequest {
            model: "2026.5.5".into(),
            messages,
            tools: vec![],
            system: Some("you are an agent".into()),
            max_tokens: None,
            temperature: None,
            frequency_penalty: None,
            thinking_budget: None,
            kv_cache_mode: mode,
            session_key: key.map(str::to_string),
        }
    }

    #[test]
    fn split_request_maps_tools() {
        let mut req = req_with(vec![], 2, Some("k"));
        req.tools.push(ToolDef {
            name: "search".into(),
            description: "search the web".into(),
            parameters: json!({"type":"object","properties":{}}),
        });
        let split = split_request(&req).unwrap();
        assert_eq!(split.user_tools.len(), 1);
        assert_eq!(split.user_tools[0]["name"], "search");
        assert!(split.user_tools[0].get("input_schema").is_some());
    }

    #[test]
    fn history_for_replay_drops_trailing_delta() {
        let m = |role, txt: &str| Message {
            role,
            content: MessageContent::Text(txt.into()),
        };
        let msgs = vec![
            m(Role::User, "hi"),
            m(Role::Assistant, "yo"),
            m(Role::User, "again"),
        ];
        let slice = history_for_replay(&msgs);
        assert_eq!(slice.len(), 2);
        assert!(matches!(slice[0].role, Role::User));
        assert!(matches!(slice[1].role, Role::Assistant));
    }

    #[test]
    fn history_for_replay_handles_empty_and_singleton() {
        let empty: Vec<Message> = Vec::new();
        assert!(history_for_replay(&empty).is_empty());
        let one = vec![Message {
            role: Role::User,
            content: MessageContent::Text("solo".into()),
        }];
        assert!(history_for_replay(&one).is_empty());
    }

    #[test]
    fn turn_delta_user_text() {
        let req = req_with(
            vec![Message {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
            }],
            2,
            Some("k"),
        );
        let delta = TurnDelta::from_request(&req).unwrap();
        let body = serde_json::to_value(&delta).unwrap();
        assert_eq!(body["user_message"], "hello");
    }

    #[test]
    fn turn_delta_tool_results() {
        let req = req_with(
            vec![Message {
                role: Role::Tool,
                content: MessageContent::Parts(vec![ContentPart::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "ok".into(),
                    is_error: None,
                }]),
            }],
            2,
            Some("k"),
        );
        let delta = TurnDelta::from_request(&req).unwrap();
        let body = serde_json::to_value(&delta).unwrap();
        assert_eq!(body["tool_results"][0]["tool_use_id"], "toolu_1");
    }

    #[test]
    fn lookup_and_bump_evicts_on_history_shrink() {
        let provider = RsclawProvider::new("http://x", None);
        provider.store(
            "k",
            SessionEntry {
                session_id: "rs_w7_abc".into(),
                rsclaw_version: "2026.5.5".into(),
                last_seen_msgs_len: 12,
            },
        );
        // Same len → cached entry returned, last_seen unchanged.
        assert!(provider.lookup_and_bump("k", "2026.5.5", 12).is_some());
        // Growth → bumped, returned.
        assert!(provider.lookup_and_bump("k", "2026.5.5", 14).is_some());
        // Shrink (compaction trimmed history) → None, caller re-hydrates.
        assert!(provider.lookup_and_bump("k", "2026.5.5", 8).is_none());
        // Version drift → None even if len matches.
        assert!(provider.lookup_and_bump("k", "2026.5.6", 14).is_none());
        // Missing key → None.
        assert!(provider.lookup_and_bump("missing", "2026.5.5", 14).is_none());
    }

    #[test]
    fn turn_delta_collects_parallel_tool_results() {
        // Assistant called 3 tools in parallel → 3 trailing Tool messages.
        let tool_msg = |id: &str, body: &str| Message {
            role: Role::Tool,
            content: MessageContent::Parts(vec![ContentPart::ToolResult {
                tool_use_id: id.into(),
                content: body.into(),
                is_error: None,
            }]),
        };
        let req = req_with(
            vec![
                Message {
                    role: Role::User,
                    content: MessageContent::Text("do three things".into()),
                },
                tool_msg("toolu_a", "result a"),
                tool_msg("toolu_b", "result b"),
                tool_msg("toolu_c", "result c"),
            ],
            2,
            Some("k"),
        );
        let delta = TurnDelta::from_request(&req).unwrap();
        let body = serde_json::to_value(&delta).unwrap();
        let arr = body["tool_results"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["tool_use_id"], "toolu_a");
        assert_eq!(arr[1]["tool_use_id"], "toolu_b");
        assert_eq!(arr[2]["tool_use_id"], "toolu_c");
    }

    #[test]
    fn turn_delta_does_not_cross_user_boundary() {
        // A non-Tool message between User and the trailing Tool must
        // stop the back-walk — the earlier Tool belongs to a prior turn.
        let req = req_with(
            vec![
                Message {
                    role: Role::Tool,
                    content: MessageContent::Parts(vec![ContentPart::ToolResult {
                        tool_use_id: "toolu_old".into(),
                        content: "stale".into(),
                        is_error: None,
                    }]),
                },
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Text("ack".into()),
                },
                Message {
                    role: Role::Tool,
                    content: MessageContent::Parts(vec![ContentPart::ToolResult {
                        tool_use_id: "toolu_new".into(),
                        content: "fresh".into(),
                        is_error: None,
                    }]),
                },
            ],
            2,
            Some("k"),
        );
        let delta = TurnDelta::from_request(&req).unwrap();
        let body = serde_json::to_value(&delta).unwrap();
        let arr = body["tool_results"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["tool_use_id"], "toolu_new");
    }

    #[test]
    fn rejects_non_kv2_mode() {
        let provider = RsclawProvider::new("http://x", None);
        let req = req_with(vec![], 1, Some("k"));
        let err = match futures::executor::block_on(provider.stream(req)) {
            Ok(_) => panic!("expected error for kv_cache_mode=1"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("kv_cache_mode=2"));
    }

    #[test]
    fn rejects_missing_session_key() {
        let provider = RsclawProvider::new("http://x", None);
        let req = req_with(vec![], 2, None);
        let err = match futures::executor::block_on(provider.stream(req)) {
            Ok(_) => panic!("expected error for missing session_key"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("session_key"));
    }
}
