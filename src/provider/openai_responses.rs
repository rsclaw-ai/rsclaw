//! OpenAI Responses API provider.
//!
//! Implements the newer OpenAI Responses format used by Doubao/ARK and other
//! providers.  The wire format uses `input` instead of `messages`, typed SSE
//! events (`response.output_text.delta`, etc.), and a different tool-call
//! structure compared to the Chat Completions API.

use anyhow::{Context, Result};
use futures::{StreamExt, TryStreamExt, future::BoxFuture};
use serde_json::{Value, json};

use super::{
    ContentPart, LlmProvider, LlmRequest, LlmStream, Message, MessageContent, Role, StreamEvent,
    TokenUsage,
};

const DEFAULT_MAX_TOKENS: u32 = 65536;

pub struct OpenAiResponsesProvider {
    client: reqwest::Client,
    api_key: Option<String>,
    base_url: String,
}

impl OpenAiResponsesProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: super::http_client(),
            api_key: Some(api_key.into()),
            base_url: super::openai::OPENAI_API_BASE.to_owned(),
        }
    }

    /// Create a provider with custom base URL (for Doubao/ARK etc.).
    pub fn with_base_url(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            client: super::http_client(),
            api_key,
            base_url: base_url.into(),
        }
    }
}

impl LlmProvider for OpenAiResponsesProvider {
    fn name(&self) -> &str {
        "openai-responses"
    }

    fn stream(&self, req: LlmRequest) -> BoxFuture<'_, Result<LlmStream>> {
        Box::pin(async move {
            let body = build_request_body(&req)?;
            let body_str = serde_json::to_string(&body).unwrap_or_default();
            tracing::debug!(
                model = %req.model,
                tools_count = req.tools.len(),
                body_len = body_str.len(),
                "openai-responses: request prepared"
            );
            let _ = std::fs::write(
                std::env::temp_dir().join("rsclaw_last_responses_request.json"),
                &body_str,
            );

            let url = if self.base_url.ends_with("/v1") || self.base_url.contains("/v1/") {
                format!("{}/responses", self.base_url.trim_end_matches('/'))
            } else {
                format!(
                    "{}/v1/responses",
                    self.base_url.trim_end_matches('/')
                )
            };

            let mut builder = self
                .client
                .post(&url)
                .header("content-type", "application/json");

            if let Some(key) = &self.api_key {
                builder = builder.header("authorization", format!("Bearer {key}"));
            }

            let resp = builder
                .json(&body)
                .send()
                .await
                .context("OpenAI Responses request failed")?;

            let status = resp.status();
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            tracing::debug!(
                %status,
                content_type = %content_type,
                "openai-responses: response received"
            );

            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("OpenAI Responses API error {status}: {body}");
            }

            // Non-streaming JSON fallback
            if !content_type.contains("text/event-stream") && content_type.contains("json") {
                let body: Value =
                    resp.json().await.context("OpenAI Responses: parse JSON response")?;
                tracing::debug!(
                    body = %body.to_string().chars().take(300).collect::<String>(),
                    "openai-responses: non-streaming JSON response"
                );
                // Extract text from output array
                let text = body
                    .pointer("/output/0/content/0/text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !text.is_empty() {
                    let stream = futures::stream::iter(vec![
                        Ok(StreamEvent::TextDelta(text.to_owned())),
                        Ok(StreamEvent::Done { usage: None }),
                    ]);
                    return Ok(Box::pin(stream) as LlmStream);
                }
                anyhow::bail!(
                    "OpenAI Responses: empty non-streaming response: {}",
                    body.to_string().chars().take(500).collect::<String>()
                );
            }

            let byte_stream = resp.bytes_stream();
            let event_stream = byte_stream
                .map_err(|e| anyhow::anyhow!("stream read error: {e}"))
                .flat_map(|chunk| futures::stream::iter(parse_sse_chunk(chunk)));

            let stream: LlmStream = Box::pin(event_stream);
            Ok(stream)
        })
    }
}

// ---------------------------------------------------------------------------
// Request body builder
// ---------------------------------------------------------------------------

