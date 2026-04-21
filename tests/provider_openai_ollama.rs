//! Wiremock integration tests for Ollama JSONL format via OpenAiProvider::ollama.

mod common;

use std::sync::Once;

static INIT_TLS: Once = Once::new();
fn init_tls() {
    INIT_TLS.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

use rsclaw::provider::{
    LlmProvider, LlmRequest, Message, MessageContent, Role, StreamEvent,
    openai::OpenAiProvider,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::mock_provider::{
    OllamaEvent, collect_stream_events, mount_ollama_native,
};

fn simple_request(model: &str) -> LlmRequest {
    LlmRequest {
        model: model.to_owned(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("hello".to_owned()),
        }],
        tools: vec![],
        system: None,
        max_tokens: Some(1024),
        temperature: None,
        frequency_penalty: None,
        thinking_budget: None,
        kv_cache_mode: 0,
        session_key: None,
    }
}

// ---------------------------------------------------------------------------
// Model routing tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn qwen3_routes_to_api_chat() {
    init_tls();
    let server = MockServer::start().await;

    // Mount on /api/chat (native ollama path)
    mount_ollama_native(
        &server,
        &[
            OllamaEvent::Content("Hello from qwen3".to_owned()),
            OllamaEvent::Done,
        ],
    )
    .await;

    let provider = OpenAiProvider::ollama(&server.uri(), None);
    let stream = provider
        .stream(simple_request("qwen3:8b"))
        .await
        .unwrap();
    let events = collect_stream_events(stream).await;

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        text.contains("Hello from qwen3"),
        "expected qwen3 content, got: {text}"
    );
}

#[tokio::test]
async fn non_reasoning_model_routes_to_v1_completions() {
    init_tls();
    let server = MockServer::start().await;

    // Mount on /v1/chat/completions (OpenAI-compatible path).
    // Provider gets base_url with /v1 suffix so it uses the OAI-compat path.
    let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hi from llama\"},\"index\":0}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\",\"index\":0}]}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(sse_body)
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&server)
        .await;

    let provider = OpenAiProvider::ollama(format!("{}/v1", server.uri()), None);
    let stream = provider
        .stream(simple_request("llama3:8b"))
        .await
        .unwrap();
    let events = collect_stream_events(stream).await;

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        text.contains("Hi from llama"),
        "expected llama content, got: {text}"
    );
}

// ---------------------------------------------------------------------------
// Ollama native JSONL stream tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ollama_content_stream() {
    init_tls();
    let server = MockServer::start().await;
    mount_ollama_native(
        &server,
        &[
            OllamaEvent::Content("Hello ".to_owned()),
            OllamaEvent::Content("world".to_owned()),
            OllamaEvent::Done,
        ],
    )
    .await;

    let provider = OpenAiProvider::ollama(&server.uri(), None);
    let stream = provider
        .stream(simple_request("qwen3:8b"))
        .await
        .unwrap();
    let events = collect_stream_events(stream).await;

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello world");

    let has_done = events.iter().any(|e| matches!(e, StreamEvent::Done { .. }));
    assert!(has_done, "expected Done event");
}

#[tokio::test]
async fn ollama_thinking_stream() {
    init_tls();
    let server = MockServer::start().await;
    mount_ollama_native(
        &server,
        &[
            OllamaEvent::Thinking("Let me think...".to_owned()),
            OllamaEvent::Content("Answer".to_owned()),
            OllamaEvent::Done,
        ],
    )
    .await;

    let provider = OpenAiProvider::ollama(&server.uri(), None);
    let stream = provider
        .stream(simple_request("qwen3:8b"))
        .await
        .unwrap();
    let events = collect_stream_events(stream).await;

    // Thinking should be wrapped in <think> tags
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        text.contains("<think>"),
        "expected <think> tag in output, got: {text}"
    );
    assert!(
        text.contains("</think>"),
        "expected </think> tag in output, got: {text}"
    );
    assert!(
        text.contains("Answer"),
        "expected answer content, got: {text}"
    );
}

#[tokio::test]
async fn ollama_tool_call() {
    init_tls();
    let server = MockServer::start().await;
    mount_ollama_native(
        &server,
        &[
            OllamaEvent::ToolCall {
                name: "get_weather".to_owned(),
                arguments: serde_json::json!({"location": "Tokyo"}),
            },
            OllamaEvent::Done,
        ],
    )
    .await;

    let provider = OpenAiProvider::ollama(&server.uri(), None);
    let stream = provider
        .stream(simple_request("qwen3:8b"))
        .await
        .unwrap();
    let events = collect_stream_events(stream).await;

    let tool_call = events.iter().find_map(|e| match e {
        StreamEvent::ToolCall { name, input, .. } => Some((name.clone(), input.clone())),
        _ => None,
    });
    assert!(tool_call.is_some(), "expected a ToolCall event");
    let (name, input) = tool_call.unwrap();
    assert_eq!(name, "get_weather");
    assert_eq!(input["location"], "Tokyo");
}

// ---------------------------------------------------------------------------
// Ollama HTTP error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ollama_http_error() {
    init_tls();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(500).set_body_string("model not found"),
        )
        .mount(&server)
        .await;

    let provider = OpenAiProvider::ollama(&server.uri(), None);
    let result = provider.stream(simple_request("qwen3:8b")).await;
    let err_msg = result.err().expect("expected error").to_string();
    assert!(
        err_msg.contains("model not found"),
        "expected error body, got: {err_msg}"
    );
}
