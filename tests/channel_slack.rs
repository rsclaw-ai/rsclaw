//! Integration tests for `SlackChannel`.

use std::sync::Arc;

use rsclaw::channel::{Channel, OutboundMessage};
use rsclaw::channel::slack::SlackChannel;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn init_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn noop_on_message() -> Arc<dyn Fn(String, String, String, bool) + Send + Sync> {
    Arc::new(|_, _, _, _| {})
}

fn make_channel(base_url: &str) -> SlackChannel {
    init_crypto();
    SlackChannel::new("xoxb-test-token", None, Some(base_url.to_owned()), noop_on_message())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn channel_name_is_slack() {
    init_crypto();
    let ch = SlackChannel::new("xoxb-tok", None, None, noop_on_message());
    assert_eq!(ch.name(), "slack");
}

#[tokio::test]
async fn send_text_posts_to_chat_post_message() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat.postMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok": true})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let msg = OutboundMessage {
        target_id: "C12345".to_owned(),
        is_group: false,
        text: "Hello, Slack!".to_owned(),
        reply_to: None,
        images: vec![],
    ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed");
}

#[tokio::test]
async fn send_chunked_3000() {
    let server = MockServer::start().await;

    // Text longer than 3000 chars should be split into 2 chunks.
    Mock::given(method("POST"))
        .and(path("/chat.postMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok": true})),
        )
        .expect(2)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let long_text = "C".repeat(3000 + 100);
    let msg = OutboundMessage {
        target_id: "C12345".to_owned(),
        is_group: false,
        text: long_text,
        reply_to: None,
        images: vec![],
    ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed");
}

#[tokio::test]
async fn slack_api_error_returns_err() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat.postMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok": false, "error": "channel_not_found"})),
        )
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let msg = OutboundMessage {
        target_id: "C12345".to_owned(),
        is_group: false,
        text: "Hello".to_owned(),
        reply_to: None,
        images: vec![],
    ..Default::default()
    };

    let result = ch.send(msg).await;
    assert!(result.is_err(), "should fail when Slack returns ok=false");
}

#[tokio::test]
async fn send_uses_bearer_auth() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat.postMessage"))
        .and(wiremock::matchers::header("authorization", "Bearer xoxb-test-token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok": true})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let msg = OutboundMessage {
        target_id: "C12345".to_owned(),
        is_group: false,
        text: "Auth check".to_owned(),
        reply_to: None,
        images: vec![],
    ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed with bearer auth");
}