fn build_request_body(req: &LlmRequest) -> Result<Value> {
    let input: Vec<Value> = req
        .messages
        .iter()
        .filter(|m| m.role != Role::System)
        .map(serialize_input_message)
        .collect();

    let mut body = json!({
        "model":  req.model,
        "stream": true,
        "input":  input,
    });

    // System instructions go in a top-level `instructions` field.
    let mut system_parts: Vec<String> = Vec::new();
    if let Some(ref sys) = req.system {
        system_parts.push(sys.clone());
    }
    for msg in &req.messages {
        if msg.role == Role::System {
            if let MessageContent::Text(t) = &msg.content {
                system_parts.push(t.clone());
            }
        }
    }
    if !system_parts.is_empty() {
        body["instructions"] = json!(system_parts.join("\n\n"));
    }

    body["max_output_tokens"] = json!(req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS));

    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }

    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                })
            })
            .collect();
        body["tools"] = json!(tools);
    }

    Ok(body)
}

fn serialize_input_message(msg: &Message) -> Value {
    let role_str = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::System => "user", // shouldn't happen — filtered out
    };

    // Tool role messages: wrap as function_call_output
    if msg.role == Role::Tool {
        if let MessageContent::Parts(parts) = &msg.content {
            for part in parts {
                if let ContentPart::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } = part
                {
                    return json!({
                        "type": "function_call_output",
                        "call_id": tool_use_id,
                        "output": content,
                    });
                }
            }
        }
        let text = match &msg.content {
            MessageContent::Text(t) => t.clone(),
            MessageContent::Parts(_) => String::new(),
        };
        return json!({
            "type": "function_call_output",
            "call_id": "",
            "output": text,
        });
    }

    // Assistant messages: extract function_call items
    if msg.role == Role::Assistant {
        if let MessageContent::Parts(parts) = &msg.content {
            let mut items: Vec<Value> = Vec::new();
            let mut text_parts = Vec::new();
            for part in parts {
                match part {
                    ContentPart::ToolUse { id, name, input } => {
                        items.push(json!({
                            "type": "function_call",
                            "id": id,
                            "name": name,
                            "arguments": input.to_string(),
                        }));
                    }
                    ContentPart::Text { text } => text_parts.push(text.clone()),
                    _ => {}
                }
            }
            if !items.is_empty() {
                // Include text content as a message item too
                if !text_parts.is_empty() {
                    items.insert(
                        0,
                        json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{ "type": "output_text", "text": text_parts.join("") }],
                        }),
                    );
                }
                // Return items array directly — Responses API uses flat item list
                return json!({
                    "role": "assistant",
                    "content": items,
                });
            }
        }
    }

    // Default: role + content parts
    let content = match &msg.content {
        MessageContent::Text(t) => json!([{ "type": "input_text", "text": t }]),
        MessageContent::Parts(parts) => {
            let serialized: Vec<Value> = parts.iter().map(serialize_input_part).collect();
            json!(serialized)
        }
    };

    json!({ "role": role_str, "content": content })
}

fn serialize_input_part(part: &ContentPart) -> Value {
    match part {
        ContentPart::Text { text } => json!({ "type": "input_text", "text": text }),
        ContentPart::Image { url } => json!({
            "type": "input_image",
            "image_url": url,
        }),
        ContentPart::ToolUse { id, name, input } => json!({
            "type": "function_call",
            "id":   id,
            "name": name,
            "arguments": input.to_string(),
        }),
        ContentPart::ToolResult {
            tool_use_id,
            content,
            ..
        } => json!({
            "type": "function_call_output",
            "call_id": tool_use_id,
            "output": content,
        }),
    }
}

// ---------------------------------------------------------------------------
// SSE parser (OpenAI Responses API format)
// ---------------------------------------------------------------------------

