//! OpenAI-compatible provider (openai-responses / openai-completions / Ollama).
//!
//! Supports:
//!   - OpenAI Chat Completions API (`openai-completions`, default)
//!   - OpenAI Responses API (`openai-responses`) — newer streaming format
//!   - Ollama (same completions wire format, custom base_url)

use anyhow::{Context, Result};
use futures::{StreamExt, TryStreamExt, future::BoxFuture};
use serde_json::{Value, json};

use super::{
    ContentPart, LlmProvider, LlmRequest, LlmStream, Message, MessageContent, Role, StreamEvent,
    TokenUsage,
};

pub(crate) const OPENAI_API_BASE: &str = "https://api.openai.com/v1";
const DEFAULT_MAX_TOKENS: u32 = 65536;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OpenAiMode {
    Chat,       // /chat/completions (default)
    Responses,  // /responses (newer format)
}

pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: Option<String>,
    base_url: String,
    /// When true, reasoning models use ollama native /api/chat with think=true.
    is_ollama: bool,
    /// Custom User-Agent header.
    user_agent: Option<String>,
    /// API mode: Chat Completions or Responses.
    mode: OpenAiMode,
}

impl OpenAiProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: super::http_client(),
            api_key: Some(api_key.into()),
            base_url: OPENAI_API_BASE.to_owned(),
            is_ollama: false,
            user_agent: None,
            mode: OpenAiMode::Chat,
        }
    }

    /// Create a provider with custom base URL.
    pub fn with_base_url(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            client: super::http_client(),
            api_key,
            base_url: base_url.into(),
            is_ollama: false,
            user_agent: None,
            mode: OpenAiMode::Chat,
        }
    }

    /// Create a Chat Completions provider (default).
    pub fn chat(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            client: super::http_client(),
            api_key,
            base_url: base_url.into(),
            is_ollama: false,
            user_agent: None,
            mode: OpenAiMode::Chat,
        }
    }

    /// Create a Responses API provider.
    pub fn responses(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            client: super::http_client(),
            api_key,
            base_url: base_url.into(),
            is_ollama: false,
            user_agent: None,
            mode: OpenAiMode::Responses,
        }
    }

    /// Create an ollama-backed provider. Reasoning models will use
    /// native /api/chat with think=true for proper content output.
    pub fn ollama(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            client: super::http_client(),
            api_key,
            base_url: base_url.into(),
            is_ollama: true,
            user_agent: None,
            mode: OpenAiMode::Chat,
        }
    }

    /// Create a provider with custom User-Agent.
    pub fn with_user_agent(
        base_url: impl Into<String>,
        api_key: Option<String>,
        user_agent: Option<String>,
    ) -> Self {
        Self {
            client: super::http_client(),
            api_key,
            base_url: base_url.into(),
            is_ollama: false,
            user_agent,
            mode: OpenAiMode::Chat,
        }
    }
}

impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &str {
        match self.mode {
            OpenAiMode::Chat => "openai",
            OpenAiMode::Responses => "openai-responses",
        }
    }

    fn stream(&self, req: LlmRequest) -> BoxFuture<'_, Result<LlmStream>> {
        Box::pin(async move {
            // Ollama + reasoning model -> use native /api/chat with think=false
            // (thinking disabled by default, TODO: make configurable per agent)
            if self.is_ollama {
                let model_lower = req.model.to_lowercase();
                if model_lower.contains("qwen3")
                    || model_lower.contains("qwq")
                    || model_lower.contains("deepseek-r1")
                {
                    return self.stream_ollama_native(&req).await;
                }
            }

            if self.mode == OpenAiMode::Responses {
                return self.stream_responses(&req).await;
            }

            let body = build_request_body(&req)?;
            let body_str = serde_json::to_string(&body).unwrap_or_default();
            tracing::debug!(
                model = %req.model,
                tools_count = req.tools.len(),
                has_tools_in_body = body.get("tools").is_some(),
                body_len = body_str.len(),
                "openai: request prepared"
            );
            // Dump full body to temp file for debugging
            let _ = std::fs::write(
                std::env::temp_dir().join("rsclaw_last_request.json"),
                &body_str,
            );

            let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
            let mut builder = self
                .client
                .post(url)
                .header("content-type", "application/json");

            if let Some(key) = &self.api_key {
                builder = builder.header("authorization", format!("Bearer {key}"));
            }

            let resp = builder
                .json(&body)
                .send()
                .await
                .context("OpenAI request failed")?;

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
                "openai: response received"
            );

            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("OpenAI API error {status}: {body}");
            }

            // If the response is not SSE (e.g. plain JSON), handle it directly.
            if !content_type.contains("text/event-stream") && content_type.contains("json") {
                let body: serde_json::Value =
                    resp.json().await.context("OpenAI: parse JSON response")?;
                tracing::debug!(body = %body.to_string().chars().take(300).collect::<String>(), "openai: non-streaming JSON response");
                // Extract text from non-streaming response
                let text = body
                    .pointer("/choices/0/message/content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !text.is_empty() {
                    let stream = futures::stream::iter(vec![
                        Ok(StreamEvent::TextDelta(text.to_owned())),
                        Ok(StreamEvent::Done { usage: None }),
                    ]);
                    let llm_stream: LlmStream = Box::pin(stream);
                    return Ok(llm_stream);
                }
                anyhow::bail!(
                    "OpenAI: empty non-streaming response: {}",
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

impl OpenAiProvider {
    /// Use ollama native /api/chat for reasoning models with think=true.
    /// This gives properly formatted content with newlines.
    async fn stream_ollama_native(&self, req: &LlmRequest) -> Result<LlmStream> {
        // Build ollama native API URL: strip /v1 suffix
        let base = self.base_url.trim_end_matches('/').trim_end_matches("/v1");
        let url = format!("{base}/api/chat");

        // Build messages in ollama format (same as OpenAI format)
        let mut messages: Vec<Value> = Vec::new();
        if let Some(ref sys) = req.system {
            messages.push(json!({"role": "system", "content": sys}));
        }
        for msg in &req.messages {
            let mut m = serialize_message(msg);
            // ollama native API requires arguments as JSON object, not string.
            if let Some(tcs) = m.get_mut("tool_calls").and_then(|v| v.as_array_mut()) {
                for tc in tcs {
                    if let Some(args) = tc.pointer_mut("/function/arguments") {
                        if let Some(s) = args.as_str() {
                            if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                                *args = parsed;
                            }
                        }
                    }
                }
            }
            // ollama native API requires content as string, not array.
            // Extract images into separate "images" field.
            if let Some(content) = m.get("content") {
                if content.is_array() {
                    let parts = content.as_array().unwrap();
                    let mut texts = Vec::new();
                    let mut images = Vec::new();
                    for p in parts {
                        if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                            texts.push(t.to_owned());
                        } else if let Some(url) =
                            p.pointer("/image_url/url").and_then(|v| v.as_str())
                        {
                            // Strip data URI prefix to get raw base64
                            let b64 = url.split(",").last().unwrap_or(url);
                            images.push(json!(b64));
                        }
                    }
                    m["content"] = json!(texts.join("\n"));
                    if !images.is_empty() {
                        m["images"] = json!(images);
                    }
                }
            }
            messages.push(m);
        }

        // Build tools if any
        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "stream": true,
            // Thinking disabled by default. TODO: make configurable per agent.
            "think": false,
        });

        if let Some(t) = req.temperature {
            body["options"] = json!({"temperature": t});
        }

        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = json!(tools);
        }

        let _ = std::fs::write(
            std::env::temp_dir().join("rsclaw_ollama_request.json"),
            serde_json::to_string_pretty(&body).unwrap_or_default(),
        );
        tracing::debug!(
            tools_count = req.tools.len(),
            think = req.tools.is_empty(),
            "ollama native: calling {url}"
        );

        let resp = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("ollama native request failed")?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("ollama native API error: {body}");
        }

        // Ollama native streaming: JSONL (one JSON object per line)
        let byte_stream = resp.bytes_stream();
        // Track whether we are inside a thinking block so we can emit
        // <think> / </think> boundary tags exactly once.
        let in_thinking = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let event_stream = byte_stream
            .map_err(|e| anyhow::anyhow!("stream read error: {e}"))
            .flat_map(move |chunk| {
                let in_thinking = std::sync::Arc::clone(&in_thinking);
                let events: Vec<Result<StreamEvent>> = match chunk {
                    Ok(bytes) => {
                        let text = String::from_utf8_lossy(&bytes);
                        text.lines()
                            .filter_map(|line| {
                                let line = line.trim();
                                if line.is_empty() {
                                    return None;
                                }
                                let v: Value = serde_json::from_str(line).ok()?;

                                // Check for tool calls
                                if let Some(tc) = v
                                    .get("message")
                                    .and_then(|m| m.get("tool_calls"))
                                    .and_then(|tc| tc.as_array())
                                    .and_then(|a| a.first())
                                {
                                    let func = &tc["function"];
                                    let name = func["name"].as_str().unwrap_or("").to_owned();
                                    // ollama native: arguments is a JSON object (not string)
                                    let input = if func["arguments"].is_object() {
                                        func["arguments"].clone()
                                    } else {
                                        let args_str = func["arguments"].as_str().unwrap_or("{}");
                                        serde_json::from_str(args_str).unwrap_or(json!({}))
                                    };
                                    return Some(Ok(StreamEvent::ToolCall {
                                        id: format!("call_{}", name),
                                        name,
                                        input,
                                    }));
                                }

                                // Thinking content (think=true mode)
                                let thinking = v
                                    .pointer("/message/thinking")
                                    .and_then(|c| c.as_str())
                                    .unwrap_or("");

                                // Text content
                                let content = v
                                    .pointer("/message/content")
                                    .and_then(|c| c.as_str())
                                    .unwrap_or("");

                                let done = v["done"].as_bool().unwrap_or(false);
                                if done {
                                    // Close thinking block if still open
                                    if in_thinking.swap(false, std::sync::atomic::Ordering::Relaxed) {
                                        return Some(Ok(StreamEvent::TextDelta("</think>".to_owned())));
                                    }
                                    return Some(Ok(StreamEvent::Done { usage: None }));
                                }

                                // Emit thinking content with boundary tags
                                if !thinking.is_empty() {
                                    let was_thinking = in_thinking.swap(true, std::sync::atomic::Ordering::Relaxed);
                                    if !was_thinking {
                                        // First thinking chunk: prepend <think>
                                        return Some(Ok(StreamEvent::TextDelta(format!("<think>{thinking}"))));
                                    }
                                    return Some(Ok(StreamEvent::TextDelta(thinking.to_owned())));
                                }

                                if !content.is_empty() {
                                    let was_thinking = in_thinking.swap(false, std::sync::atomic::Ordering::Relaxed);
                                    if was_thinking {
                                        // Transition from thinking to content: close tag
                                        return Some(Ok(StreamEvent::TextDelta(format!("</think>{content}"))));
                                    }
                                    Some(Ok(StreamEvent::TextDelta(content.to_owned())))
                                } else {
                                    None
                                }
                            })
                            .collect()
                    }
                    Err(e) => vec![Err(e)],
                };
                futures::stream::iter(events)
            });

        Ok(Box::pin(event_stream))
    }

    /// Stream using the OpenAI Responses API format.
    async fn stream_responses(&self, req: &LlmRequest) -> Result<LlmStream> {
        let body = build_responses_body(req)?;
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

        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));

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
            .flat_map(|chunk| futures::stream::iter(parse_responses_sse_chunk(chunk)));

        let stream: LlmStream = Box::pin(event_stream);
        Ok(stream)
    }
}

