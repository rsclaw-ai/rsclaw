//! Shared SSE/JSONL mock builders for wiremock-based provider tests.

#![allow(dead_code, unused_imports)]

use anyhow::Result;
use futures::StreamExt;
use rsclaw::provider::{LlmStream, StreamEvent, TokenUsage};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Anthropic SSE helpers
// ---------------------------------------------------------------------------

pub enum AnthropicEvent {
    TextDelta(String),
    ThinkingDelta(String),
    InputJsonDelta(String),
    ToolUseStart {
        id: String,
        name: String,
    },
    MessageDelta {
        stop_reason: String,
        input_tokens: u32,
        output_tokens: u32,
    },
    Error(String),
    Done,
}

pub fn anthropic_sse_body(events: &[AnthropicEvent]) -> String {
    let mut body = String::new();
    for event in events {
        match event {
            AnthropicEvent::TextDelta(text) => {
                body.push_str(&format!(
                    "data: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n"
                ));
            }
            AnthropicEvent::ThinkingDelta(text) => {
                body.push_str(&format!(
                    "data: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"thinking_delta\",\"thinking\":\"{text}\"}}}}\n\n"
                ));
            }
            AnthropicEvent::InputJsonDelta(json) => {
                body.push_str(&format!(
                    "data: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{json}\"}}}}\n\n"
                ));
            }
            AnthropicEvent::ToolUseStart { id, name } => {
                body.push_str(&format!(
                    "data: {{\"type\":\"content_block_start\",\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"{name}\"}}}}\n\n"
                ));
            }
            AnthropicEvent::MessageDelta {
                stop_reason,
                input_tokens,
                output_tokens,
            } => {
                body.push_str(&format!(
                    "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop_reason}\"}},\"usage\":{{\"input_tokens\":{input_tokens},\"output_tokens\":{output_tokens}}}}}\n\n"
                ));
            }
            AnthropicEvent::Error(msg) => {
                body.push_str(&format!(
                    "data: {{\"type\":\"error\",\"error\":{{\"message\":\"{msg}\"}}}}\n\n"
                ));
            }
            AnthropicEvent::Done => {
                body.push_str("data: [DONE]\n\n");
            }
        }
    }
    body
}

pub async fn mount_anthropic_stream(server: &MockServer, events: &[AnthropicEvent]) {
    let body = anthropic_sse_body(events);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(body)
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------
// OpenAI SSE helpers
// ---------------------------------------------------------------------------

pub enum OpenAiEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallDelta {
        id: String,
        name: String,
        arguments: String,
    },
    FinishStop {
        prompt_tokens: Option<u32>,
        completion_tokens: Option<u32>,
    },
    Done,
}

pub fn openai_sse_body(events: &[OpenAiEvent]) -> String {
    let mut body = String::new();
    for event in events {
        match event {
            OpenAiEvent::TextDelta(text) => {
                body.push_str(&format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}},\"index\":0}}]}}\n\n"
                ));
            }
            OpenAiEvent::ReasoningDelta(text) => {
                body.push_str(&format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"reasoning_content\":\"{text}\"}},\"index\":0}}]}}\n\n"
                ));
            }
            OpenAiEvent::ToolCallDelta {
                id,
                name,
                arguments,
            } => {
                body.push_str(&format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"id\":\"{id}\",\"function\":{{\"name\":\"{name}\",\"arguments\":\"{arguments}\"}}}}]}},\"index\":0}}]}}\n\n"
                ));
            }
            OpenAiEvent::FinishStop {
                prompt_tokens,
                completion_tokens,
            } => {
                let usage_str = match (prompt_tokens, completion_tokens) {
                    (Some(pt), Some(ct)) => {
                        format!(",\"usage\":{{\"prompt_tokens\":{pt},\"completion_tokens\":{ct}}}")
                    }
                    _ => String::new(),
                };
                body.push_str(&format!(
                    "data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\",\"index\":0}}]{usage_str}}}\n\n"
                ));
            }
            OpenAiEvent::Done => {
                body.push_str("data: [DONE]\n\n");
            }
        }
    }
    body
}

pub async fn mount_openai_stream(server: &MockServer, events: &[OpenAiEvent]) {
    let body = openai_sse_body(events);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(body)
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(server)
        .await;
}

/// Mount a non-SSE JSON response (for the JSON fallback path).
pub async fn mount_openai_json(server: &MockServer, content: &str) {
    let body = serde_json::json!({
        "choices": [{
            "message": { "content": content },
            "finish_reason": "stop",
            "index": 0,
        }]
    });
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&body)
                .insert_header("content-type", "application/json"),
        )
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------
// Ollama JSONL helpers
// ---------------------------------------------------------------------------