fn parse_sse_chunk(chunk: Result<bytes::Bytes>) -> Vec<Result<StreamEvent>> {
    let bytes = match chunk {
        Ok(b) => b,
        Err(e) => return vec![Err(e)],
    };

    let text = match std::str::from_utf8(&bytes) {
        Ok(t) => t,
        Err(e) => return vec![Err(anyhow::anyhow!("UTF-8 error: {e}"))],
    };

    let mut events = Vec::new();
    // Track the current event type from `event:` lines
    let mut current_event_type: Option<String> = None;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            // Blank line resets event type for next event
            current_event_type = None;
            continue;
        }
        if let Some(event_type) = line.strip_prefix("event: ") {
            current_event_type = Some(event_type.trim().to_owned());
            continue;
        }
        if let Some(data) = line.strip_prefix("data: ") {
            if data == "[DONE]" {
                events.push(Ok(StreamEvent::Done { usage: None }));
                continue;
            }
            if let Some(event) =
                parse_responses_event(data, current_event_type.as_deref())
            {
                events.push(Ok(event));
            } else {
                tracing::debug!(data, "openai-responses: unparsed SSE data");
            }
        }
    }

    events
}

fn parse_responses_event(data: &str, event_type: Option<&str>) -> Option<StreamEvent> {
    let v: Value = serde_json::from_str(data).ok()?;

    // Check for error
    if let Some(err) = v.get("error") {
        let msg = err["message"]
            .as_str()
            .unwrap_or("unknown API error")
            .to_owned();
        return Some(StreamEvent::Error(msg));
    }

    // Use the `event:` line type if available, otherwise check `type` field in JSON
    let evt_type = event_type
        .or_else(|| v["type"].as_str())
        .unwrap_or("");

    match evt_type {
        // Text delta
        "response.output_text.delta" => {
            let delta = v["delta"].as_str().unwrap_or("").to_owned();
            if delta.is_empty() {
                None
            } else {
                Some(StreamEvent::TextDelta(delta))
            }
        }

        // Output item done — may contain function_call
        "response.output_item.done" => {
            let item = &v["item"];
            if item["type"].as_str() == Some("function_call") {
                let id = item["id"]
                    .as_str()
                    .or_else(|| item["call_id"].as_str())
                    .unwrap_or("")
                    .to_owned();
                let name = item["name"].as_str().unwrap_or("").to_owned();
                let args_str = item["arguments"].as_str().unwrap_or("{}");
                let input = serde_json::from_str(args_str)
                    .unwrap_or_else(|_| Value::String(args_str.to_owned()));
                Some(StreamEvent::ToolCall { id, name, input })
            } else {
                None
            }
        }

        // Stream completed — extract usage
        "response.completed" | "response.done" => {
            let usage = v
                .pointer("/response/usage")
                .or_else(|| v.get("usage"))
                .and_then(|u| u.as_object())
                .map(|u| TokenUsage {
                    input: u
                        .get("input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u32,
                    output: u
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u32,
                });
            Some(StreamEvent::Done { usage })
        }

        // Ignore other Responses API events (content_part.added, etc.)
        _ if evt_type.starts_with("response.") => None,

        // ---------- Fallback: Chat Completions format ----------
        // Some providers claim Responses API but return Chat Completions SSE.
        _ => parse_completions_fallback(&v),
    }
}

/// Fallback parser for providers that return Chat Completions format
/// (`choices[0].delta.content`).
fn parse_completions_fallback(v: &Value) -> Option<StreamEvent> {
    let choices = v["choices"].as_array()?;
    let choice = choices.first()?;
    let delta = &choice["delta"];

    // Tool call
    if let Some(tool_calls) = delta["tool_calls"].as_array()
        && let Some(tc) = tool_calls.first()
    {
        let func = &tc["function"];
        let id = tc["id"].as_str().unwrap_or("").to_owned();
        let name = func["name"].as_str().unwrap_or("").to_owned();
        let args_str = func["arguments"].as_str().unwrap_or("");
        let input = if args_str.is_empty() {
            Value::Object(Default::default())
        } else {
            serde_json::from_str(args_str).unwrap_or_else(|_| Value::String(args_str.to_owned()))
        };
        return Some(StreamEvent::ToolCall { id, name, input });
    }

    // Text delta
    if let Some(text) = delta["content"].as_str()
        && !text.is_empty()
    {
        return Some(StreamEvent::TextDelta(text.to_owned()));
    }

    // Finish
    if choice["finish_reason"].is_string() {
        let usage = v["usage"].as_object().map(|u| TokenUsage {
            input: u
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
            output: u
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
        });
        return Some(StreamEvent::Done { usage });
    }

    None
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
            model: "doubao-seed-2-0-pro-260215".to_owned(),
            messages: vec![],
            tools: vec![],
            system: None,
            max_tokens: None,
            temperature: None,
            thinking_budget: None,
        }
    }

    #[test]
    fn request_uses_input_not_messages() {
        let req = LlmRequest {
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hello".to_owned()),
            }],
            ..make_request()
        };
        let body = build_request_body(&req).unwrap();
        assert!(body.get("input").is_some(), "should have 'input' field");
        assert!(body.get("messages").is_none(), "should NOT have 'messages' field");
    }

    #[test]
    fn system_goes_to_instructions() {
        let req = LlmRequest {
            system: Some("be helpful".to_owned()),
            ..make_request()
        };
        let body = build_request_body(&req).unwrap();
        assert_eq!(body["instructions"].as_str().unwrap(), "be helpful");
    }

    #[test]
    fn content_parts_use_input_text() {
        let req = LlmRequest {
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hello".to_owned()),
            }],
            ..make_request()
        };
        let body = build_request_body(&req).unwrap();
        let input = body["input"].as_array().unwrap();
        let part_type = input[0]["content"][0]["type"].as_str().unwrap();
        assert_eq!(part_type, "input_text");
    }

    #[test]
    fn image_uses_input_image() {
        let req = LlmRequest {
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Parts(vec![ContentPart::Image {
                    url: "https://example.com/img.png".to_owned(),
                }]),
            }],
            ..make_request()
        };
        let body = build_request_body(&req).unwrap();
        let input = body["input"].as_array().unwrap();
        let part = &input[0]["content"][0];
        assert_eq!(part["type"].as_str().unwrap(), "input_image");
        assert_eq!(
            part["image_url"].as_str().unwrap(),
            "https://example.com/img.png"
        );
    }

    #[test]
    fn parse_text_delta_event() {
        let data = r#"{"type":"response.output_text.delta","delta":"hello"}"#;
        let event = parse_responses_event(data, Some("response.output_text.delta"));
        assert!(matches!(event, Some(StreamEvent::TextDelta(ref t)) if t == "hello"));
    }

    #[test]
    fn parse_done_event() {
        let data = r#"{"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":20}}}"#;
        let event = parse_responses_event(data, Some("response.completed"));
        match event {
            Some(StreamEvent::Done { usage: Some(u) }) => {
                assert_eq!(u.input, 10);
                assert_eq!(u.output, 20);
            }
            other => panic!("expected Done with usage, got {other:?}"),
        }
    }

    #[test]
    fn parse_tool_call_event() {
        let data = r#"{"type":"response.output_item.done","item":{"type":"function_call","id":"call_123","name":"read_file","arguments":"{\"path\":\"/tmp/x\"}"}}"#;
        let event = parse_responses_event(data, Some("response.output_item.done"));
        match event {
            Some(StreamEvent::ToolCall { id, name, input }) => {
                assert_eq!(id, "call_123");
                assert_eq!(name, "read_file");
                assert_eq!(input["path"].as_str().unwrap(), "/tmp/x");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn parse_completions_fallback() {
        // Providers that return Chat Completions format should still work
        let data = r#"{"choices":[{"delta":{"content":"world"},"finish_reason":null}]}"#;
        let event = parse_responses_event(data, None);
        assert!(matches!(event, Some(StreamEvent::TextDelta(ref t)) if t == "world"));
    }

    #[test]
    fn parse_sse_chunk_with_event_lines() {
        let raw = b"event: response.output_text.delta\ndata: {\"delta\":\"hi\"}\n\nevent: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n";
        let events = parse_sse_chunk(Ok(bytes::Bytes::from_static(raw)));
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], Ok(StreamEvent::TextDelta(t)) if t == "hi"));
        assert!(matches!(&events[1], Ok(StreamEvent::Done { .. })));
    }
}
