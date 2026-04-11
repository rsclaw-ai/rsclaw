//! Wiremock integration tests for GeminiProvider SSE parsing.

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
    gemini::GeminiProvider,
};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::mock_provider::{
    GeminiEvent, assert_stream_done, assert_stream_text, assert_stream_tool_call, assert_usage,
    collect_stream_events, mount_gemini_stream,
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
async fn stream_text_parts() {
    init_tls();
    let server = MockServer::start().await;
    mount_gemini_stream(
        &server,
        "gemini-2.0-flash",
        &[
            GeminiEvent::Text("Hello".to_owned()),
            GeminiEvent::Text(" world".to_owned()),
            GeminiEvent::Finish {
                prompt_tokens: 10,
                candidates_tokens: 5,
            },
        ],
    )
    .await;

    let provider = GeminiProvider::with_base_url("test-api-key", &format!("{}/v1beta", server.uri()));
    let stream = provider
        .stream(simple_request("gemini-2.0-flash"))
        .await
        .unwrap();
    let events = collect_stream_events(stream).await;

    assert_stream_text(&events, "Hello world");
    assert_stream_done(&events);
    assert_usage(&events, 10, 5);
}

#[tokio::test]
async fn stream_function_call() {
    init_tls();
    let server = MockServer::start().await;
    mount_gemini_stream(
        &server,
        "gemini-2.0-flash",
        &[
            GeminiEvent::FunctionCall {
                name: "search".to_owned(),
                args: serde_json::json!({"query": "test"}),
            },
            GeminiEvent::Finish {
                prompt_tokens: 15,
                candidates_tokens: 8,
            },
        ],
    )
    .await;

    let provider = GeminiProvider::with_base_url("test-key", &format!("{}/v1beta", server.uri()));
    let stream = provider
        .stream(simple_request("gemini-2.0-flash"))
        .await
        .unwrap();
    let events = collect_stream_events(stream).await;

    assert_stream_tool_call(&events, "search");
    // Verify the input args
    let tool_call = events.iter().find_map(|e| match e {
        StreamEvent::ToolCall { name, input, .. } if name == "search" => Some(input.clone()),
        _ => None,
    });
    assert!(tool_call.is_some());
    assert_eq!(tool_call.unwrap()["query"], "test");
}

#[tokio::test]
async fn stream_error_field() {
    init_tls();
    let server = MockServer::start().await;
    mount_gemini_stream(
        &server,
        "gemini-2.0-flash",
        &[GeminiEvent::Error("quota exceeded".to_owned())],
    )
    .await;

    let provider = GeminiProvider::with_base_url("test-key", &format!("{}/v1beta", server.uri()));
    let stream = provider
        .stream(simple_request("gemini-2.0-flash"))
        .await
        .unwrap();
    let events = collect_stream_events(stream).await;

    let has_error = events.iter().any(|e| {
        matches!(e, StreamEvent::Error(msg) if msg.contains("quota exceeded"))
    });
    assert!(has_error, "expected Error event with 'quota exceeded'");
}

// ---------------------------------------------------------------------------
// Request URL and auth tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn request_url_includes_model() {
    init_tls();
    let server = MockServer::start().await;

    // Mount on the model-specific path
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.0-flash:streamGenerateContent"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(
                    "data: {\"candidates\":[{\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":1}}\n\n",
                )
                .insert_header("content-type", "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = GeminiProvider::with_base_url("key123", &format!("{}/v1beta", server.uri()));
    let result = provider.stream(simple_request("gemini-2.0-flash")).await;
    assert!(
        result.is_ok(),
        "request should hit the model-specific path: {:?}",
        result.err()
    );
}

#[tokio::test]
async fn api_key_in_query_params() {
    init_tls();
    let server = MockServer::start().await;

    // Match on the API key query parameter
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.0-flash:streamGenerateContent"))
        .and(query_param("key", "my-secret-gemini-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(
                    "data: {\"candidates\":[{\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":1}}\n\n",
                )
                .insert_header("content-type", "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        GeminiProvider::with_base_url("my-secret-gemini-key", &format!("{}/v1beta", server.uri()));
    let result = provider.stream(simple_request("gemini-2.0-flash")).await;
    assert!(
        result.is_ok(),
        "request should include API key in query params: {:?}",
        result.err()
    );
}

// ---------------------------------------------------------------------------
// HTTP error tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gemini_http_error() {
    init_tls();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1beta/models/gemini-2.0-flash:streamGenerateContent"))
        .respond_with(
            ResponseTemplate::new(403).set_body_string("API key invalid"),
        )
        .mount(&server)
        .await;

    let provider = GeminiProvider::with_base_url("bad-key", &format!("{}/v1beta", server.uri()));
    let result = provider.stream(simple_request("gemini-2.0-flash")).await;
    let err_msg = result.err().expect("expected error").to_string();
    assert!(err_msg.contains("403"), "expected 403 in error: {err_msg}");
}
