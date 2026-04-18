//! OpenAI-compatible provider (openai-responses / openai-completions / Ollama).
//!
//! Supports:
//!   - OpenAI Chat Completions API (`openai-completions`, default)
//!   - OpenAI Responses API (`openai-responses`) — newer streaming format
//!   - Ollama (same completions wire format, custom base_url)

use std::collections::HashMap;

use anyhow::{Context, Result};
use futures::{StreamExt, TryStreamExt, future::BoxFuture};
use serde_json::{Value, json};

use super::{
    ContentPart, LlmProvider, LlmRequest, LlmStream, Message, MessageContent, Role, StreamEvent,
    TokenUsage,
};

pub(crate) const OPENAI_API_BASE: &str = "https://api.openai.com/v1";
#[allow(dead_code)]
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
            mode: OpenAiMode::Chat,
        }
    }

    /// Create a provider with custom User-Agent (OpenAI Chat mode).
    pub fn with_user_agent(
        base_url: impl Into<String>,
        api_key: Option<String>,
        user_agent: Option<String>,
    ) -> Self {
        Self {
            client: super::http_client_with_ua(user_agent.as_deref()),
            api_key,
            base_url: base_url.into(),
            is_ollama: false,
            mode: OpenAiMode::Chat,
        }
    }

    /// Create a Responses-API provider with custom User-Agent.
    pub fn responses_with_ua(
        base_url: impl Into<String>,
        api_key: Option<String>,
        user_agent: Option<String>,
    ) -> Self {
        Self {
            client: super::http_client_with_ua(user_agent.as_deref()),
            api_key,
            base_url: base_url.into(),
            is_ollama: false,
            mode: OpenAiMode::Responses,
        }
    }

    /// Create an Ollama-backed provider with custom User-Agent.
    pub fn ollama_with_ua(
        base_url: impl Into<String>,
        api_key: Option<String>,
        user_agent: Option<String>,
    ) -> Self {
        Self {
            client: super::http_client_with_ua(user_agent.as_deref()),
            api_key,
            base_url: base_url.into(),
            is_ollama: true,
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
            tracing::info!(
                model = %req.model,
                max_tokens = ?req.max_tokens,
                thinking_budget = ?req.thinking_budget,
                "openai: preparing LLM request"
            );

            // Ollama: if base_url does NOT contain "/v1", use native /api/chat.
            // If it has "/v1" (e.g. http://localhost:11434/v1), fall through to
            // OpenAI-compatible /chat/completions path.
            if self.is_ollama && !self.base_url.contains("/v1") {
                return self.stream_ollama_native(&req).await;
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
            #[cfg(debug_assertions)]
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
            tracing::info!(
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
                tracing::info!(body = %body.to_string().chars().take(500).collect::<String>(), "openai: non-streaming JSON response");
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

            // SSE parser with line buffer to handle chunks that split lines.
            // This is critical for correctness: SSE data can be split across
            // multiple chunks, and we must buffer incomplete lines.
            let line_buffer = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
            let event_stream = byte_stream
                .map_err(|e| anyhow::anyhow!("stream read error: {e}"))
                .then(move |chunk| {
                    let line_buffer = line_buffer.clone();
                    async move {
                        parse_sse_chunk_with_buffer(chunk, &line_buffer).await
                    }
                })
                .flat_map(|events| futures::stream::iter(events));

            let stream: LlmStream = Box::pin(event_stream);
            Ok(stream)
        })
    }
}

