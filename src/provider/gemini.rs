//! Google Gemini API provider.
//!
//! Uses the `generateContent` streaming endpoint with API-key authentication.
//! The wire format differs from OpenAI: messages are `contents` with `parts`,
//! and streaming returns JSON lines with `candidates[0].content.parts[0].text`.

use anyhow::{Context, Result};
use futures::{StreamExt, TryStreamExt, future::BoxFuture};
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
            let body = build_request_body(&req)?;
            let url = format!(
                "{}/models/{}:streamGenerateContent?alt=sse&key={}",
                self.base_url, req.model, self.api_key
            );

            let resp = self
                .client
                .post(&url)
                .header("content-type", "application/json")
                .header(
                    "user-agent",
                    self.user_agent.as_deref().unwrap_or(super::DEFAULT_USER_AGENT),
                )
                .json(&body)
                .send()
                .await
                .context("Gemini request failed")?;

            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Gemini API error {status}: {body}");
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
    let gen_cfg = body["generationConfig"].as_object_mut().unwrap();
    if let Some(max) = req.max_tokens {
        gen_cfg.insert("maxOutputTokens".to_owned(), json!(max));
    }
    if let Some(t) = req.temperature {
        gen_cfg.insert("temperature".to_owned(), json!(t));
    }

    // Tools.
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
        ContentPart::Image { url } => json!({
            "inlineData": {
                "mimeType": "image/png",
                "data": url,
            }
        }),
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
    }
}

// ---------------------------------------------------------------------------
// SSE parser (Gemini streaming format)
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
            tracing::warn!("gemini: UTF-8 decode error at byte {}, replacing: {}", e.valid_up_to(), e);
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
        let data = if let Some(d) = line.strip_prefix("data: ") {
            d
        } else {
            continue;
        };
        if let Some(event) = parse_event(data) {
            events.push(Ok(event));
        }
    }

    events
}

fn parse_event(data: &str) -> Option<StreamEvent> {
    let v: Value = serde_json::from_str(data).ok()?;

    // Check for errors.
    if let Some(err) = v.get("error") {
        let msg = err["message"].as_str().unwrap_or("unknown Gemini error");
        return Some(StreamEvent::Error(msg.to_owned()));
    }

    let candidates = v["candidates"].as_array()?;
    let candidate = candidates.first()?;

    // Check for function calls.
    if let Some(parts) = candidate["content"]["parts"].as_array() {
        for part in parts {
            if let Some(fc) = part.get("functionCall") {
                let name = fc["name"].as_str().unwrap_or("").to_owned();
                let args = fc
                    .get("args")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));
                return Some(StreamEvent::ToolCall {
                    id: name.clone(), // Gemini doesn't use separate IDs
                    name,
                    input: args,
                });
            }
        }
    }

    // Text delta.
    if let Some(text) = candidate["content"]["parts"]
        .as_array()
        .and_then(|parts| parts.first())
        .and_then(|part| part["text"].as_str())
        && !text.is_empty()
    {
        return Some(StreamEvent::TextDelta(text.to_owned()));
    }

    // Finish reason.
    if candidate.get("finishReason").is_some() {
        let usage = v.get("usageMetadata").map(|u| TokenUsage {
            input: u["promptTokenCount"].as_u64().unwrap_or(0) as u32,
            output: u["candidatesTokenCount"].as_u64().unwrap_or(0) as u32,
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
            model: "gemini-2.0-flash".to_owned(),
            messages: vec![],
            tools: vec![],
            system: None,
            max_tokens: None,
            temperature: None,
            frequency_penalty: None,
            thinking_budget: None,
            kv_cache_mode: 0,
            session_key: None,
        }
    }

    #[test]
    fn request_serializes_contents() {
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
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0]["role"].as_str().unwrap(), "user");
        assert_eq!(contents[1]["role"].as_str().unwrap(), "model");
    }

    #[test]
    fn system_instruction_present() {
        let req = LlmRequest {
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
}
