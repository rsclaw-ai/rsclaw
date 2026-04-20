//! Wiremock integration tests for OpenAiProvider SSE parsing.

mod common;

use std::sync::Once;

static INIT_TLS: Once = Once::new();
fn init_tls() {
    INIT_TLS.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

use rsclaw::provider::{
    LlmProvider, LlmRequest, Message, MessageContent, Role,
    openai::{OpenAiProvider, strip_think_tags_pub},
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::mock_provider::{
    OpenAiEvent, assert_stream_done, assert_stream_text, assert_stream_tool_call,
    collect_stream_events, mount_openai_json, mount_openai_stream,
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
// SSE parsing tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_text_delta() {
    init_tls();
    let server = MockServer::start().await;
    mount_openai_stream(
        &server,
        &[
            OpenAiEvent::TextDelta("Hello".to_owned()),
            OpenAiEvent::TextDelta(" world".to_owned()),
            OpenAiEvent::FinishStop {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
            },
            OpenAiEvent::Done,
        ],
    )
    .await;

    let provider = OpenAiProvider::with_base_url(&server.uri(), Some("test-key".to_owned()));
    let stream = provider.stream(simple_request("gpt-4o")).await.unwrap();
    let events = collect_stream_events(stream).await;

    assert_stream_text(&events, "Hello world");
    assert_stream_done(&events);
}

#[tokio::test]
async fn stream_tool_call_delta() {
    init_tls();
    let server = MockServer::start().await;
    mount_openai_stream(
        &server,
        &[
            OpenAiEvent::ToolCallDelta {
                id: "tc_1".to_owned(),
                name: "search".to_owned(),
                arguments: r#"{\"q\":\"test\"}"#.to_owned(),
            },
            OpenAiEvent::FinishStop {
                prompt_tokens: None,
                completion_tokens: None,
            },
            OpenAiEvent::Done,
        ],
    )
    .await;

    let provider = OpenAiProvider::with_base_url(&server.uri(), Some("test-key".to_owned()));
    let stream = provider.stream(simple_request("gpt-4o")).await.unwrap();
    let events = collect_stream_events(stream).await;

    assert_stream_tool_call(&events, "search");
    assert_stream_done(&events);
}

#[tokio::test]
async fn non_sse_json_response() {
    init_tls();
    let server = MockServer::start().await;
    mount_openai_json(&server, "This is JSON content").await;

    let provider = OpenAiProvider::with_base_url(&server.uri(), Some("test-key".to_owned()));
    let stream = provider.stream(simple_request("gpt-4o")).await.unwrap();
    let events = collect_stream_events(stream).await;

    assert_stream_text(&events, "This is JSON content");
    assert_stream_done(&events);
}

// ---------------------------------------------------------------------------
// HTTP error tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_error_401() {
    init_tls();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::with_base_url(&server.uri(), Some("bad-key".to_owned()));
    let result = provider.stream(simple_request("gpt-4o")).await;
    let err_msg = result.err().expect("expected error").to_string();
    assert!(err_msg.contains("401"), "expected 401: {err_msg}");
}

#[tokio::test]
async fn http_error_429() {
    init_tls();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_body_string("Rate limited"))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::with_base_url(&server.uri(), Some("key".to_owned()));
    let result = provider.stream(simple_request("gpt-4o")).await;
    let err_msg = result.err().expect("expected error").to_string();
    assert!(err_msg.contains("429"), "expected 429: {err_msg}");
}

#[tokio::test]
async fn http_error_500() {
    init_tls();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal error"))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::with_base_url(&server.uri(), Some("key".to_owned()));
    let result = provider.stream(simple_request("gpt-4o")).await;
    let err_msg = result.err().expect("expected error").to_string();
    assert!(err_msg.contains("500"), "expected 500: {err_msg}");
}

// ---------------------------------------------------------------------------
// strip_think_tags tests (unit tests, no wiremock)
// ---------------------------------------------------------------------------

#[test]
fn strip_think_tags_removes_complete_block() {
    let input = "Before<think>thinking stuff</think>After";
    assert_eq!(strip_think_tags_pub(input), "BeforeAfter");
}

#[test]
fn strip_think_tags_removes_multiple_blocks() {
    let input = "<think>first</think>middle<think>second</think>end";
    assert_eq!(strip_think_tags_pub(input), "middleend");
}

#[test]
fn strip_think_tags_handles_unclosed_tag() {
    let input = "Start<think>partial thinking without close";
    assert_eq!(strip_think_tags_pub(input), "Start");
}

#[test]
fn strip_think_tags_removes_lone_close_tag() {
    let input = "</think>content after";
    assert_eq!(strip_think_tags_pub(input), "content after");
}

#[test]
fn strip_think_tags_no_tags_unchanged() {
    let input = "Hello world, no tags here";
    assert_eq!(strip_think_tags_pub(input), input);
}

#[test]
fn strip_think_tags_empty_string() {
    assert_eq!(strip_think_tags_pub(""), "");
}