// ---------------------------------------------------------------------------
// Request body builder
// ---------------------------------------------------------------------------

fn build_request_body(req: &LlmRequest) -> Result<Value> {
    let messages: Vec<Value> = req.messages.iter().map(serialize_message).collect();

    let mut body = json!({
        "model":      req.model,
        "stream":     true,
        "messages":   messages,
    });
    if let Some(max_tokens) = req.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }

    // Thinking/reasoning mode: configurable via agents.defaults.thinking or per-agent thinking.
    // Default: disabled. When enabled with budget > 0, model uses reasoning_content field.
    match req.thinking_budget {
        Some(budget) if budget > 0 => {
            body["enable_thinking"] = json!(true);
            body["thinking_budget"] = json!(budget);
        }
        _ => {
            // Disable by default for models that auto-enable thinking (Qwen3, DeepSeek-R1).
            if req.model.to_lowercase().starts_with("qwen")
                || req.model.to_lowercase().contains("deepseek-r1")
            {
                body["enable_thinking"] = json!(false);
            }
        }
    }

    if let Some(sys) = &req.system {
        // Prepend a system message if not already present.
        if messages
            .first()
            .and_then(|m| m["role"].as_str())
            .is_none_or(|r| r != "system")
        {
            let mut msgs = vec![json!({"role": "system", "content": sys})];
            msgs.extend(body["messages"].as_array().cloned().unwrap_or_default());
            body["messages"] = json!(msgs);
        }
    }

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
                    "function": {
                        "name":        t.name,
                        "description": t.description,
                        "parameters":  t.parameters,
                    }
                })
            })
            .collect();
        body["tools"] = json!(tools);
    }

    Ok(body)
}