impl OpenAiProvider {
    /// Use ollama native /api/chat for reasoning models with think=true.
    /// This gives properly formatted content with newlines.
    async fn stream_ollama_native(&self, req: &LlmRequest) -> Result<LlmStream> {
        // Build ollama native API URL
        let base = self.base_url.trim_end_matches('/');
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

        // Ollama uses "options" for parameters, not top-level fields.
        let mut options = serde_json::Map::new();
        if let Some(t) = req.temperature {
            options.insert("temperature".into(), json!(t));
        }
        if let Some(max) = req.max_tokens {
            if max > 0 {
                options.insert("num_predict".into(), json!(max));
            }
        }
        if !options.is_empty() {
            body["options"] = Value::Object(options);
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

    /// Upload a base64 data URI image to the provider's Files API.
    /// Returns the file_id on success.
    async fn upload_image_to_files(&self, data_uri: &str) -> Result<String> {
        use base64::Engine;

        // Parse "data:image/png;base64,{base64_data}"
        let rest = data_uri
            .strip_prefix("data:")
            .ok_or_else(|| anyhow::anyhow!("invalid data URI: missing data: prefix"))?;
        let (meta, b64_data) = rest
            .split_once(',')
            .ok_or_else(|| anyhow::anyhow!("invalid data URI: missing comma"))?;
        let mime_type = meta.split(';').next().unwrap_or("image/png");
        let ext = match mime_type {
            "image/jpeg" | "image/jpg" => "jpg",
            "image/png" => "png",
            "image/gif" => "gif",
            "image/webp" => "webp",
            "video/mp4" => "mp4",
            "video/quicktime" => "mov",
            "video/webm" => "webm",
            "video/x-msvideo" => "avi",
            _ => "bin",
        };

        let image_bytes = base64::engine::general_purpose::STANDARD
            .decode(b64_data)
            .context("failed to decode base64 image data")?;

        let filename = format!("upload.{ext}");
        let file_part = reqwest::multipart::Part::bytes(image_bytes)
            .file_name(filename)
            .mime_str(mime_type)
            .context("invalid mime type")?;

        let form = reqwest::multipart::Form::new()
            .text("purpose", "user_data")
            .part("file", file_part);

        let url = format!("{}/files", self.base_url.trim_end_matches('/'));

        let mut builder = self.client.post(&url);
        if let Some(key) = &self.api_key {
            builder = builder.header("authorization", format!("Bearer {key}"));
        }

        let resp = builder
            .multipart(form)
            .timeout(std::time::Duration::from_secs(500))
            .send()
            .await
            .context("Files API upload request failed")?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Files API upload error: {body}");
        }

        let body: Value = resp.json().await.context("Files API: parse response")?;
        let file_id = body["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Files API response missing id field: {body}"))?
            .to_owned();

        tracing::debug!(file_id = %file_id, "uploaded image to Files API");
        Ok(file_id)
    }

    /// Scan messages for data: URI images and upload them to the Files API.
    /// Returns a mapping from data_uri -> file_id for successful uploads.
    async fn upload_images_for_messages(&self, messages: &[Message]) -> HashMap<String, String> {
        let mut file_id_map = HashMap::new();
        let mut data_uris: Vec<String> = Vec::new();

        // Collect all data: URIs from messages
        for msg in messages {
            if let MessageContent::Parts(parts) = &msg.content {
                for part in parts {
                    if let ContentPart::Image { url } = part {
                        if url.starts_with("data:") && !data_uris.contains(url) {
                            data_uris.push(url.clone());
                        }
                    }
                }
            }
        }

        tracing::info!(count = data_uris.len(), "upload_images: found data URIs to upload");
        // Upload each unique data URI
        for uri in data_uris {
            match self.upload_image_to_files(&uri).await {
                Ok(file_id) => {
                    file_id_map.insert(uri, file_id);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to upload image to Files API, falling back to base64 inline");
                }
            }
        }

        file_id_map
    }

    /// Stream using the OpenAI Responses API format.
    async fn stream_responses(&self, req: &LlmRequest) -> Result<LlmStream> {
        // Upload data: URI images to Files API before building the request body
        let file_id_map = self.upload_images_for_messages(&req.messages).await;

        let body = build_responses_body(req, &file_id_map)?;
        let body_str = serde_json::to_string(&body).unwrap_or_default();
        tracing::debug!(
            model = %req.model,
            tools_count = req.tools.len(),
            body_len = body_str.len(),
            "openai-responses: request prepared"
        );
        #[cfg(debug_assertions)]
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
        if max_tokens > 0 {
            body["max_tokens"] = json!(max_tokens);
        }
    }

    // Thinking/reasoning mode: configurable via agents.defaults.thinking or per-agent thinking.
    // Default: disabled. When enabled with budget > 0, model uses reasoning_content field.
    let model_lower = req.model.to_lowercase();
    let is_minimax = model_lower.contains("minimax");

    match req.thinking_budget {
        Some(budget) if budget > 0 => {
            body["enable_thinking"] = json!(true);
            body["thinking_budget"] = json!(budget);
            body["chat_template_kwargs"] = json!({"enable_thinking": true});
        }
        _ => {
            // Disable thinking for models that support it (DashScope, llama.cpp).
            body["enable_thinking"] = json!(false);
            body["chat_template_kwargs"] = json!({"enable_thinking": false});
        }
    }

