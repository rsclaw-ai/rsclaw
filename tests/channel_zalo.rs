//! Integration tests for `ZaloChannel`.

use std::sync::Arc;

use rsclaw::channel::{Channel, OutboundMessage};
use rsclaw::channel::zalo::ZaloChannel;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn init_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn noop_on_message() -> Arc<
    dyn Fn(String, String, Vec<rsclaw::agent::registry::ImageAttachment>) + Send + Sync,
> {
    Arc::new(|_, _, _| {})
}

fn make_channel(base_url: &str) -> ZaloChannel {
    init_crypto();
    ZaloChannel::with_api_base("test-access-token", Some(base_url.to_owned()), noop_on_message())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn channel_name_is_zalo() {
    init_crypto();
    let ch = ZaloChannel::new("tok", noop_on_message());
    assert_eq!(ch.name(), "zalo");
}

#[tokio::test]
async fn send_text_posts_to_message_cs() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/message/cs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"error": 0})))
        .expect(1)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let msg = OutboundMessage {
        target_id: "Z12345".to_owned(),
        is_group: false,
        text: "Hello, Zalo!".to_owned(),
        reply_to: None,
        images: vec![],
    };

    ch.send(msg).await.expect("send should succeed");
}

#[tokio::test]
async fn send_chunked_2000() {
    let server = MockServer::start().await;

    // Text longer than 2000 chars should be split into 2 chunks.
    Mock::given(method("POST"))
        .and(path("/message/cs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"error": 0})))
        .expect(2)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let long_text = "F".repeat(2000 + 100);
    let msg = OutboundMessage {
        target_id: "Z12345".to_owned(),
        is_group: false,
        text: long_text,
        reply_to: None,
        images: vec![],
    };

    ch.send(msg).await.expect("send should succeed");
}

#[tokio::test]
async fn webhook_text_dispatches_to_callback() {
    use std::sync::Mutex;
    init_crypto();

    let received: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(vec![]));
    let rx = Arc::clone(&received);

    let ch = ZaloChannel::new(
        "token",
        Arc::new(move |from, text, _images| {
            rx.lock().expect("lock").push((from, text));
        }),
    );

    let body = r#"{
        "event_name": "user_send_text",
        "sender": { "id": "Z12345" },
        "message": { "text": "xin chao", "msg_id": "m1" }
    }"#;

    ch.handle_webhook(body).await.expect("webhook should succeed");

    let msgs = received.lock().expect("lock");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].0, "Z12345");
    assert_eq!(msgs[0].1, "xin chao");
}

#[tokio::test]
async fn webhook_empty_sender_is_ignored() {
    use std::sync::Mutex;
    init_crypto();

    let received: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(vec![]));
    let rx = Arc::clone(&received);

    let ch = ZaloChannel::new(
        "token",
        Arc::new(move |from, text, _images| {
            rx.lock().expect("lock").push((from, text));
        }),
    );

    let body = r#"{
        "event_name": "user_send_text",
        "sender": { "id": "" },
        "message": { "text": "should be ignored", "msg_id": "m2" }
    }"#;

    ch.handle_webhook(body).await.expect("webhook should succeed");

    let msgs = received.lock().expect("lock");
    assert_eq!(msgs.len(), 0, "empty sender should be ignored");
}

#[tokio::test]
async fn send_uses_access_token_header() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/message/cs"))
        .and(wiremock::matchers::header("access_token", "test-access-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"error": 0})))
        .expect(1)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let msg = OutboundMessage {
        target_id: "Z12345".to_owned(),
        is_group: false,
        text: "Auth check".to_owned(),
        reply_to: None,
        images: vec![],
    };

    ch.send(msg).await.expect("send should succeed with access_token header");
}

#[tokio::test]
async fn http_error_returns_err() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/message/cs"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Server Error"))
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let msg = OutboundMessage {
        target_id: "Z12345".to_owned(),
        is_group: false,
        text: "Hello".to_owned(),
        reply_to: None,
        images: vec![],
    };

    let result = ch.send(msg).await;
    assert!(result.is_err(), "should fail on HTTP 500");
}

#[tokio::test]
async fn webhook_unknown_event_is_skipped() {
    use std::sync::Mutex;
    init_crypto();

    let received: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(vec![]));
    let rx = Arc::clone(&received);

    let ch = ZaloChannel::new(
        "token",
        Arc::new(move |from, text, _images| {
            rx.lock().expect("lock").push((from, text));
        }),
    );

    let body = r#"{
        "event_name": "user_follow_oa",
        "sender": { "id": "Z99999" }
    }"#;

    ch.handle_webhook(body).await.expect("webhook should succeed");

    let msgs = received.lock().expect("lock");
    assert_eq!(msgs.len(), 0, "unknown event should be skipped");
}