fn serialize_message(msg: &Message) -> Value {
    let role_str = match msg.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };

    // Tool role messages need special handling
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
                        "role": "tool",
                        "tool_call_id": tool_use_id,
                        "content": content,
                    });
                }
            }
        }
        // Fallback for text-only tool messages
        let text = match &msg.content {
            MessageContent::Text(t) => t.clone(),
            MessageContent::Parts(_) => String::new(),
        };
        return json!({ "role": "tool", "content": text });
    }

    // Assistant messages: extract tool_calls if present
    if msg.role == Role::Assistant {
        if let MessageContent::Parts(parts) = &msg.content {
            let mut text_parts = Vec::new();
            let mut tool_calls = Vec::new();
            for part in parts {
                match part {
                    ContentPart::ToolUse { id, name, input } => {
                        tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": input.to_string()
                            }
                        }));
                    }
                    ContentPart::Text { text } => text_parts.push(text.clone()),
                    _ => {}
                }
            }
            let text = text_parts.join("");
            if !tool_calls.is_empty() {
                return json!({
                    "role": "assistant",
                    "content": text,
                    "tool_calls": tool_calls,
                });
            }
        }
    }

    // Default: simple role + content
    let content = match &msg.content {
        MessageContent::Text(t) => json!(t),
        MessageContent::Parts(parts) => {
            let serialized: Vec<Value> = parts.iter().map(serialize_part).collect();
            json!(serialized)
        }
    };

    json!({ "role": role_str, "content": content })
}

fn serialize_part(part: &ContentPart) -> Value {
    match part {
        ContentPart::Text { text } => json!({ "type": "text", "text": text }),
        ContentPart::Image { url } => json!({
            "type": "image_url",
            "image_url": { "url": url }
        }),
        ContentPart::ToolUse { id, name, input } => json!({
            "type": "function",
            "id":   id,
            "function": { "name": name, "arguments": input.to_string() }
        }),
        ContentPart::ToolResult {
            tool_use_id,
            content,
            ..
        } => json!({
            "role":         "tool",
            "tool_call_id": tool_use_id,
            "content":      content,
        }),
    }
}

