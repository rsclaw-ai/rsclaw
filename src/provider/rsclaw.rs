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

    fn lookup(&self, session_key: &str) -> Option<SessionEntry> {
        self.sessions.lock().ok()?.get(session_key).cloned()
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

            // Lookup or open. If lookup returns a session_id but the
            // server later 404s, we replay with full history and retry.
            let entry = match self.lookup(&session_key) {
                Some(e) if e.rsclaw_version == split.rsclaw_version => e,
                _ => {
                    self.forget(&session_key);
                    let opened = self.open(&split).await?;
                    let entry = SessionEntry {
                        session_id: opened.session_id.clone(),
                        rsclaw_version: opened.rsclaw_version.clone(),
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
                    self.forget(&session_key);
                    let replayed = self.replay(&split, &req.messages).await?;
                    let entry = SessionEntry {
                        session_id: replayed.session_id.clone(),
                        rsclaw_version: replayed.rsclaw_version.clone(),
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
        let mut builder = self.client.post(&url).json(&body);
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
        let mut builder = self.client.post(&url).json(&body);
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
            anyhow::bail!("rsclaw turn failed {status}: {body}");
        }

        let byte_stream = resp.bytes_stream();
        let line_buffer = Arc::new(tokio::sync::Mutex::new(String::new()));
        let event_stream = byte_stream
            .map_err(|e| anyhow::anyhow!("stream read error: {e}"))
            .then(move |chunk| {
                let line_buffer = line_buffer.clone();
                async move { parse_sse_chunk(chunk, &line_buffer).await }
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
    #[allow(dead_code)]
    n_prefix_tokens: u32,
    #[allow(dead_code)]
    n_user_tokens: u32,
    #[allow(dead_code)]
    n_tokens: u32,
    rsclaw_version: String,
    #[allow(dead_code)]
    instance_id: String,
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
        let mut tool_results: Vec<ToolResultDelta> = Vec::new();
        let mut user_text: Option<String> = None;
        match &last.content {
            MessageContent::Text(t) => user_text = Some(t.clone()),
            MessageContent::Parts(parts) => {
                for p in parts {
                    match p {
                        ContentPart::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => tool_results.push(ToolResultDelta {
                            tool_use_id: tool_use_id.clone(),
                            content: content.clone(),
                            is_error: is_error.unwrap_or(false),
                        }),
                        ContentPart::Text { text } => {
                            user_text.get_or_insert_with(String::new).push_str(text);
                        }
                        _ => {}
                    }
                }
            }
        }
        if !tool_results.is_empty() && user_text.is_some() {
            anyhow::bail!(
                "rsclaw: turn delta must be either user_message or tool_results, not both"
            );
        }
        if !tool_results.is_empty() {
            Ok(TurnDelta::Tools { tool_results })
        } else if let Some(t) = user_text {
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
) -> Vec<Result<StreamEvent>> {
    let mut events: Vec<Result<StreamEvent>> = Vec::new();
    let bytes = match chunk {
        Ok(b) => b,
        Err(e) => {
            events.push(Err(e));
            return events;
        }
    };
    let mut buf = line_buffer.lock().await;
    buf.push_str(&String::from_utf8_lossy(&bytes));
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolDef;

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
