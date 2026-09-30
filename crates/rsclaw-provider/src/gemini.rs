//! Google Gemini API provider.
//!
//! Uses the `generateContent` streaming endpoint with API-key authentication.
//! The wire format differs from OpenAI: messages are `contents` with `parts`,
//! and streaming returns JSON lines with `candidates[0].content.parts[0].text`.

use anyhow::{Context, Result};
use futures::{StreamExt, future::BoxFuture};
use reqwest::Client;
use serde_json::{Value, json};

use super::{
    ContentPart, LlmProvider, LlmRequest, LlmStream, Message, MessageContent, Role, StreamEvent,
    TokenUsage,
};

pub const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

pub struct GeminiProvider {
    client: Client,
    api_key: String,
    base_url: String,
    user_agent: Option<String>,
}

impl GeminiProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: super::http_client(),
            api_key: api_key.into(),
            base_url: GEMINI_API_BASE.to_owned(),
            user_agent: None,
        }
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

impl LlmProvider for GeminiProvider {
    fn name(&self) -> &str {
        "gemini"
    }

    fn stream(&self, req: LlmRequest) -> BoxFuture<'_, Result<LlmStream>> {
        Box::pin(async move {
            super::warn_unsupported_kv_cache_mode_2(self.name(), &req);
            let body = build_request_body(&req)?;
            let url = format!(
                "{}/models/{}:streamGenerateContent?alt=sse",
                self.base_url, req.model
            );

            // Bound only time-to-headers; a `RequestBuilder::timeout` would
            // also cover the streamed body and cut long generations. The body
            // is guarded by the per-chunk idle timeout below.
            let send_fut = self
                .client
                .post(&url)
                .header("content-type", "application/json")
                .header("x-goog-api-key", &self.api_key)
                .header(
                    "user-agent",
                    self.user_agent
                        .as_deref()
                        .unwrap_or(super::DEFAULT_USER_AGENT),
                )
                .json(&body)
                .send();
            let resp = tokio::time::timeout(std::time::Duration::from_secs(120), send_fut)
                .await
                .map_err(|_| {
                    anyhow::anyhow!("Gemini request timed out after 120s waiting for response headers")
                })?
                // `without_url` keeps the request URL out of the error text.
                .map_err(reqwest::Error::without_url)
                .context("Gemini request failed")?;

            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Gemini API error {status}: {body}");
            }

            let byte_stream = resp.bytes_stream();
            let byte_stream =
                tokio_stream::StreamExt::timeout(byte_stream, std::time::Duration::from_secs(120));
            let mapped = byte_stream.map(move |r| match r {
                Ok(Ok(bytes)) => Ok(bytes),
                Ok(Err(e)) => Err(anyhow::anyhow!("Gemini stream read error: {}", e.without_url())),
                Err(_) => Err(anyhow::anyhow!(
                    "Gemini stream idle for 120s (server stalled mid-generation)"
                )),
            });
            let event_stream = mapped
                .scan(SseBuffers::default(), |buffers, chunk| {
                    futures::future::ready(Some(parse_sse_chunk_buffered(chunk, buffers)))
                })
                .flat_map(futures::stream::iter);

            let stream: LlmStream = Box::pin(event_stream);
            Ok(stream)
        })
    }
}

// ---------------------------------------------------------------------------
// Request body builder
// ---------------------------------------------------------------------------

fn build_request_body(req: &LlmRequest) -> Result<Value> {
    let mut contents: Vec<Value> = Vec::new();

    for msg in &req.messages {
        if msg.role == Role::System {
            // System messages are handled via systemInstruction below.
            continue;
        }
        contents.push(serialize_message(msg));
    }

    let mut body = json!({
        "contents": contents,
        "generationConfig": {},
    });

    // System instruction: combine explicit system prompt + any system-role
    // messages.
    let mut system_parts: Vec<String> = Vec::new();
    if let Some(sys) = &req.system {
        system_parts.push(sys.clone());
    }
    for msg in &req.messages {
        if msg.role == Role::System
            && let MessageContent::Text(t) = &msg.content
        {
            system_parts.push(t.clone());
        }
    }
    if !system_parts.is_empty() {
        body["systemInstruction"] = json!({
            "parts": [{ "text": system_parts.join("\n\n") }]
        });
    }

    // Generation config.
    let gen_cfg = body["generationConfig"]
        .as_object_mut()
        .expect("generationConfig field constructed above");
    if let Some(max) = req.max_tokens {
        gen_cfg.insert("maxOutputTokens".to_owned(), json!(max));
    }
    if let Some(t) = req.temperature {
        gen_cfg.insert("temperature".to_owned(), super::json_f32(t));
    }

    // Tools. Gemini's function-name regex is
    // `^[a-zA-Z][a-zA-Z0-9_.-]{0,63}$`, which allows `.` — so plugin
    // tools like `wechat.send_text` pass through unchanged. No
    // `sanitize_tool_name` wrapper needed (cf. openai.rs / anthropic.rs
    // which both reject `.` and require the `rc_` wire encoding).
    if !req.tools.is_empty() {
        let functions: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name":        t.name,
                    "description": t.description,
                    "parameters":  t.parameters,
                })
            })
            .collect();
        body["tools"] = json!([{ "functionDeclarations": functions }]);
    }

    Ok(body)
}

