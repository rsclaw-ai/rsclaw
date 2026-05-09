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

/// How long to wait for `/sessions/<id>/turn` to start responding
/// (TCP connect + TLS + send body + receive headers + first byte).
/// Once the body stream begins this deadline no longer applies — the
/// SSE body is allowed to take as long as the model needs.
const TURN_HEADERS_TIMEOUT: Duration = Duration::from_secs(60);

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

    fn stream(&self, mut req: LlmRequest) -> BoxFuture<'_, Result<LlmStream>> {
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

            // The runtime appends `Role::System` messages AFTER the
            // User/Tool delta on the first iteration of any turn that
            // has dynamic /ctx or just-installed-skill blocks (see
            // agent/runtime.rs ~4068-4082). Without this, `from_request`
            // sees `Role::System` as the last message and aborts the
            // entire turn. Fold trailing System text back into the
            // preceding User delta so the model still gets the context.
            normalize_trailing_system(&mut req.messages);

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
                        rsclaw_version: resp.rsclaw_version_or(split.rsclaw_version),
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
                        rsclaw_version: replayed.rsclaw_version_or(split.rsclaw_version),
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
        // Protocol §2.2 history accepts only `role: "user"` and
        // `role: "assistant"`. The runtime, however, threads
        // `Role::System` messages into the conversation list for
        // plugins/skills prefixes, just-installed skills, and
        // dynamic /ctx blocks (see agent/runtime.rs ~4054). Sending
        // those through as-is would trigger `400 invalid_history`
        // and tank every replay. Pull them out and append their
        // text to `user_suffix` so the content still reaches the
        // server — at the static-prefix slot, the only place the
        // protocol allows non-conversational system content.
        let (filtered, extra_suffix) = split_system_messages(messages);
        let user_suffix_owned: String = if extra_suffix.is_empty() {
            String::new()
        } else if split.user_suffix.is_empty() {
            extra_suffix
        } else {
            format!("{}\n\n{}", split.user_suffix, extra_suffix)
        };
        let user_suffix: &str = if user_suffix_owned.is_empty() {
            split.user_suffix
        } else {
            &user_suffix_owned
        };
        let history: Vec<Value> = filtered.iter().map(|m| serialize_history_message(m)).collect();
        let body = ReplayReq {
            rsclaw_version: split.rsclaw_version,
            user_suffix,
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
        // No `.timeout()` on the builder: reqwest's `.timeout()` is a
        // total deadline that includes the streaming body, so a 180s
        // cap would kill long generations (reasoning models with
        // extended thinking + large outputs routinely run past three
        // minutes). Instead bound only the time-to-response-headers
        // with `tokio::time::timeout` around the send so a wedged
        // server still surfaces fast — once headers arrive, the body
        // stream is allowed to take as long as it needs. Connection
        // liveness during streaming is covered by the client-level
        // `tcp_keepalive(30s)` configured in `http_client_with_ua`.
        let mut builder = self.client.post(&url).json(&body);
        if let Some((k, v)) = self.auth_header() {
            builder = builder.header(k, v);
        }
        let send_fut = super::send_with_transport_retry(builder);
        let resp = match tokio::time::timeout(TURN_HEADERS_TIMEOUT, send_fut).await {
            Ok(r) => r.with_context(|| format!("rsclaw POST {url}"))?,
            Err(_) => anyhow::bail!(
                "rsclaw turn: timed out waiting for response headers after {}s ({url})",
                TURN_HEADERS_TIMEOUT.as_secs()
            ),
        };
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
    /// Per protocol §2.1 the create response includes `rsclaw_version`
    /// (the registered/canonical version on the chosen node, which may
    /// differ from the requested alias). Per §2.2 the **replay**
    /// response does NOT include it — and rsclaw-server's backend
    /// passes the upstream JSON straight through, so the field is
    /// absent on replay. Default to empty here; the caller falls back
    /// to the request's `rsclaw_version` when this is empty.
    #[serde(default)]
    rsclaw_version: String,
}

