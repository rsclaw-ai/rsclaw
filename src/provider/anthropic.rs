//! Anthropic Messages API provider.
//!
//! Implements streaming via `anthropic-version: 2023-06-01` SSE.

use anyhow::{Context, Result};
use futures::{StreamExt, TryStreamExt, future::BoxFuture};
use reqwest::Client;
use serde_json::{Value, json};

use super::{
    ContentPart, LlmProvider, LlmRequest, LlmStream, Message, MessageContent, Role, StreamEvent,
    TokenUsage,
};

pub const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com/v1";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 8192;

pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    base_url: String,
    user_agent: Option<String>,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, ANTHROPIC_API_BASE)
    }

    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            client: super::http_client(),
            api_key: api_key.into(),
            base_url: base_url.into(),
            user_agent: None,
        }
    }

    /// Create a provider with custom User-Agent.
    pub fn with_user_agent(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        user_agent: Option<String>,
    ) -> Self {
        Self {
            client: super::http_client_with_ua(user_agent.as_deref()),
            api_key: api_key.into(),
            base_url: base_url.into(),
            user_agent,
        }
    }
}

impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn stream(&self, req: LlmRequest) -> BoxFuture<'_, Result<LlmStream>> {
        Box::pin(async move {
            let body = build_request_body(&req)?;

            let resp = self
                .client
                .post(format!("{}/messages", self.base_url.trim_end_matches('/')))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json")
                .header(
                    "user-agent",
                    self.user_agent.as_deref().unwrap_or(super::DEFAULT_USER_AGENT),
                )
                .json(&body)
                .timeout(std::time::Duration::from_secs(120))
                .send()
                .await
                .context("Anthropic request failed")?;

            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Anthropic API error {status}: {body}");
            }

            let byte_stream = resp.bytes_stream();
            let line_buffer = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
            let event_stream = byte_stream
                .map_err(|e| anyhow::anyhow!("stream read error: {e}"))
                .then(move |chunk| {
                    let line_buffer = line_buffer.clone();
                    async move { parse_sse_chunk_buffered(chunk, &line_buffer).await }
                })
                .flat_map(|events| futures::stream::iter(events));

            let stream: LlmStream = Box::pin(event_stream);
            Ok(stream)
        })
    }
}

// ---------------------------------------------------------------------------
// Request body builder
// ---------------------------------------------------------------------------

fn build_request_body(req: &LlmRequest) -> Result<Value> {
    // Split system messages from conversation messages.
    let (system, messages) = split_system_messages(&req.messages, req.system.as_deref());

    let mut body = json!({
        "model":      req.model,
        "max_tokens": req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "stream":     true,
        "messages":   messages,
    });

    if let Some(sys) = system {
        body["system"] = json!(sys);
    }

    // Inject prompt caching markers (system_and_3 strategy).
    inject_cache_control(&mut body);

    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }

    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name":         t.name,
                    "description":  t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect();
        body["tools"] = json!(tools);
    }

    // Extended thinking: if budget > 0, enable thinking with the specified budget.
    if let Some(budget) = req.thinking_budget
        && budget > 0
    {
        body["thinking"] = json!({
            "type": "enabled",
            "budget_tokens": budget,
        });
    }

    Ok(body)
}