fn serialize_message(msg: &Message) -> Value {
    let role = match msg.role {
        Role::User | Role::Tool => "user",
        Role::Assistant => "model",
        Role::System => "user", // fallback, shouldn't reach here
    };

    let parts = match &msg.content {
        MessageContent::Text(t) => vec![json!({ "text": t })],
        MessageContent::Parts(parts) => parts.iter().map(serialize_part).collect(),
    };

    json!({ "role": role, "parts": parts })
}

fn serialize_part(part: &ContentPart) -> Value {
    match part {
        ContentPart::Text { text } => json!({ "text": text }),
        ContentPart::Image { url } => {
            // Parse MIME type from data URI prefix (e.g. "data:image/jpeg;base64,..."),
            // falling back to image/png for raw base64 without a prefix.
            let (mime, data) = if let Some(rest) = url.strip_prefix("data:") {
                if let Some((header, b64)) = rest.split_once(',') {
                    let m = header.split(';').next().unwrap_or("image/png");
                    (m, b64)
                } else {
                    ("image/png", url.as_str())
                }
            } else {
                ("image/png", url.as_str())
            };
            json!({
                "inlineData": {
                    "mimeType": mime,
                    "data": data,
                }
            })
        }
        ContentPart::ToolUse { name, input, .. } => json!({
            "functionCall": {
                "name": name,
                "args": input,
            }
        }),
        ContentPart::ToolResult {
            tool_use_id,
            content,
            ..
        } => json!({
            "functionResponse": {
                "name": tool_use_id,
                "response": { "content": content },
            }
        }),
        ContentPart::Reasoning { text } => json!({
            "text": text,
        }),
    }
}

// ---------------------------------------------------------------------------
// SSE parser (Gemini streaming format)
// ---------------------------------------------------------------------------

/// Per-stream carry-over between chunks: the incomplete trailing line and
/// the incomplete trailing UTF-8 sequence.
#[derive(Default)]
struct SseBuffers {
    line: String,
    utf8_pending: Vec<u8>,
}

/// Buffered SSE parser — handles TCP chunk boundaries that split lines and
/// multi-byte characters.
// TODO: SSE buffered parsing is duplicated across openai.rs, anthropic.rs,
// gemini.rs — extract shared utility
fn parse_sse_chunk_buffered(
    chunk: Result<bytes::Bytes>,
    buffers: &mut SseBuffers,
) -> Vec<Result<StreamEvent>> {
    let bytes = match chunk {
        Ok(b) => b,
        Err(e) => return vec![Err(e)],
    };

    let text = super::decode_utf8_chunk(&mut buffers.utf8_pending, &bytes);
    buffers.line.push_str(&text);

    let last_newline_pos = match buffers.line.rfind('\n') {
        Some(pos) => pos,
        None => return vec![],
    };

    let complete_portion = buffers.line[..last_newline_pos].to_owned();
    buffers.line.drain(..=last_newline_pos);

    let mut events = Vec::new();
    for line in complete_portion.lines() {
        let data = if let Some(d) = line.strip_prefix("data:").map(|s| s.trim_start()) {
            d
        } else {
            continue;
        };
        events.extend(parse_event(data).into_iter().map(Ok));
    }

    events
}

