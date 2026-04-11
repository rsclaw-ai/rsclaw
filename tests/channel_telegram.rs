//! Integration tests for `TelegramChannel`.

use std::sync::Arc;

use rsclaw::channel::{Channel, OutboundMessage};
use rsclaw::channel::telegram::TelegramChannel;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path_regex},
};

fn init_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn noop_on_message() -> Arc<
    dyn Fn(i64, String, i64, bool, Option<i64>, Vec<rsclaw::agent::registry::ImageAttachment>, Vec<rsclaw::agent::registry::FileAttachment>)
        + Send
        + Sync,
> {
    Arc::new(|_, _, _, _, _, _, _| {})
}

fn make_channel(base_url: &str) -> TelegramChannel {
    init_crypto();
    TelegramChannel::new("test-token", Some(base_url.to_owned()), noop_on_message())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn channel_name_is_telegram() {
    init_crypto();
    let ch = TelegramChannel::new("tok", None, noop_on_message());
    assert_eq!(ch.name(), "telegram");
}

#[tokio::test]
async fn send_text_posts_to_send_message() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex(r"/bottest-token/sendMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok": true, "result": {"message_id": 1}})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let msg = OutboundMessage {
        target_id: "12345".to_owned(),
        is_group: false,
        text: "Hello, Telegram!".to_owned(),
        reply_to: None,
        images: vec![],
    ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed");
}

#[tokio::test]
async fn send_chunked_4096() {
    let server = MockServer::start().await;

    // A message longer than 4096 chars should be split into 2 chunks.
    Mock::given(method("POST"))
        .and(path_regex(r"/bottest-token/sendMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok": true, "result": {"message_id": 1}})),
        )
        .expect(2)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let long_text = "A".repeat(4096 + 100);
    let msg = OutboundMessage {
        target_id: "12345".to_owned(),
        is_group: false,
        text: long_text,
        reply_to: None,
        images: vec![],
    ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed");
}

#[tokio::test]
async fn send_image_multipart() {
    let server = MockServer::start().await;

    // sendMessage for text
    Mock::given(method("POST"))
        .and(path_regex(r"/bottest-token/sendMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok": true, "result": {"message_id": 1}})),
        )
        .mount(&server)
        .await;

    // sendPhoto for image
    Mock::given(method("POST"))
        .and(path_regex(r"/bottest-token/sendPhoto"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok": true, "result": {"message_id": 2}})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());

    // Create a small valid base64 PNG-like payload.
    use base64::Engine;
    let fake_png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let b64 = base64::engine::general_purpose::STANDARD.encode(&fake_png);
    let data_uri = format!("data:image/png;base64,{b64}");

    let msg = OutboundMessage {
        target_id: "12345".to_owned(),
        is_group: false,
        text: "Check this image".to_owned(),
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
        .and(path_regex(r"/bottest-token/sendMessage"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let msg = OutboundMessage {
        target_id: "12345".to_owned(),
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
async fn send_with_reply_to() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex(r"/bottest-token/sendMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok": true, "result": {"message_id": 1}})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let msg = OutboundMessage {
        target_id: "12345".to_owned(),
        is_group: false,
        text: "Reply text".to_owned(),
        reply_to: Some("42".to_owned()),
        images: vec![],
    ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed");
}