fn split_system_messages<'a>(
    messages: &'a [Message],
    extra_system: Option<&'a str>,
) -> (Option<String>, Vec<Value>) {
    let mut system_parts: Vec<String> =
        extra_system.map(|s| vec![s.to_owned()]).unwrap_or_default();

    let mut conv: Vec<Value> = Vec::new();

    for msg in messages {
        match msg.role {
            Role::System => {
                if let MessageContent::Text(t) = &msg.content {
                    system_parts.push(t.clone());
                }
            }
            Role::User | Role::Assistant | Role::Tool => {
                conv.push(serialize_message(msg));
            }
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };

    (system, conv)
}

fn serialize_message(msg: &Message) -> Value {
    let role = match msg.role {
        Role::User | Role::Tool => "user",
        Role::Assistant => "assistant",
        Role::System => "user", // fallback, shouldn't happen
    };

    let content = match &msg.content {
        MessageContent::Text(t) => json!(t),
        MessageContent::Parts(parts) => {
            let serialized: Vec<Value> = parts.iter().map(serialize_part).collect();
            json!(serialized)
        }
    };

    json!({ "role": role, "content": content })
}

fn serialize_part(part: &ContentPart) -> Value {
    match part {
        ContentPart::Text { text } => json!({ "type": "text", "text": text }),
        ContentPart::Image { url } => json!({
            "type": "image",
            "source": { "type": "url", "url": url }
        }),
        ContentPart::ToolUse { id, name, input } => json!({
            "type": "tool_use",
            "id":    id,
            "name":  name,
            "input": input,
        }),
        ContentPart::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => json!({
            "type":        "tool_result",
            "tool_use_id": tool_use_id,
            "content":     content,
            "is_error":    is_error.unwrap_or(false),
        }),
    }
}

// ---------------------------------------------------------------------------
// Prompt caching — "system_and_3" strategy
// ---------------------------------------------------------------------------

/// Inject `cache_control: {"type": "ephemeral"}` markers into the request body.
///
/// Strategy (matches hermes-agent "system_and_3"):
/// - 1 breakpoint on the system prompt (stable prefix, high cache-hit rate)
/// - Up to 3 rolling breakpoints on the most recent non-system messages
///
/// The system prompt is converted from a plain string to a content-block array
/// so that the `cache_control` field can be attached to the last block.
fn inject_cache_control(body: &mut Value) {
    let cache_marker = json!({"type": "ephemeral"});

    // -- System prompt breakpoint --
    if let Some(system_val) = body.get_mut("system") {
        match system_val {
            // Plain string -> convert to content-block array with cache_control.
            Value::String(text) => {
                let block = json!([{
                    "type": "text",
                    "text": text.clone(),
                    "cache_control": cache_marker.clone(),
                }]);
                *system_val = block;
            }
            // Already an array of content blocks -> tag the last one.
            Value::Array(blocks) => {
                if let Some(last) = blocks.last_mut() {
                    last["cache_control"] = cache_marker.clone();
                }
            }
            _ => {}
        }
    }

    // -- Message breakpoints (last 3 messages) --
    if let Some(Value::Array(messages)) = body.get_mut("messages") {
        let len = messages.len();
        let start = len.saturating_sub(3);
        for msg in &mut messages[start..] {
            tag_last_content_block(msg, &cache_marker);
        }
    }
}

/// Add `cache_control` to the last content block of a message.
///
/// If the message content is a plain string, convert it to a content-block array
/// so the marker can be attached.
fn tag_last_content_block(msg: &mut Value, marker: &Value) {
    let content = match msg.get_mut("content") {
        Some(c) => c,
        None => return,
    };

    match content {
        // Plain string -> convert to [{type: "text", text: "...", cache_control: ...}]
        Value::String(text) => {
            let block = json!([{
                "type": "text",
                "text": text.clone(),
                "cache_control": marker.clone(),
            }]);
            *content = block;
        }
        // Array of content blocks -> tag the last one.
        Value::Array(blocks) => {
            if let Some(last) = blocks.last_mut() {
                last["cache_control"] = marker.clone();
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// SSE parser
// ---------------------------------------------------------------------------

/// Buffered SSE parser — handles TCP chunk boundaries that split lines.
async fn parse_sse_chunk_buffered(
    chunk: Result<bytes::Bytes>,
    line_buffer: &tokio::sync::Mutex<String>,
) -> Vec<Result<StreamEvent>> {
    let bytes = match chunk {
        Ok(b) => b,
        Err(e) => return vec![Err(e)],
    };

    let text = match std::str::from_utf8(&bytes) {
        Ok(t) => std::borrow::Cow::Borrowed(t),
        Err(e) => {
            tracing::warn!("anthropic: UTF-8 decode error at byte {}, replacing: {}", e.valid_up_to(), e);
            std::borrow::Cow::Owned(String::from_utf8_lossy(&bytes).into_owned())
        }
    };

    let mut buffer = line_buffer.lock().await;
    buffer.push_str(&text);

    let last_newline_pos = match buffer.rfind('\n') {
        Some(pos) => pos,
        None => return vec![],
    };

    let complete_portion = buffer[..last_newline_pos].to_owned();
    let incomplete_portion = buffer[last_newline_pos + 1..].to_owned();
    buffer.clear();
    buffer.push_str(&incomplete_portion);

    let mut events = Vec::new();
    for line in complete_portion.lines() {
        if let Some(data) = line.strip_prefix("data: ").or_else(|| line.strip_prefix("data:")) {
            if data == "[DONE]" {
                continue;
            }
            if let Some(event) = parse_event(data) {
                events.push(Ok(event));
            }
        }
    }

    events
}

fn parse_event(data: &str) -> Option<StreamEvent> {
    let v: Value = serde_json::from_str(data).ok()?;
    let event_type = v["type"].as_str()?;

    match event_type {
        "content_block_delta" => {
            let delta_type = v["delta"]["type"].as_str()?;
            match delta_type {
                "text_delta" => {
                    let text = v["delta"]["text"].as_str()?.to_owned();
                    Some(StreamEvent::TextDelta(text))
                }
                "thinking_delta" => {
                    let text = v["delta"]["thinking"].as_str().unwrap_or("").to_owned();
                    if text.is_empty() { None } else { Some(StreamEvent::ReasoningDelta(text)) }
                }
                "input_json_delta" => {
                    // Tool input streaming — accumulation is handled by the agent loop.
                    None
                }
                _ => None,
            }
        }
        "content_block_start" => {
            let block = &v["content_block"];
            match block["type"].as_str() {
                Some("tool_use") => {
                    // Tool call start — emit immediately so the agent loop knows.
                    Some(StreamEvent::ToolCall {
                        id: block["id"].as_str().unwrap_or("").to_owned(),
                        name: block["name"].as_str().unwrap_or("").to_owned(),
                        input: serde_json::Value::Object(Default::default()),
                    })
                }
                Some("thinking") => {
                    // Thinking block start — no action needed.
                    None
                }
                _ => None,
            }
        }
        "message_delta" => {
            let usage = v["usage"].as_object().map(|u| TokenUsage {
                input: u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
                output: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
            });
            if v["delta"]["stop_reason"].is_string() {
                Some(StreamEvent::Done { usage })
            } else {
                None
            }
        }
        "error" => {
            let msg = v["error"]["message"]
                .as_str()
                .unwrap_or("unknown error")
                .to_owned();
            Some(StreamEvent::Error(msg))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{
        super::{LlmRequest, Message, MessageContent, Role},
        *,
    };

    fn make_request() -> LlmRequest {
        LlmRequest {
            model: "claude-3-5-sonnet-20241022".to_owned(),
            messages: vec![],
            tools: vec![],
            system: None,
            max_tokens: None,
            temperature: None,
            frequency_penalty: None,
            thinking_budget: None,
        }
    }

    #[test]
    fn request_serializes_messages() {
        let req = LlmRequest {
            messages: vec![
                Message {
                    role: Role::User,
                    content: MessageContent::Text("hi".to_owned()),
                },
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Text("hello".to_owned()),
                },
            ],
            ..make_request()
        };
        let body = build_request_body(&req).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"].as_str().unwrap(), "user");
        assert_eq!(msgs[1]["role"].as_str().unwrap(), "assistant");
    }

    #[test]
    fn system_field_present() {
        let req = LlmRequest {
            system: Some("hello".to_owned()),
            ..make_request()
        };
        let body = build_request_body(&req).unwrap();
        // After cache_control injection, system is an array of content blocks.
        let blocks = body["system"].as_array().expect("system should be content-block array");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["text"].as_str().unwrap(), "hello");
        assert_eq!(
            blocks[0]["cache_control"]["type"].as_str().unwrap(),
            "ephemeral"
        );
    }

    #[test]
    fn cache_control_system_and_3() {
        let req = LlmRequest {
            system: Some("system prompt".to_owned()),
            messages: vec![
                Message {
                    role: Role::User,
                    content: MessageContent::Text("m1".to_owned()),
                },
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Text("m2".to_owned()),
                },
                Message {
                    role: Role::User,
                    content: MessageContent::Text("m3".to_owned()),
                },
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Text("m4".to_owned()),
                },
                Message {
                    role: Role::User,
                    content: MessageContent::Text("m5".to_owned()),
                },
            ],
            ..make_request()
        };
        let body = build_request_body(&req).unwrap();

        // System should have cache_control.
        let sys_blocks = body["system"].as_array().expect("system content blocks");
        assert_eq!(
            sys_blocks[0]["cache_control"]["type"].as_str().unwrap(),
            "ephemeral"
        );

        // Last 3 messages (m3, m4, m5) should have cache_control.
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 5);

        // m1, m2: no cache_control
        assert!(msgs[0]["content"].as_str().is_some() || msgs[0]["content"][0].get("cache_control").is_none());
        assert!(msgs[1]["content"].as_str().is_some() || msgs[1]["content"][0].get("cache_control").is_none());

        // m3, m4, m5: have cache_control on last content block
        for i in 2..5 {
            let content = &msgs[i]["content"];
            let block = if content.is_array() {
                content.as_array().unwrap().last().unwrap()
            } else {
                panic!("expected content to be converted to array for cached message");
            };
            assert_eq!(
                block["cache_control"]["type"].as_str().unwrap(),
                "ephemeral",
                "message index {i} should have cache_control"
            );
        }
    }

    #[test]
    fn cache_control_fewer_than_3_messages() {
        let req = LlmRequest {
            messages: vec![
                Message {
                    role: Role::User,
                    content: MessageContent::Text("only one".to_owned()),
                },
            ],
            ..make_request()
        };
        let body = build_request_body(&req).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        // Single message should still get cache_control.
        let content = &msgs[0]["content"];
        assert!(content.is_array());
        assert_eq!(
            content[0]["cache_control"]["type"].as_str().unwrap(),
            "ephemeral"
        );
    }

    #[test]
    fn temperature_serializes() {
        let req = LlmRequest {
            temperature: Some(0.7),
            ..make_request()
        };
        let body = build_request_body(&req).unwrap();
        let t = body["temperature"].as_f64().unwrap();
        assert!((t - 0.7).abs() < 1e-4);
    }
}