impl CreateSessionResp {
    /// Returns the response's `rsclaw_version` if non-empty, else
    /// `fallback` (typically `split.rsclaw_version`). Replay responses
    /// per §2.2 don't carry the field, so the request's version is
    /// the authoritative value for cache-key comparison.
    fn rsclaw_version_or(&self, fallback: &str) -> String {
        if self.rsclaw_version.is_empty() {
            fallback.to_owned()
        } else {
            self.rsclaw_version.clone()
        }
    }
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

/// Serializer for `Option<f32>` that routes the value through
/// `super::json_f32` (rounds to 2 decimal places) instead of the default
/// f32 → f64 path which leaks IEEE 754 precision artefacts (0.6_f32 →
/// 0.6000000238418579 in JSON output). Mirrors what every other provider
/// in this crate does manually with `body["temperature"] = json_f32(t)`.
fn ser_opt_f32<S: serde::Serializer>(
    v: &Option<f32>,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    match v {
        None => s.serialize_none(),
        Some(f) => super::json_f32(*f).serialize(s),
    }
}

#[derive(Debug, Serialize, Clone, Default)]
struct TurnOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "ser_opt_f32")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "ser_opt_f32")]
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
///
/// When the assistant calls N tools in parallel the runtime queues N
/// consecutive `Role::Tool` messages, and `TurnDelta::from_request`
/// folds ALL of them into a single tool_results delta (protocol §2.3
/// requires it). To keep the two sides symmetric the history slice
/// must drop every consecutive trailing `Role::Tool` — dropping just
/// one would leave the other N-1 in history, the server would replay
/// them into the KV, and then the turn would re-send the same
/// tool_results, hydrating duplicates.
///
/// For a User-trailing list (the iter-1 case after
/// `normalize_trailing_system` ran) we drop exactly one message.
fn history_for_replay(messages: &[Message]) -> &[Message] {
    if messages.is_empty() {
        return messages;
    }
    let last = &messages[messages.len() - 1];
    if matches!(last.role, Role::Tool) {
        let mut keep = messages.len();
        while keep > 0 && matches!(messages[keep - 1].role, Role::Tool) {
            keep -= 1;
        }
        &messages[..keep]
    } else {
        &messages[..messages.len() - 1]
    }
}

/// Partition a history slice into (non-system messages, concatenated
/// system text). The runtime threads `Role::System` messages through
/// the conversation list for plugins/skills/ctx blocks, but protocol
/// §2.2 only accepts `user` / `assistant` in history — so we lift the
/// system text out and let the caller append it to `user_suffix`.
/// Order of system blocks is preserved within the returned String;
/// blocks are joined with a blank line. Non-text content on a
/// `Role::System` message is dropped (system messages are documented
/// as text-only in the runtime).
fn split_system_messages(messages: &[Message]) -> (Vec<&Message>, String) {
    let mut filtered: Vec<&Message> = Vec::with_capacity(messages.len());
    let mut sys_parts: Vec<String> = Vec::new();
    for m in messages {
        if matches!(m.role, Role::System) {
            match &m.content {
                MessageContent::Text(t) => sys_parts.push(t.clone()),
                MessageContent::Parts(parts) => {
                    let mut joined = String::new();
                    for p in parts {
                        if let ContentPart::Text { text } = p {
                            joined.push_str(text);
                        }
                    }
                    if !joined.is_empty() {
                        sys_parts.push(joined);
                    }
                }
            }
        } else {
            filtered.push(m);
        }
    }
    (filtered, sys_parts.join("\n\n"))
}