    // MiniMax: always split reasoning into reasoning_content field so <think>
    // tags don't leak into content. Works regardless of thinking_budget.
    if is_minimax {
        body["reasoning_split"] = json!(true);
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

    if let Some(fp) = req.frequency_penalty {
        if fp > 0.0 {
            body["frequency_penalty"] = json!(fp);
        }
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
// SSE parser with line buffering (handles chunks that split lines)
// ---------------------------------------------------------------------------

/// Parse SSE chunk with line buffering.
/// SSE data lines can be split across multiple chunks, causing JSON parsing
/// failures if we process each chunk independently. This function buffers
/// incomplete lines until they are complete.
async fn parse_sse_chunk_with_buffer(
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
            // Log and replace invalid bytes with U+FFFD so the stream continues.
            tracing::warn!("openai: UTF-8 decode error at byte {}, replacing with �: {}", e.valid_up_to(), e);
            std::borrow::Cow::Owned(String::from_utf8_lossy(&bytes).into_owned())
        }
    };

    // Lock the buffer and append the new text
    let mut buffer = line_buffer.lock().await;
    buffer.push_str(&text);

    let mut events = Vec::new();

    // Find the last complete line (ending with newline)
    // Process complete lines and keep incomplete portion in buffer
    if let Some(last_newline_pos) = buffer.rfind('\n') {
        // Extract complete portion (up to and including the last newline)
        let complete_portion = buffer[..last_newline_pos].to_owned();

        // Keep incomplete portion (after the last newline) in buffer
        let incomplete_portion = buffer[last_newline_pos + 1..].to_owned();
        buffer.clear();
        buffer.push_str(&incomplete_portion);

        // Process complete lines
        for line in complete_portion.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                if data == "[DONE]" {
                    events.push(Ok(StreamEvent::Done { usage: None }));
                    continue;
                }
                if let Some(event) = parse_event(data) {
                    events.push(Ok(event));
                } else {
                    tracing::warn!(data, "openai: unparsed SSE data");
                }
            }
        }
    }
    // If no newline found, buffer contains incomplete line - wait for more data

    events
}

// ---------------------------------------------------------------------------
// SSE parser (OpenAI chat completions format) - legacy without buffering
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn parse_sse_chunk(chunk: Result<bytes::Bytes>) -> Vec<Result<StreamEvent>> {
    let bytes = match chunk {
        Ok(b) => b,
        Err(e) => return vec![Err(e)],
    };

    let text = String::from_utf8_lossy(&bytes);

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
        tracing::warn!(
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
    let mut result = text.to_owned();
    // Remove complete <think>...</think> blocks.
    // Search for </think> strictly AFTER each <think> to avoid matching a lone
    // </think> that appears earlier in the string (stream-chunk residual).
    while let Some(start) = result.find("<think>") {
        // start + 7 skips past the 7-byte "<think>" tag itself.
        if let Some(rel_end) = result[start + 7..].find("</think>") {
            let end = start + 7 + rel_end;
            result = format!("{}{}", &result[..start], &result[end + 8..]);
        } else {
            // Opening tag without closing — strip from <think> to end.
            result = result[..start].to_owned();
            break;
        }
    }
    // Remove any residual lone </think> tags.
    result = result.replace("</think>", "");
    result
}

fn parse_event(data: &str) -> Option<StreamEvent> {
    let v: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(data, error = %e, "openai: failed to parse SSE JSON");
            return None;
        }
    };

    // Check for error response embedded in SSE stream
    if let Some(err) = v.get("error") {
        let msg = err["message"].as_str().unwrap_or("unknown API error");
        return Some(StreamEvent::Error(msg.to_owned()));
    }

    let choices = match v["choices"].as_array() {
        Some(c) => c,
        None => {
            // Log when choices is missing or not an array
            tracing::warn!(data, "openai: SSE response missing choices array");
            return None;
        }
    };
    let choice = match choices.first() {
        Some(c) => c,
        None => {
            tracing::warn!(data, "openai: SSE response has empty choices array");
            return None;
        }
    };
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

    // Text / reasoning deltas — stateless: emit ReasoningDelta or TextDelta
    // based solely on which field is present in this chunk. No cross-chunk state
    // needed (the old thread_local IN_REASONING was unsafe with concurrent streams
    // on the same tokio thread).
    let reasoning_text = delta["reasoning_content"]
        .as_str()
        .filter(|s| !s.is_empty());
    let content_text = delta["content"].as_str().filter(|s| !s.is_empty());
    let is_done = choice["finish_reason"].is_string();

    if let Some(text) = reasoning_text {
        return Some(StreamEvent::ReasoningDelta(text.to_owned()));
    }

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