pub enum OllamaEvent {
    Content(String),
    Thinking(String),
    ToolCall { name: String, arguments: serde_json::Value },
    Done,
}

pub fn ollama_jsonl_body(events: &[OllamaEvent]) -> String {
    let mut body = String::new();
    for event in events {
        match event {
            OllamaEvent::Content(text) => {
                body.push_str(&format!(
                    "{{\"message\":{{\"content\":\"{text}\"}},\"done\":false}}\n"
                ));
            }
            OllamaEvent::Thinking(text) => {
                body.push_str(&format!(
                    "{{\"message\":{{\"thinking\":\"{text}\"}},\"done\":false}}\n"
                ));
            }
            OllamaEvent::ToolCall { name, arguments } => {
                let args = serde_json::to_string(arguments).unwrap_or_default();
                body.push_str(&format!(
                    "{{\"message\":{{\"tool_calls\":[{{\"function\":{{\"name\":\"{name}\",\"arguments\":{args}}}}}]}},\"done\":false}}\n"
                ));
            }
            OllamaEvent::Done => {
                body.push_str("{\"done\":true}\n");
            }
        }
    }
    body
}

pub async fn mount_ollama_native(server: &MockServer, events: &[OllamaEvent]) {
    let body = ollama_jsonl_body(events);
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(body)
                .insert_header("content-type", "application/x-ndjson"),
        )
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------
// Gemini SSE helpers
// ---------------------------------------------------------------------------

pub enum GeminiEvent {
    Text(String),
    FunctionCall { name: String, args: serde_json::Value },
    Finish { prompt_tokens: u32, candidates_tokens: u32 },
    Error(String),
}

pub fn gemini_sse_body(events: &[GeminiEvent]) -> String {
    let mut body = String::new();
    for event in events {
        match event {
            GeminiEvent::Text(text) => {
                body.push_str(&format!(
                    "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"{text}\"}}]}}}}]}}\n\n"
                ));
            }
            GeminiEvent::FunctionCall { name, args } => {
                let args_str = serde_json::to_string(args).unwrap_or_default();
                body.push_str(&format!(
                    "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"functionCall\":{{\"name\":\"{name}\",\"args\":{args_str}}}}}]}}}}]}}\n\n"
                ));
            }
            GeminiEvent::Finish {
                prompt_tokens,
                candidates_tokens,
            } => {
                body.push_str(&format!(
                    "data: {{\"candidates\":[{{\"finishReason\":\"STOP\"}}],\"usageMetadata\":{{\"promptTokenCount\":{prompt_tokens},\"candidatesTokenCount\":{candidates_tokens}}}}}\n\n"
                ));
            }
            GeminiEvent::Error(msg) => {
                body.push_str(&format!(
                    "data: {{\"error\":{{\"message\":\"{msg}\"}}}}\n\n"
                ));
            }
        }
    }
    body
}

pub async fn mount_gemini_stream(server: &MockServer, model: &str, events: &[GeminiEvent]) {
    let body = gemini_sse_body(events);
    let stream_path = format!(
        "/v1beta/models/{model}:streamGenerateContent"
    );
    Mock::given(method("POST"))
        .and(path(stream_path))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(body)
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------
// Stream collection helpers
// ---------------------------------------------------------------------------

/// Collect all StreamEvents from a stream into a Vec.
pub async fn collect_stream_events(stream: LlmStream) -> Vec<StreamEvent> {
    stream
        .filter_map(|r| async { r.ok() })
        .collect::<Vec<_>>()
        .await
}

/// Assert that the collected events contain TextDelta events whose text
/// concatenates to the expected string.
pub fn assert_stream_text(events: &[StreamEvent], expected: &str) {
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, expected, "stream text mismatch");
}

/// Assert that there is at least one ToolCall event with the given name.
pub fn assert_stream_tool_call(events: &[StreamEvent], expected_name: &str) {
    let found = events.iter().any(|e| matches!(e, StreamEvent::ToolCall { name, .. } if name == expected_name));
    assert!(
        found,
        "expected ToolCall with name '{expected_name}' but none found in {events:?}"
    );
}

/// Assert that there is a Done event.
pub fn assert_stream_done(events: &[StreamEvent]) {
    let found = events.iter().any(|e| matches!(e, StreamEvent::Done { .. }));
    assert!(found, "expected Done event but none found in {events:?}");
}

/// Assert token usage on the Done event.
pub fn assert_usage(events: &[StreamEvent], expected_input: u32, expected_output: u32) {
    for event in events {
        if let StreamEvent::Done {
            usage: Some(usage), ..
        } = event
        {
            assert_eq!(usage.input, expected_input, "input tokens mismatch");
            assert_eq!(usage.output, expected_output, "output tokens mismatch");
            return;
        }
    }
    panic!("no Done event with usage found in {events:?}");
}