/// Pull any trailing `Role::System` messages off the end of `messages`
/// and fold their text into the preceding `Role::User` message.
///
/// The runtime appends `Role::System` blocks (dynamic /ctx, just-
/// installed skills) AFTER the User delta on the first iteration of
/// each turn (`turn_scratchpad` empty — see agent/runtime.rs). On
/// later iterations the scratchpad's Assistant/Tool entries follow,
/// so trailing-System only happens iter-1. `TurnDelta::from_request`
/// rejects a trailing System ("last message must be User or Tool"),
/// failing the whole turn — fold the text inline so the model still
/// sees the dynamic context, just as part of the user_message body.
///
/// If the message immediately before the trailing System block(s) is
/// `Role::Tool` (parallel tool_results case, theoretical — runtime
/// doesn't currently inject System after Tool, but defend anyway),
/// drop the System text. Persistent system content already lives in
/// `user_suffix` from the prior open/replay; only the per-iteration
/// dynamic context would be lost, which the protocol has no slot
/// for in a tool_results delta.
fn normalize_trailing_system(messages: &mut Vec<Message>) {
    let mut trailing: Vec<String> = Vec::new();
    while matches!(messages.last(), Some(m) if matches!(m.role, Role::System)) {
        let m = messages.pop().expect("matched Some above");
        let txt = match m.content {
            MessageContent::Text(t) => t,
            MessageContent::Parts(parts) => parts
                .into_iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        };
        if !txt.is_empty() {
            trailing.push(txt);
        }
    }
    if trailing.is_empty() {
        return;
    }
    trailing.reverse();
    let combined = trailing.join("\n\n");
    match messages.last_mut() {
        Some(last) if matches!(last.role, Role::User) => match &mut last.content {
            MessageContent::Text(t) => {
                if t.is_empty() {
                    *t = combined;
                } else {
                    t.push_str("\n\n");
                    t.push_str(&combined);
                }
            }
            MessageContent::Parts(parts) => {
                parts.push(ContentPart::Text { text: combined });
            }
        },
        _ => {
            // Tool / empty — nothing to fold into. Drop silently.
        }
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
        // Per WHATWG SSE the space after the colon is optional, so
        // `data:{...}` is just as valid as `data: {...}`. Accept both
        // — matches the openai/anthropic/gemini providers in this
        // crate. Strip the colon, then trim leading ASCII spaces.
        let Some(payload) = line.strip_prefix("data:").map(|s| s.trim_start_matches(' ')) else {
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
                // Default to an empty JSON object — `Value::Null` would
                // make downstream `input.as_object()` calls return None
                // and force every consumer to `unwrap_or(&empty_map)`.
                // The other three providers in this crate (anthropic,
                // gemini, openai) all default to an empty object; align
                // here so the runtime can treat the field uniformly.
                let input = match value.get("input").cloned() {
                    Some(Value::Null) | None => Value::Object(Default::default()),
                    Some(other) => other,
                };
                events.push(Ok(StreamEvent::ToolCall { id, name, input }));
            }
            Some("done") => {
                // If the `usage` object is present, surface it — even
                // partially. Defaulting missing fields to 0 (rather than
                // dropping the entire TokenUsage on a single missing
                // field) matches the anthropic provider's pattern and
                // means a server that ships e.g. `input_tokens` without
                // `output_tokens` still gets the input count through to
                // cost-tracking instead of losing both.
                let usage = value.get("usage").map(|u| TokenUsage {
                    input: u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
                    output: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
                });
                events.push(Ok(StreamEvent::Done { usage }));
            }
            Some("error") => {
                // Protocol §2.3: error event carries both `code` and
                // `detail`. Preserve both so downstream consumers can
                // discriminate by code (e.g. `slot_evicted` →
                // replay-and-retry) and operators see the full message
                // in tail logs.
                let code = value.get("code").and_then(|v| v.as_str()).unwrap_or("");
                let detail = value.get("detail").and_then(|v| v.as_str()).unwrap_or("");
                let msg = match (code.is_empty(), detail.is_empty()) {
                    (false, false) => format!("rsclaw stream error [{code}]: {detail}"),
                    (false, true) => format!("rsclaw stream error [{code}]"),
                    (true, false) => format!("rsclaw stream error: {detail}"),
                    (true, true) => "rsclaw stream error".to_string(),
                };
                events.push(Ok(StreamEvent::Error(msg)));
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

    #[tokio::test]
    async fn parse_sse_chunk_accepts_data_without_leading_space() {
        // SSE field syntax allows the space after the colon to be
        // omitted; rsclaw-server (or any node that routes through
        // hyper / nginx with comp-stripping middleware) may emit
        // `data:{...}` without a space. Both forms must parse.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = b"data:{\"type\":\"delta\",\"content\":\"hi\"}\n";
        let evs = parse_sse_chunk(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let texts: Vec<_> = evs
            .into_iter()
            .filter_map(|e| match e {
                Ok(StreamEvent::TextDelta(t)) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["hi".to_string()]);
    }

    #[tokio::test]
    async fn parse_sse_chunk_emits_done_with_usage() {
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = b"data: {\"type\":\"done\",\"usage\":{\"input_tokens\":17,\"output_tokens\":42}}\n";
        let evs = parse_sse_chunk(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let mut saw_done = false;
        for e in evs {
            if let Ok(StreamEvent::Done { usage }) = e {
                let u = usage.expect("usage should be populated");
                assert_eq!(u.input, 17);
                assert_eq!(u.output, 42);
                saw_done = true;
            }
        }
        assert!(saw_done, "expected one Done event");
    }

    #[tokio::test]
    async fn parse_sse_chunk_emits_done_without_usage() {
        // Server may omit usage on early termination — Done must still fire.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = b"data: {\"type\":\"done\"}\n";
        let evs = parse_sse_chunk(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let mut saw_done = false;
        for e in evs {
            if let Ok(StreamEvent::Done { usage }) = e {
                assert!(usage.is_none());
                saw_done = true;
            }
        }
        assert!(saw_done, "expected Done event even without usage");
    }

    #[tokio::test]
    async fn parse_sse_chunk_tool_call_defaults_input_to_empty_object() {
        // Tools without parameters legitimately ship `input: {}`, which
        // already arrives as Value::Object — verify pass-through.
        // But missing input or null input should ALSO produce an empty
        // object, matching the anthropic/gemini/openai providers, so
        // runtime consumers can call .as_object() without first
        // matching Null.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = b"data: {\"type\":\"tool_call\",\"id\":\"toolu_xyz\",\"name\":\"get_time\"}\n";
        let evs = parse_sse_chunk(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let input = evs
            .into_iter()
            .find_map(|e| match e {
                Ok(StreamEvent::ToolCall { input, .. }) => Some(input),
                _ => None,
            })
            .expect("expected one ToolCall event");
        assert!(
            input.as_object().is_some_and(|m| m.is_empty()),
            "expected empty object, got {input:?}"
        );
    }

    #[tokio::test]
    async fn parse_sse_chunk_tool_call_normalizes_explicit_null_input() {
        // A misbehaving server might emit `"input": null`. Don't pass
        // the Null through — coerce to {} so downstream stays uniform.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line =
            b"data: {\"type\":\"tool_call\",\"id\":\"t\",\"name\":\"n\",\"input\":null}\n";
        let evs = parse_sse_chunk(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let input = evs
            .into_iter()
            .find_map(|e| match e {
                Ok(StreamEvent::ToolCall { input, .. }) => Some(input),
                _ => None,
            })
            .expect("expected one ToolCall event");
        assert!(
            input.as_object().is_some_and(|m| m.is_empty()),
            "explicit null should normalize to empty object, got {input:?}"
        );
    }

    #[tokio::test]
    async fn parse_sse_chunk_tool_call_preserves_populated_input() {
        // Round-trip a real input — the normalization must NOT clobber
        // a non-null object. Regression test for the obvious foot-gun
        // of a too-eager fallback.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = br#"data: {"type":"tool_call","id":"t","name":"read_file","input":{"path":"x.rs"}}
"#;
        let evs = parse_sse_chunk(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let input = evs
            .into_iter()
            .find_map(|e| match e {
                Ok(StreamEvent::ToolCall { input, .. }) => Some(input),
                _ => None,
            })
            .expect("expected one ToolCall event");
        assert_eq!(
            input.get("path").and_then(Value::as_str),
            Some("x.rs"),
            "populated input must round-trip; got {input:?}"
        );
    }

    #[tokio::test]
    async fn parse_sse_chunk_done_preserves_partial_usage() {
        // Server may legitimately ship usage with one of the two token
        // counts missing (e.g. early-termination or a buggy proxy that
        // strips fields). Old parser used `?` to short-circuit, which
        // nuked the entire TokenUsage on any missing field. Defaulting
        // each side to 0 keeps the half we DID get.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = b"data: {\"type\":\"done\",\"usage\":{\"input_tokens\":17}}\n";
        let evs = parse_sse_chunk(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let usage = evs
            .into_iter()
            .find_map(|e| match e {
                Ok(StreamEvent::Done { usage }) => Some(usage),
                _ => None,
            })
            .expect("expected one Done event")
            .expect("usage should survive partial fields");
        assert_eq!(usage.input, 17);
        assert_eq!(usage.output, 0);
    }

    #[tokio::test]
    async fn parse_sse_chunk_error_preserves_code_and_detail() {
        // Protocol §2.3: error frame is `{type:"error", code, detail}`.
        // Both fields must survive into StreamEvent::Error so callers
        // can branch on code (e.g. slot_evicted → replay) and humans
        // see the detail in tail logs.
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = br#"data: {"type":"error","code":"slot_evicted","detail":"slot was reclaimed mid-decode"}
"#;
        let evs = parse_sse_chunk(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let mut msgs = Vec::new();
        for e in evs {
            if let Ok(StreamEvent::Error(m)) = e {
                msgs.push(m);
            }
        }
        assert_eq!(msgs.len(), 1, "expected exactly one Error event");
        assert!(msgs[0].contains("slot_evicted"), "missing code: {}", msgs[0]);
        assert!(
            msgs[0].contains("slot was reclaimed mid-decode"),
            "missing detail: {}",
            msgs[0]
        );
    }

    #[tokio::test]
    async fn parse_sse_chunk_error_falls_back_when_code_missing() {
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = br#"data: {"type":"error","detail":"upstream hung up"}
"#;
        let evs = parse_sse_chunk(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let msg = evs
            .into_iter()
            .find_map(|e| match e {
                Ok(StreamEvent::Error(m)) => Some(m),
                _ => None,
            })
            .expect("expected one Error event");
        assert!(msg.contains("upstream hung up"), "missing detail: {msg}");
        assert!(!msg.contains("[]"), "empty-code marker leaked: {msg}");
    }

    #[tokio::test]
    async fn parse_sse_chunk_error_falls_back_when_detail_missing() {
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = br#"data: {"type":"error","code":"version_drift"}
"#;
        let evs = parse_sse_chunk(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let msg = evs
            .into_iter()
            .find_map(|e| match e {
                Ok(StreamEvent::Error(m)) => Some(m),
                _ => None,
            })
            .expect("expected one Error event");
        assert!(msg.contains("version_drift"), "missing code: {msg}");
        assert!(!msg.ends_with(": "), "trailing empty-detail leaked: {msg}");
    }

    #[tokio::test]
    async fn parse_sse_chunk_error_uses_default_when_both_missing() {
        let buf = Arc::new(tokio::sync::Mutex::new(String::new()));
        let rem = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let line = b"data: {\"type\":\"error\"}\n";
        let evs = parse_sse_chunk(Ok(bytes::Bytes::copy_from_slice(line)), &buf, &rem).await;
        let msg = evs
            .into_iter()
            .find_map(|e| match e {
                Ok(StreamEvent::Error(m)) => Some(m),
                _ => None,
            })
            .expect("expected one Error event");
        assert_eq!(msg, "rsclaw stream error");
    }

    #[test]
    fn turn_headers_timeout_is_bounded_and_finite() {
        // Sanity-check the constant: streaming turns rely on this for
        // wedged-server detection. Too short would cause spurious
        // failures on a slow TLS handshake; too long would let a dead
        // server hang the runtime indefinitely.
        let s = TURN_HEADERS_TIMEOUT.as_secs();
        assert!((30..=120).contains(&s), "TURN_HEADERS_TIMEOUT={s}s out of range");
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
    fn history_for_replay_drops_all_consecutive_trailing_tools() {
        // Parallel-tool case: assistant emits N tool_use blocks, runtime
        // queues N consecutive Role::Tool messages, from_request folds
        // them into a single Tools delta. history_for_replay must drop
        // ALL N — dropping just one leaves N-1 tool_results in history,
        // server replays them into KV, then turn() re-sends them as the
        // delta, hydrating duplicates.
        let m = |role, txt: &str| Message {
            role,
            content: MessageContent::Text(txt.into()),
        };
        let tool = |id: &str| Message {
            role: Role::Tool,
            content: MessageContent::Parts(vec![ContentPart::ToolResult {
                tool_use_id: id.into(),
                content: "ok".into(),
                is_error: None,
            }]),
        };
        let msgs = vec![
            m(Role::User, "do all three"),
            m(Role::Assistant, "calling tools"),
            tool("toolu_1"),
            tool("toolu_2"),
            tool("toolu_3"),
        ];
        let slice = history_for_replay(&msgs);
        assert_eq!(slice.len(), 2);
        assert!(matches!(slice[0].role, Role::User));
        assert!(matches!(slice[1].role, Role::Assistant));
    }

    #[test]
    fn history_for_replay_keeps_earlier_tool_messages() {
        // Sequential-tool case across a multi-iteration turn:
        // [..., User, Asst, Tool, Asst, Tool, Asst, Tool] — only the
        // FINAL contiguous Tool run belongs to the current step's delta;
        // earlier Tool messages are part of completed sub-iterations and
        // stay in history.
        let m = |role, txt: &str| Message {
            role,
            content: MessageContent::Text(txt.into()),
        };
        let tool = |id: &str| Message {
            role: Role::Tool,
            content: MessageContent::Parts(vec![ContentPart::ToolResult {
                tool_use_id: id.into(),
                content: "ok".into(),
                is_error: None,
            }]),
        };
        let msgs = vec![
            m(Role::User, "go"),
            m(Role::Assistant, "step1"),
            tool("a"),
            m(Role::Assistant, "step2"),
            tool("b"),
        ];
        let slice = history_for_replay(&msgs);
        assert_eq!(slice.len(), 4);
        // The earlier Tool stays in history.
        assert!(matches!(slice[2].role, Role::Tool));
        // Trailing Tool dropped.
        assert!(matches!(slice[3].role, Role::Assistant));
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
    fn split_system_messages_lifts_system_to_suffix() {
        // Mid-conversation Role::System blocks (plugins, skills,
        // /ctx) must NOT appear in /sessions/replay history — the
        // protocol rejects role:"system" with 400 invalid_history.
        let m = |role: Role, txt: &str| Message {
            role,
            content: MessageContent::Text(txt.into()),
        };
        let msgs = vec![
            m(Role::System, "PLUGINS"),
            m(Role::System, "SKILLS"),
            m(Role::User, "hi"),
            m(Role::Assistant, "yo"),
            m(Role::System, "## New Skill Installed\nfoo"),
            m(Role::User, "again"),
        ];
        let (filtered, suffix) = split_system_messages(&msgs);
        assert_eq!(filtered.len(), 3);
        for m in &filtered {
            assert!(!matches!(m.role, Role::System));
        }
        assert_eq!(suffix, "PLUGINS\n\nSKILLS\n\n## New Skill Installed\nfoo");
    }

    #[test]
    fn split_system_messages_handles_text_parts() {
        let msgs = vec![Message {
            role: Role::System,
            content: MessageContent::Parts(vec![
                ContentPart::Text { text: "hello ".into() },
                ContentPart::Text { text: "world".into() },
            ]),
        }];
        let (filtered, suffix) = split_system_messages(&msgs);
        assert!(filtered.is_empty());
        assert_eq!(suffix, "hello world");
    }

    #[test]
    fn split_system_messages_empty_when_no_system() {
        let msgs = vec![Message {
            role: Role::User,
            content: MessageContent::Text("hi".into()),
        }];
        let (filtered, suffix) = split_system_messages(&msgs);
        assert_eq!(filtered.len(), 1);
        assert!(suffix.is_empty());
    }

    #[test]
    fn normalize_trailing_system_folds_into_user_text() {
        // Runtime appended a dynamic-ctx Role::System after the User
        // delta — fold it into the user_message body so from_request
        // sees a User-trailing list.
        let m = |role: Role, txt: &str| Message {
            role,
            content: MessageContent::Text(txt.into()),
        };
        let mut msgs = vec![
            m(Role::User, "fix the bug"),
            m(Role::System, "## Dynamic /ctx\nworking on handler.py"),
        ];
        normalize_trailing_system(&mut msgs);
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0].role, Role::User));
        let MessageContent::Text(t) = &msgs[0].content else {
            panic!("expected Text content")
        };
        assert_eq!(t, "fix the bug\n\n## Dynamic /ctx\nworking on handler.py");
    }

    #[test]
    fn normalize_trailing_system_concatenates_multiple_in_order() {
        // Two runtime-appended System blocks (e.g. new_skills_tail +
        // dynamic_ctx) concatenated in original order, joined by \n\n.
        let m = |role: Role, txt: &str| Message {
            role,
            content: MessageContent::Text(txt.into()),
        };
        let mut msgs = vec![
            m(Role::User, "go"),
            m(Role::System, "FIRST"),
            m(Role::System, "SECOND"),
        ];
        normalize_trailing_system(&mut msgs);
        assert_eq!(msgs.len(), 1);
        let MessageContent::Text(t) = &msgs[0].content else {
            panic!("expected Text content")
        };
        assert_eq!(t, "go\n\nFIRST\n\nSECOND");
    }

    #[test]
    fn normalize_trailing_system_noop_without_trailing_system() {
        // No System anywhere — list is unchanged.
        let m = |role: Role, txt: &str| Message {
            role,
            content: MessageContent::Text(txt.into()),
        };
        let original = vec![m(Role::User, "hi"), m(Role::Assistant, "yo")];
        let mut msgs = original.clone();
        normalize_trailing_system(&mut msgs);
        assert_eq!(msgs.len(), 2);
        let MessageContent::Text(last) = &msgs[1].content else {
            panic!()
        };
        assert_eq!(last, "yo");
    }

    #[test]
    fn normalize_trailing_system_folds_into_user_parts() {
        // User content already in Parts form (text + image) — fold
        // System text by appending a new Text part rather than mutating
        // an existing part.
        let mut msgs = vec![
            Message {
                role: Role::User,
                content: MessageContent::Parts(vec![
                    ContentPart::Text { text: "look at this".into() },
                    ContentPart::Image { url: "https://x/y.png".into() },
                ]),
            },
            Message {
                role: Role::System,
                content: MessageContent::Text("CTX".into()),
            },
        ];
        normalize_trailing_system(&mut msgs);
        assert_eq!(msgs.len(), 1);
        let MessageContent::Parts(parts) = &msgs[0].content else {
            panic!("expected Parts content")
        };
        assert_eq!(parts.len(), 3);
        match &parts[2] {
            ContentPart::Text { text } => assert_eq!(text, "CTX"),
            _ => panic!("expected appended Text part"),
        }
    }

    #[test]
    fn normalize_trailing_system_drops_when_preceded_by_tool() {
        // Defensive — runtime doesn't currently inject System after
        // Tool, but if it ever did, we can't fold into a tool_results
        // delta. Drop the System text, leave the Tool tail intact so
        // from_request can build a Tools delta.
        let mut msgs = vec![
            Message {
                role: Role::Tool,
                content: MessageContent::Parts(vec![ContentPart::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "result".into(),
                    is_error: None,
                }]),
            },
            Message {
                role: Role::System,
                content: MessageContent::Text("dynamic ctx".into()),
            },
        ];
        normalize_trailing_system(&mut msgs);
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0].role, Role::Tool));
    }

    #[test]
    fn normalize_trailing_system_skips_empty_system_blocks() {
        // An empty Role::System (text=="") shouldn't add stray "\n\n"
        // separators. Drop empty ones; fold non-empty ones.
        let m = |role: Role, txt: &str| Message {
            role,
            content: MessageContent::Text(txt.into()),
        };
        let mut msgs = vec![
            m(Role::User, "hi"),
            m(Role::System, ""),
            m(Role::System, "non-empty"),
        ];
        normalize_trailing_system(&mut msgs);
        assert_eq!(msgs.len(), 1);
        let MessageContent::Text(t) = &msgs[0].content else {
            panic!()
        };
        assert_eq!(t, "hi\n\nnon-empty");
    }

    #[test]
    fn turn_options_temperature_clamps_to_two_decimals() {
        // Default serde f32 serialization leaks IEEE 754 noise:
        // `0.6_f32` lifts to f64 `0.6000000238418579`, which makes the
        // request body ugly, breaks request hashing for caching layers,
        // and confuses anyone tailing logs. ser_opt_f32 routes through
        // super::json_f32 which rounds to 2 decimals.
        let opts = TurnOptions {
            max_tokens: None,
            temperature: Some(0.6),
            top_p: Some(0.95),
            enable_thinking: None,
            stop: None,
            idle_ttl_secs: None,
        };
        let body = serde_json::to_value(&opts).unwrap();
        // serde_json compares numbers by value not by string repr, so
        // an Eq against json!(0.6) would succeed even with the buggy
        // path. Compare the serialized text instead.
        let s = serde_json::to_string(&opts).unwrap();
        assert!(
            s.contains("\"temperature\":0.6"),
            "expected temperature:0.6, got {s}"
        );
        assert!(!s.contains("0.6000000238418579"), "leaked f32→f64 noise: {s}");
        assert!(
            s.contains("\"top_p\":0.95"),
            "expected top_p:0.95, got {s}"
        );
        // sanity — body is still well-formed JSON.
        assert!(body.is_object());
    }

    #[test]
    fn turn_options_temperature_none_omits_field() {
        let opts = TurnOptions {
            max_tokens: None,
            temperature: None,
            top_p: None,
            enable_thinking: None,
            stop: None,
            idle_ttl_secs: None,
        };
        let body = serde_json::to_value(&opts).unwrap();
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
    }

    #[test]
    fn create_session_resp_parses_replay_shape_without_rsclaw_version() {
        // Protocol §2.2 replay response carries session_id but NOT
        // rsclaw_version. Without #[serde(default)] this fails with
        // "missing field rsclaw_version" and breaks every replay path.
        let body = r#"{
            "session_id": "rs_w7_8a3c1f2b",
            "n_prefix_tokens": 27981,
            "n_user_tokens": 612,
            "n_history_tokens": 8420,
            "n_tokens": 37013,
            "instance_id": "llama-worker-7",
            "replay_ms": 2340
        }"#;
        let resp: CreateSessionResp = serde_json::from_str(body).expect("replay shape parses");
        assert_eq!(resp.session_id, "rs_w7_8a3c1f2b");
        assert!(resp.rsclaw_version.is_empty());
    }

    #[test]
    fn create_session_resp_parses_create_shape_with_rsclaw_version() {
        // Protocol §2.1 create response DOES carry rsclaw_version.
        let body = r#"{
            "session_id": "rs_w7_8a3c1f2b",
            "rsclaw_version": "2026.5.5"
        }"#;
        let resp: CreateSessionResp = serde_json::from_str(body).expect("create shape parses");
        assert_eq!(resp.rsclaw_version, "2026.5.5");
    }

    #[test]
    fn rsclaw_version_or_falls_back_when_empty() {
        let empty = CreateSessionResp {
            session_id: "id".into(),
            rsclaw_version: String::new(),
        };
        assert_eq!(empty.rsclaw_version_or("2026.5.5"), "2026.5.5");
    }

    #[test]
    fn rsclaw_version_or_keeps_response_value_when_present() {
        // If the upstream (e.g. an open() response, or a future replay
        // response that adds the field) DOES include the field, prefer
        // it over the fallback — that's the canonical/registered name
        // and it may differ from the requested alias.
        let canonical = CreateSessionResp {
            session_id: "id".into(),
            rsclaw_version: "2026.5.5".into(),
        };
        assert_eq!(canonical.rsclaw_version_or("alias-name"), "2026.5.5");
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