// ---------------------------------------------------------------------------
// SSE parser (OpenAI chat completions format)
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
    let mut has_data_line = false;
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            has_data_line = true;
            if data == "[DONE]" {
                events.push(Ok(StreamEvent::Done { usage: None }));
                continue;
            }
            if let Some(event) = parse_event(data) {
                events.push(Ok(event));
            } else {
                tracing::debug!(data, "openai: unparsed SSE data");
            }
        }
    }
    if !has_data_line && !text.trim().is_empty() {
        tracing::debug!(
            raw = &text[..text.len().min(500)],
            "openai: non-SSE chunk received"
        );
    }
    events
}

/// Strip `<think>...</think>` tags from final accumulated text (public for
/// runtime use).
pub fn strip_think_tags_pub(text: &str) -> String {
    strip_think_tags(text)
}

/// Strip `<think>...</think>` tags from content (qwen3.5, QwQ, etc.).
fn strip_think_tags(text: &str) -> String {
    // Simple approach: remove <think>...</think> blocks and lone </think> tags
    let mut result = text.to_owned();
    // Remove complete <think>...</think> blocks
    while let Some(start) = result.find("<think>") {
        if let Some(end) = result.find("</think>") {
            result = format!("{}{}", &result[..start], &result[end + 8..]);
        } else {
            // Opening tag without closing -- strip from <think> to end (partial thinking)
            result = result[..start].to_owned();
            break;
        }
    }
    // Remove lone </think> (from a previous chunk's <think>)
    result = result.replace("</think>", "");
    result
}

