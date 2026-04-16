//! Wiremock integration tests for AnthropicProvider SSE parsing.

mod common;

use std::sync::Once;

static INIT_TLS: Once = Once::new();
fn init_tls() {
    INIT_TLS.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

use rsclaw::provider::{
    LlmProvider, LlmRequest, Message, MessageContent, Role, StreamEvent, ToolDef,
    anthropic::AnthropicProvider,
};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::mock_provider::{
    AnthropicEvent, assert_stream_done, assert_stream_text, assert_stream_tool_call, assert_usage,
    collect_stream_events, mount_anthropic_stream,
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
    }
}

// ---------------------------------------------------------------------------
// SSE parsing tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_text_delta_events() {
    init_tls();
    let server = MockServer::start().await;
    mount_anthropic_stream(
        &server,
        &[
            AnthropicEvent::TextDelta("Hello".to_owned()),
            AnthropicEvent::TextDelta(" world".to_owned()),
            AnthropicEvent::MessageDelta {
                stop_reason: "end_turn".to_owned(),
                input_tokens: 10,
                output_tokens: 5,
            },
            AnthropicEvent::Done,
        ],
    )
    .await;

    let provider = AnthropicProvider::with_base_url("test-key", &server.uri());
    let stream = provider.stream(simple_request("claude-3-5-sonnet")).await.unwrap();
    let events = collect_stream_events(stream).await;

    assert_stream_text(&events, "Hello world");
    assert_stream_done(&events);
    assert_usage(&events, 10, 5);
}

#[tokio::test]
async fn stream_tool_use() {
    init_tls();
    let server = MockServer::start().await;
    mount_anthropic_stream(
        &server,
        &[
            AnthropicEvent::ToolUseStart {
                id: "tu_1".to_owned(),
                name: "search".to_owned(),
            },
            AnthropicEvent::MessageDelta {
                stop_reason: "tool_use".to_owned(),
                input_tokens: 20,
                output_tokens: 10,
            },
            AnthropicEvent::Done,
        ],
    )
    .await;

    let provider = AnthropicProvider::with_base_url("test-key", &server.uri());
    let stream = provider.stream(simple_request("claude-3-5-sonnet")).await.unwrap();
    let events = collect_stream_events(stream).await;

    assert_stream_tool_call(&events, "search");
    assert_stream_done(&events);
}

#[tokio::test]
#[ignore = "thinking deltas are now forwarded as ReasoningDelta, not discarded"]
async fn stream_thinking_delta_discarded() {
    init_tls();
    let server = MockServer::start().await;
    mount_anthropic_stream(
        &server,
        &[
            AnthropicEvent::ThinkingDelta("I am thinking".to_owned()),
            AnthropicEvent::TextDelta("Result".to_owned()),
            AnthropicEvent::MessageDelta {
                stop_reason: "end_turn".to_owned(),
                input_tokens: 5,
                output_tokens: 2,
            },
            AnthropicEvent::Done,
        ],
    )
    .await;

    let provider = AnthropicProvider::with_base_url("test-key", &server.uri());
    let stream = provider.stream(simple_request("claude-3-5-sonnet")).await.unwrap();
    let events = collect_stream_events(stream).await;

    // Thinking deltas should be discarded, only "Result" appears
    assert_stream_text(&events, "Result");
    // No ReasoningDelta events should appear from Anthropic provider
    let has_reasoning = events
        .iter()
        .any(|e| matches!(e, StreamEvent::ReasoningDelta(_)));
    assert!(!has_reasoning, "thinking delta should be discarded");
}

#[tokio::test]
async fn stream_error_event() {
    init_tls();
    let server = MockServer::start().await;
    mount_anthropic_stream(
        &server,
        &[AnthropicEvent::Error("overloaded".to_owned())],
    )
    .await;

    let provider = AnthropicProvider::with_base_url("test-key", &server.uri());
    let stream = provider.stream(simple_request("claude-3-5-sonnet")).await.unwrap();
    let events = collect_stream_events(stream).await;

    let has_error = events.iter().any(|e| {
        matches!(e, StreamEvent::Error(msg) if msg.contains("overloaded"))
    });
    assert!(has_error, "expected Error event with 'overloaded'");
}

// ---------------------------------------------------------------------------
// HTTP error tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_401_error() {
    init_tls();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(r#"{"error":"invalid_api_key"}"#),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::with_base_url("bad-key", &server.uri());
    let result = provider.stream(simple_request("claude-3-5-sonnet")).await;
    let err_msg = result.err().expect("expected error").to_string();
    assert!(err_msg.contains("401"), "expected 401 in error: {err_msg}");
}

#[tokio::test]
async fn http_429_error() {
    init_tls();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(
            ResponseTemplate::new(429).set_body_string(r#"{"error":"rate_limited"}"#),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::with_base_url("key", &server.uri());
    let result = provider.stream(simple_request("claude-3-5-sonnet")).await;
    let err_msg = result.err().expect("expected error").to_string();
    assert!(err_msg.contains("429"), "expected 429 in error: {err_msg}");
}

#[tokio::test]
async fn http_500_error() {
    init_tls();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal server error"))
        .mount(&server)
        .await;

    let provider = AnthropicProvider::with_base_url("key", &server.uri());
    let result = provider.stream(simple_request("claude-3-5-sonnet")).await;
    let err_msg = result.err().expect("expected error").to_string();
    assert!(err_msg.contains("500"), "expected 500 in error: {err_msg}");
}

// ---------------------------------------------------------------------------
// Request structure tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn request_includes_correct_headers() {
    init_tls();
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/messages"))
        .and(header("x-api-key", "my-secret-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(header("content-type", "application/json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}\n\ndata: [DONE]\n\n")
                .insert_header("content-type", "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = AnthropicProvider::with_base_url("my-secret-key", &server.uri());
    let result = provider.stream(simple_request("claude-3-5-sonnet")).await;
    assert!(result.is_ok(), "request should succeed when headers match");
}

#[tokio::test]
async fn request_body_maps_messages() {
    init_tls();
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}\n\ndata: [DONE]\n\n")
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&server)
        .await;

    let req = LlmRequest {
        model: "claude-3-5-sonnet".to_owned(),
        messages: vec![
            Message {
                role: Role::User,
                content: MessageContent::Text("What is 2+2?".to_owned()),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Text("4".to_owned()),
            },
        ],
        tools: vec![ToolDef {
            name: "calc".to_owned(),
            description: "Calculator".to_owned(),
            parameters: serde_json::json!({"type": "object"}),
        }],
        system: Some("Be precise".to_owned()),
        max_tokens: Some(512),
        temperature: Some(0.3),
        frequency_penalty: None,
        thinking_budget: None,
    };

    let provider = AnthropicProvider::with_base_url("key", &server.uri());
    let result = provider.stream(req).await;
    assert!(
        result.is_ok(),
        "request with full body should succeed: {:?}",
        result.err()
    );
}