/// Parse one streamed `GenerateContentResponse` into every event it
/// carries. A single chunk can hold several parallel `functionCall` parts,
/// text, and the terminal `finishReason` + `usageMetadata` together.
fn parse_event(data: &str) -> Vec<StreamEvent> {
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return Vec::new();
    };

    // Check for errors.
    if let Some(err) = v.get("error") {
        let msg = err["message"].as_str().unwrap_or("unknown Gemini error");
        return vec![StreamEvent::Error(msg.to_owned())];
    }

    let Some(candidate) = v["candidates"].as_array().and_then(|c| c.first()) else {
        return Vec::new();
    };

    let mut events = Vec::new();
    if let Some(parts) = candidate["content"]["parts"].as_array() {
        for part in parts {
            if let Some(fc) = part.get("functionCall") {
                let name = fc["name"].as_str().unwrap_or("").to_owned();
                let args = fc
                    .get("args")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));
                events.push(StreamEvent::ToolCall {
                    id: name.clone(), // Gemini doesn't use separate IDs
                    name,
                    input: args,
                });
            } else if let Some(text) = part["text"].as_str()
                && !text.is_empty()
            {
                if part["thought"].as_bool() == Some(true) {
                    events.push(StreamEvent::ReasoningDelta(text.to_owned()));
                } else {
                    events.push(StreamEvent::TextDelta(text.to_owned()));
                }
            }
        }
    }

    // Finish reason — last, after any deltas in the same chunk, and carrying
    // the usage that arrives alongside it.
    if candidate.get("finishReason").is_some() {
        let usage = v.get("usageMetadata").map(|u| TokenUsage {
            input: u["promptTokenCount"].as_u64().unwrap_or(0),
            output: u["candidatesTokenCount"].as_u64().unwrap_or(0),
            // Gemini reports cache reads via `cachedContentTokenCount`;
            // no separate creation counter (cache is implicit).
            cache_creation: 0,
            cache_read: u["cachedContentTokenCount"].as_u64().unwrap_or(0),
            ..Default::default()
        });
        events.push(StreamEvent::Done { usage });
    }

    events
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
            fallback_models: Vec::new(),
            model: "gemini-2.0-flash".to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn request_serializes_contents() {
        let req = LlmRequest {
            fallback_models: Vec::new(),
            messages: vec![
                Message {
                    role: Role::User,
                    content: MessageContent::Text("hi".to_owned()),
                    rsclaw_hidden: None,
                },
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Text("hello".to_owned()),
                    rsclaw_hidden: None,
                },
            ],
            ..make_request()
        };
        let body = build_request_body(&req).unwrap();
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0]["role"].as_str().unwrap(), "user");
        assert_eq!(contents[1]["role"].as_str().unwrap(), "model");
    }

    #[test]
    fn system_instruction_present() {
        let req = LlmRequest {
            fallback_models: Vec::new(),
            system: Some("be helpful".to_owned()),
            ..make_request()
        };
        let body = build_request_body(&req).unwrap();
        let sys = &body["systemInstruction"]["parts"][0]["text"];
        assert_eq!(sys.as_str().unwrap(), "be helpful");
    }

    #[test]
    fn temperature_in_generation_config() {
        let req = LlmRequest {
            fallback_models: Vec::new(),
            temperature: Some(0.5),
            ..make_request()
        };
        let body = build_request_body(&req).unwrap();
        let t = body["generationConfig"]["temperature"].as_f64().unwrap();
        assert!((t - 0.5).abs() < 1e-4);
    }

    #[test]
    fn tools_serialize_as_function_declarations() {
        let req = LlmRequest {
            fallback_models: Vec::new(),
            tools: vec![super::super::ToolDef {
                name: "search".to_owned(),
                description: "Search the web".to_owned(),
                parameters: json!({"type": "object"}),
            }],
            ..make_request()
        };
        let body = build_request_body(&req).unwrap();
        let decls = &body["tools"][0]["functionDeclarations"];
        assert_eq!(decls[0]["name"].as_str().unwrap(), "search");
    }

    #[test]
    fn parallel_function_calls_and_usage_in_one_chunk() {
        let data = r#"{"candidates":[{"content":{"parts":[
            {"functionCall":{"name":"a","args":{"x":1}}},
            {"functionCall":{"name":"b","args":{}}}
        ]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5}}"#;
        let events = parse_event(data);
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], StreamEvent::ToolCall { name, .. } if name == "a"));
        assert!(matches!(&events[1], StreamEvent::ToolCall { name, .. } if name == "b"));
        match &events[2] {
            StreamEvent::Done { usage: Some(u) } => {
                assert_eq!(u.input, 10);
                assert_eq!(u.output, 5);
            }
            other => panic!("expected Done with usage, got {other:?}"),
        }
    }
}
