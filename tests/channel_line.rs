//! Integration tests for `LineChannel`.

use std::sync::Arc;

use rsclaw::channel::{Channel, OutboundMessage};
use rsclaw::channel::line::LineChannel;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn init_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn noop_on_message() -> Arc<
    dyn Fn(String, String, bool, Vec<rsclaw::agent::registry::ImageAttachment>) + Send + Sync,
> {
    Arc::new(|_, _, _, _| {})
}

fn make_channel(base_url: &str) -> LineChannel {
    init_crypto();
    LineChannel::with_api_base("test-token", Some(base_url.to_owned()), noop_on_message())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn channel_name_is_line() {
    init_crypto();
    let ch = LineChannel::new("tok", noop_on_message());
    assert_eq!(ch.name(), "line");
}

#[tokio::test]
async fn send_text_push() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/message/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let msg = OutboundMessage {
        target_id: "U12345".to_owned(),
        is_group: false,
        text: "Hello, LINE!".to_owned(),
        reply_to: None,
        images: vec![],
    ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed");
}

#[tokio::test]
async fn send_chunked_5000() {
    let server = MockServer::start().await;

    // Text longer than 5000 chars should be split into 2 chunks.
    Mock::given(method("POST"))
        .and(path("/message/push"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(2)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let long_text = "E".repeat(5000 + 100);
    let msg = OutboundMessage {
        target_id: "U12345".to_owned(),
        is_group: false,
        text: long_text,
        reply_to: None,
        images: vec![],
    ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed");
}

#[tokio::test]
async fn webhook_text_dispatches_to_callback() {
    use std::sync::Mutex;

    init_crypto();
    let received: Arc<Mutex<Vec<(String, String, bool)>>> = Arc::new(Mutex::new(vec![]));
    let rx = Arc::clone(&received);

    let ch = LineChannel::new(
        "token",
        Arc::new(move |from, text, is_group, _images| {
            rx.lock().expect("lock").push((from, text, is_group));
        }),
    );

    let body = r#"{
        "events": [{
            "type": "message",
            "replyToken": "abc",
            "source": { "type": "user", "userId": "U12345" },
            "message": { "type": "text", "id": "msg1", "text": "hello from LINE" }
        }]
    }"#;

    ch.handle_webhook(body).await.expect("webhook should succeed");

    let msgs = received.lock().expect("lock");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].0, "U12345");
    assert_eq!(msgs[0].1, "hello from LINE");
    assert!(!msgs[0].2);
}

#[tokio::test]
async fn webhook_group_message() {
    use std::sync::Mutex;
    init_crypto();

    let received: Arc<Mutex<Vec<(String, String, bool)>>> = Arc::new(Mutex::new(vec![]));
    let rx = Arc::clone(&received);

    let ch = LineChannel::new(
        "token",
        Arc::new(move |from, text, is_group, _images| {
            rx.lock().expect("lock").push((from, text, is_group));
        }),
    );

    let body = r#"{
        "events": [{
            "type": "message",
            "replyToken": "def",
            "source": { "type": "group", "userId": "U999", "groupId": "C888" },
            "message": { "type": "text", "id": "msg2", "text": "group msg" }
        }]
    }"#;

    ch.handle_webhook(body).await.expect("webhook should succeed");

    let msgs = received.lock().expect("lock");
    assert_eq!(msgs.len(), 1);
    assert!(msgs[0].2, "should be a group message");
}

#[tokio::test]
async fn send_push_uses_bearer_auth() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/message/push"))
        .and(wiremock::matchers::header("authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let msg = OutboundMessage {
        target_id: "U12345".to_owned(),
        is_group: false,
        text: "Auth check".to_owned(),
        reply_to: None,
        images: vec![],
    ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed with bearer auth");
}

#[tokio::test]
async fn http_error_returns_err() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/message/push"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Server Error"))
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let msg = OutboundMessage {
        target_id: "U12345".to_owned(),
        is_group: false,
        text: "Hello".to_owned(),
        reply_to: None,
        images: vec![],
    ..Default::default()
    };

    let result = ch.send(msg).await;
    assert!(result.is_err(), "should fail on HTTP 500");
}
