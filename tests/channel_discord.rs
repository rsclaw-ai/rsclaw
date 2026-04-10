//! Integration tests for `DiscordChannel`.

use std::sync::Arc;

use rsclaw::channel::{Channel, OutboundMessage};
use rsclaw::channel::discord::DiscordChannel;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path_regex},
};

fn init_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn noop_on_message() -> Arc<dyn Fn(String, String, String, bool) + Send + Sync> {
    Arc::new(|_, _, _, _| {})
}

fn make_channel(base_url: &str) -> DiscordChannel {
    init_crypto();
    DiscordChannel::new("test-token", false, noop_on_message(), Some(base_url.to_owned()), None)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn channel_name_is_discord() {
    init_crypto();
    let ch = DiscordChannel::new("tok", false, noop_on_message(), None, None);
    assert_eq!(ch.name(), "discord");
}

#[tokio::test]
async fn send_text_posts_to_channel_messages() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex(r"/channels/chan123/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"id": "msg_1"})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let msg = OutboundMessage {
        target_id: "chan123".to_owned(),
        is_group: false,
        text: "Hello, Discord!".to_owned(),
        reply_to: None,
        images: vec![],
    ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed");
}

#[tokio::test]
async fn send_chunked_2000() {
    let server = MockServer::start().await;

    // A message longer than 2000 chars should be split into 2 chunks.
    Mock::given(method("POST"))
        .and(path_regex(r"/channels/chan123/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"id": "msg_1"})),
        )
        .expect(2)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let long_text = "B".repeat(2000 + 100);
    let msg = OutboundMessage {
        target_id: "chan123".to_owned(),
        is_group: false,
        text: long_text,
        reply_to: None,
        images: vec![],
    ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed");
}

#[tokio::test]
async fn send_image_uploads_multipart() {
    let server = MockServer::start().await;

    // Text chunk
    Mock::given(method("POST"))
        .and(path_regex(r"/channels/chan123/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"id": "msg_1"})),
        )
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());

    use base64::Engine;
    let fake_png = vec![0x89, 0x50, 0x4E, 0x47];
    let b64 = base64::engine::general_purpose::STANDARD.encode(&fake_png);
    let data_uri = format!("data:image/png;base64,{b64}");

    let msg = OutboundMessage {
        target_id: "chan123".to_owned(),
        is_group: false,
        text: "Image test".to_owned(),
        reply_to: None,
        images: vec![data_uri],
    ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed");
}

#[tokio::test]
async fn http_error_returns_err() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex(r"/channels/chan123/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let msg = OutboundMessage {
        target_id: "chan123".to_owned(),
        is_group: false,
        text: "Hello".to_owned(),
        reply_to: None,
        images: vec![],
    ..Default::default()
    };

    let result = ch.send(msg).await;
    assert!(result.is_err(), "should fail on HTTP 500");
}

#[tokio::test]
async fn auth_header_includes_bot_prefix() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex(r"/channels/chan123/messages"))
        .and(wiremock::matchers::header("authorization", "Bot test-token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"id": "msg_1"})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let msg = OutboundMessage {
        target_id: "chan123".to_owned(),
        is_group: false,
        text: "Auth check".to_owned(),
        reply_to: None,
        images: vec![],
    ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed with Bot auth header");
}