fn build_responses_body(req: &LlmRequest, file_id_map: &HashMap<String, String>) -> Result<Value> {
    let input: Vec<Value> = req
        .messages
        .iter()
        .filter(|m| m.role != Role::System)
        .flat_map(|m| serialize_input_items(m, file_id_map))
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

    if let Some(max_tokens) = req.max_tokens {
        if max_tokens > 0 {
            body["max_output_tokens"] = json!(max_tokens);
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

/// Serialize a Message into one or more Responses API input items.
/// Returns Vec because assistant tool calls become separate top-level items.
fn serialize_input_items(msg: &Message, file_id_map: &HashMap<String, String>) -> Vec<Value> {
    let role_str = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::System => "user",
    };

    // Tool role messages → function_call_output (top-level item)
    // Skip items with empty call_id (legacy/corrupt history)
    if msg.role == Role::Tool {
        if let MessageContent::Parts(parts) = &msg.content {
            let items: Vec<Value> = parts
                .iter()
                .filter_map(|p| {
                    if let ContentPart::ToolResult { tool_use_id, content, .. } = p {
                        if tool_use_id.is_empty() { return None; }
                        Some(json!({ "type": "function_call_output", "call_id": tool_use_id, "output": content }))
                    } else {
                        None
                    }
                })
                .collect();
            if !items.is_empty() { return items; }
        }
        // Skip tool messages without valid call_id — legacy history
        return vec![];
    }

    // Assistant messages → text as message item + tool calls as separate top-level items
    if msg.role == Role::Assistant {
        let mut result: Vec<Value> = Vec::new();
        let mut text_parts = Vec::new();

        match &msg.content {
            MessageContent::Text(t) => text_parts.push(t.clone()),
            MessageContent::Parts(parts) => {
                for part in parts {
                    match part {
                        ContentPart::Text { text } => text_parts.push(text.clone()),
                        ContentPart::ToolUse { id, name, input } => {
                            // Skip tool calls with empty id (legacy history)
                            if !id.is_empty() {
                                result.push(json!({
                                    "type": "function_call",
                                    "call_id": id,
                                    "name": name,
                                    "arguments": input.to_string(),
                                    "status": "completed",
                                }));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        let text = text_parts.join("");
        // Only emit assistant message if there's actual text or valid tool calls
        if !text.is_empty() {
            result.insert(0, json!({
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{ "type": "output_text", "text": text }],
            }));
        }
        if !result.is_empty() {
            return result;
        }
        // Empty assistant message with no valid tool calls — skip (legacy history)
    }

    // User messages → only text/image parts (no tool parts in content)
    let content = match &msg.content {
        MessageContent::Text(t) => json!([{ "type": "input_text", "text": t }]),
        MessageContent::Parts(parts) => {
            let serialized: Vec<Value> = parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(json!({ "type": "input_text", "text": text })),
                    ContentPart::Image { url } => {
                        Some(serialize_media_for_responses(url, file_id_map))
                    }
                    _ => None,
                })
                .collect();
            if serialized.is_empty() {
                json!([{ "type": "input_text", "text": "" }])
            } else {
                json!(serialized)
            }
        }
    };

    vec![json!({ "role": role_str, "content": content })]
}

/// Serialize a media URL (image/video) for the Responses API.
/// - data: URI → file_id reference (if uploaded) or base64 inline fallback
/// - Video URL (.mp4/.mov/.avi/.webm) → input_video
/// - Image URL → input_image
fn serialize_media_for_responses(url: &str, file_id_map: &HashMap<String, String>) -> Value {
    // data: URI → upload to Files API, reference by file_id
    if url.starts_with("data:") {
        if let Some(file_id) = file_id_map.get(url) {
            // Detect video vs image from mime type in data URI
            if url.starts_with("data:video/") {
                return json!({ "type": "input_video", "file_id": file_id });
            }
            return json!({ "type": "input_image", "file_id": file_id });
        }
        // Video fallback: skip (too large for inline, runtime will handle transcription)
        if url.starts_with("data:video/") {
            tracing::warn!("video upload failed, skipping input_video (fallback to transcription)");
            return json!({ "type": "input_text", "text": "[video attached — audio transcription fallback]" });
        }
        // Image fallback: inline base64
        return json!({ "type": "input_image", "image_url": url });
    }

    // Regular URL → detect type by extension
    let lower = url.to_lowercase();
    let path = lower.split('?').next().unwrap_or(&lower);
    if path.ends_with(".mp4")
        || path.ends_with(".mov")
        || path.ends_with(".avi")
        || path.ends_with(".webm")
        || path.ends_with(".mkv")
    {
        json!({ "type": "input_video", "video_url": url })
    } else {
        json!({ "type": "input_image", "image_url": url })
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

    let text = String::from_utf8_lossy(&bytes);

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
                let id = item["call_id"]
                    .as_str()
                    .or_else(|| item["id"].as_str())
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
            frequency_penalty: None,
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
                frequency_penalty: None,
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
            let body = build_responses_body(&req, &HashMap::new()).unwrap();
            assert!(body.get("input").is_some(), "should have 'input' field");
            assert!(body.get("messages").is_none(), "should NOT have 'messages' field");
        }

        #[test]
        fn system_goes_to_instructions() {
            let req = LlmRequest {
                system: Some("be helpful".to_owned()),
                ..make_responses_request()
            };
            let body = build_responses_body(&req, &HashMap::new()).unwrap();
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
            let body = build_responses_body(&req, &HashMap::new()).unwrap();
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
            let body = build_responses_body(&req, &HashMap::new()).unwrap();
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
        fn strip_think_normal() {
            // Normal: think block before answer
            let text = "<think>reasoning</think>answer text";
            assert_eq!(strip_think_tags(text), "answer text");
        }

        #[test]
        fn strip_think_lone_close_before_open() {
            // Bug case: lone </think> appears before <think>...</think>
            // This caused the original code to eat content between them
            let text = "</think>\nThe IP is 127.0.0.1 port 5432\n<think>extra</think>";
            let result = strip_think_tags(text);
            assert!(result.contains("127.0.0.1"), "IP should not be eaten: {result:?}");
            assert!(result.contains("5432"), "port should not be eaten: {result:?}");
        }

        #[test]
        fn strip_think_no_tags() {
            // No think tags: text unchanged
            let text = "The answer is 127.0.0.1 and port 5432";
            assert_eq!(strip_think_tags(text), text);
        }

        #[test]
        fn strip_think_unclosed() {
            // Unclosed <think>: strip from tag to end
            let text = "prefix <think>partial reasoning";
            assert_eq!(strip_think_tags(text), "prefix ");
        }

        #[test]
        fn strip_think_multiple_blocks() {
            // Multiple think blocks all removed
            let text = "<think>a</think>answer<think>b</think> rest";
            assert_eq!(strip_think_tags(text), "answer rest");
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

    #[tokio::test]
    async fn sse_line_buffer_handles_split_lines() {
        // Test that SSE data split across chunks is correctly reassembled
        let buffer = tokio::sync::Mutex::new(String::new());

        let chunk1 = Ok(bytes::Bytes::from(
            r#"data: {"choices":[{"delta":{"content":"he"#,
        ));
        let events1 = parse_sse_chunk_with_buffer(chunk1, &buffer).await;
        assert!(events1.is_empty(), "Expected no events from incomplete chunk, got {:?}", events1);

        {
            let buf = buffer.lock().await;
            assert!(buf.contains("he"), "Buffer should contain 'he', got: {}", *buf);
        }

        let chunk2 = Ok(bytes::Bytes::from("l\"}}]}\n"));
        let events2 = parse_sse_chunk_with_buffer(chunk2, &buffer).await;
        assert_eq!(events2.len(), 1, "Expected 1 event, got {:?}", events2);
        match &events2[0] {
            Ok(StreamEvent::TextDelta(text)) => assert_eq!(text, "hel"),
            other => panic!("Expected TextDelta, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn sse_line_buffer_handles_multiple_lines() {
        let buffer = tokio::sync::Mutex::new(String::new());

        let chunk = Ok(bytes::Bytes::from(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\
             data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n",
        ));
        let events = parse_sse_chunk_with_buffer(chunk, &buffer).await;
        assert_eq!(events.len(), 2);
        match &events[0] {
            Ok(StreamEvent::TextDelta(text)) => assert_eq!(text, "hello"),
            other => panic!("Expected TextDelta, got {:?}", other),
        }
        match &events[1] {
            Ok(StreamEvent::TextDelta(text)) => assert_eq!(text, " world"),
            other => panic!("Expected TextDelta, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn sse_line_buffer_handles_trailing_incomplete_line() {
        let buffer = tokio::sync::Mutex::new(String::new());

        let chunk1 = Ok(bytes::Bytes::from(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"incom",
        ));
        let events1 = parse_sse_chunk_with_buffer(chunk1, &buffer).await;
        assert_eq!(events1.len(), 1);

        let chunk2 = Ok(bytes::Bytes::from("plete\"}}]}\n"));
        let events2 = parse_sse_chunk_with_buffer(chunk2, &buffer).await;
        assert_eq!(events2.len(), 1);
        match &events2[0] {
            Ok(StreamEvent::TextDelta(text)) => assert_eq!(text, "incomplete"),
            other => panic!("Expected TextDelta, got {:?}", other),
        }
    }
}