fn parse_event(data: &str) -> Option<StreamEvent> {
    let v: Value = serde_json::from_str(data).ok()?;

    // Check for error response embedded in SSE stream
    if let Some(err) = v.get("error") {
        let msg = err["message"].as_str().unwrap_or("unknown API error");
        return Some(StreamEvent::Error(msg.to_owned()));
    }

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
        // In streaming mode, arguments arrive as partial JSON fragments.
        // Try to parse as complete JSON; if that fails, store as raw string
        // for the runtime to accumulate across chunks.
        let input = if args_str.is_empty() {
            Value::Object(Default::default())
        } else {
            serde_json::from_str(args_str).unwrap_or_else(|_| Value::String(args_str.to_owned()))
        };
        tracing::debug!(id = %id, name = %name, args_len = args_str.len(), "openai: tool call chunk");
        return Some(StreamEvent::ToolCall { id, name, input });
    }

    // Text delta — check "content" first, then "reasoning_content"/"reasoning".
    // Do NOT strip <think> tags here — tags span multiple chunks.
    // The runtime strips them from the accumulated buffer after the stream ends.
    if let Some(text) = delta["content"].as_str()
        && !text.is_empty()
    {
        return Some(StreamEvent::TextDelta(text.to_owned()));
    }
    // DeepSeek: reasoning_content, Qwen/Ollama: reasoning — wrap in <think>
    // tags so the runtime can stream them to the user and strip them later.
    use std::cell::RefCell;
    thread_local! {
        static IN_REASONING: RefCell<bool> = const { RefCell::new(false) };
    }
    // Only handle reasoning_content (DeepSeek). Ignore "reasoning" field
    // (Ollama/Qwen3) since thinking mode is disabled — content field has the actual reply.
    let reasoning_text = delta["reasoning_content"]
        .as_str()
        .filter(|s| !s.is_empty());
    let content_text = delta["content"].as_str().filter(|s| !s.is_empty());
    let is_done = choice["finish_reason"].is_string();

    if let Some(text) = reasoning_text {
        // Reasoning chunk
        return IN_REASONING.with(|r| {
            let was = *r.borrow();
            *r.borrow_mut() = true;
            if !was {
                Some(StreamEvent::TextDelta(format!("<think>{text}")))
            } else {
                Some(StreamEvent::TextDelta(text.to_owned()))
            }
        });
    }

    // Not reasoning — close think tag if we were reasoning
    let was_reasoning = IN_REASONING.with(|r| {
        let was = *r.borrow();
        if was { *r.borrow_mut() = false; }
        was
    });

    if was_reasoning {
        if let Some(text) = content_text {
            return Some(StreamEvent::TextDelta(format!("</think>{text}")));
        }
        // No content yet, just close the tag
        return Some(StreamEvent::TextDelta("</think>".to_owned()));
    }

    // Normal content (not reasoning)
    if let Some(text) = content_text {
        return Some(StreamEvent::TextDelta(text.to_owned()));
    }

    // Finish
    if is_done {
        let usage = v["usage"].as_object().map(|u| TokenUsage {
            input: u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0) as u32,
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
// Responses API: request body builder
// ---------------------------------------------------------------------------

fn build_responses_body(req: &LlmRequest) -> Result<Value> {
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

fn parse_responses_sse_chunk(chunk: Result<bytes::Bytes>) -> Vec<Result<StreamEvent>> {
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
/// (`choices[0].delta.content`) inside a Responses API stream.
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
            model: "gpt-4o".to_owned(),
            messages: vec![],
            tools: vec![],
            system: None,
            max_tokens: None,
            temperature: None,
            thinking_budget: None,
        }
    }

    #[test]
    fn request_serializes_model() {
        let req = make_request();
        let body = build_request_body(&req).unwrap();
        assert_eq!(body["model"].as_str().unwrap(), "gpt-4o");
    }

    #[test]
    fn message_role_user() {
        let req = LlmRequest {
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hello".to_owned()),
            }],
            ..make_request()
        };
        let body = build_request_body(&req).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"].as_str().unwrap(), "user");
    }

    // -----------------------------------------------------------------------
    // Responses API tests
    // -----------------------------------------------------------------------

    mod responses_tests {
        use super::*;

        fn make_responses_request() -> LlmRequest {
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
                ..make_responses_request()
            };
            let body = build_responses_body(&req).unwrap();
            assert!(body.get("input").is_some(), "should have 'input' field");
            assert!(body.get("messages").is_none(), "should NOT have 'messages' field");
        }

        #[test]
        fn system_goes_to_instructions() {
            let req = LlmRequest {
                system: Some("be helpful".to_owned()),
                ..make_responses_request()
            };
            let body = build_responses_body(&req).unwrap();
            assert_eq!(body["instructions"].as_str().unwrap(), "be helpful");
        }

        #[test]
        fn content_parts_use_input_text() {
            let req = LlmRequest {
                messages: vec![Message {
                    role: Role::User,
                    content: MessageContent::Text("hello".to_owned()),
                }],
                ..make_responses_request()
            };
            let body = build_responses_body(&req).unwrap();
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
                ..make_responses_request()
            };
            let body = build_responses_body(&req).unwrap();
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
        fn parse_completions_fallback_test() {
            // Providers that return Chat Completions format should still work
            let data = r#"{"choices":[{"delta":{"content":"world"},"finish_reason":null}]}"#;
            let event = parse_responses_event(data, None);
            assert!(matches!(event, Some(StreamEvent::TextDelta(ref t)) if t == "world"));
        }

        #[test]
        fn parse_sse_chunk_with_event_lines() {
            let raw = b"event: response.output_text.delta\ndata: {\"delta\":\"hi\"}\n\nevent: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n";
            let events = parse_responses_sse_chunk(Ok(bytes::Bytes::from_static(raw)));
            assert_eq!(events.len(), 2);
            assert!(matches!(&events[0], Ok(StreamEvent::TextDelta(t)) if t == "hi"));
            assert!(matches!(&events[1], Ok(StreamEvent::Done { .. })));
        }
    }
}
