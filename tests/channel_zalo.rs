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
        ..Default::default()
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
        ..Default::default()
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
        ..Default::default()
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
        ..Default::default()
    };

    let result = ch.send(msg).await;
    assert!(result.is_err(), "should fail on HTTP 500");
}

#[tokio::test]
async fn send_image_uploads_then_sends_attachment() {
    let server = MockServer::start().await;

    // Text precedes images.
    Mock::given(method("POST"))
        .and(path("/message/cs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"error": 0})))
        .expect(2)
        .mount(&server)
        .await;

    // Multipart upload returns attachment_id.
    Mock::given(method("POST"))
        .and(path("/upload/image"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "attachment_id": "att_42" }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());

    use base64::Engine;
    let fake_png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    let b64 = base64::engine::general_purpose::STANDARD.encode(&fake_png);
    let data_uri = format!("data:image/png;base64,{b64}");

    let msg = OutboundMessage {
        target_id: "Z12345".to_owned(),
        is_group: false,
        text: "Here's an image".to_owned(),
        reply_to: None,
        images: vec![data_uri],
        ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed");
}

#[tokio::test]
async fn webhook_image_downloads_and_dispatches() {
    use std::sync::Mutex;
    init_crypto();

    let server = MockServer::start().await;

    let fake_jpg: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
    Mock::given(method("GET"))
        .and(path("/zalo-cdn/photo.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fake_jpg.clone()))
        .expect(1)
        .mount(&server)
        .await;

    let received: Arc<Mutex<Vec<(String, String, usize)>>> = Arc::new(Mutex::new(vec![]));
    let rx = Arc::clone(&received);

    let ch = ZaloChannel::with_api_base(
        "tok",
        Some(server.uri()),
        Arc::new(move |from, text, images| {
            rx.lock().expect("lock").push((from, text, images.len()));
        }),
    );

    let body = format!(
        r#"{{
            "event_name": "user_send_image",
            "sender": {{ "id": "Z77777" }},
            "message": {{ "url": "{}/zalo-cdn/photo.jpg", "msg_id": "img1" }}
        }}"#,
        server.uri()
    );

    ch.handle_webhook(&body).await.expect("webhook should succeed");

    let msgs = received.lock().expect("lock");
    assert_eq!(msgs.len(), 1, "image webhook should dispatch once");
    assert_eq!(msgs[0].0, "Z77777");
    assert_eq!(msgs[0].2, 1, "should attach exactly one image");
}

#[tokio::test]
async fn webhook_image_via_attachments_array() {
    use std::sync::Mutex;
    init_crypto();

    let server = MockServer::start().await;

    let fake_png: Vec<u8> = vec![0x89, 0x50, 0x4E, 0x47];
    Mock::given(method("GET"))
        .and(path("/zalo-cdn/att.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fake_png.clone()))
        .expect(1)
        .mount(&server)
        .await;

    let received: Arc<Mutex<Vec<(String, usize)>>> = Arc::new(Mutex::new(vec![]));
    let rx = Arc::clone(&received);

    let ch = ZaloChannel::with_api_base(
        "tok",
        Some(server.uri()),
        Arc::new(move |from, _text, images| {
            rx.lock().expect("lock").push((from, images.len()));
        }),
    );

    let body = format!(
        r#"{{
            "event_name": "user_send_image",
            "sender": {{ "id": "Z88888" }},
            "message": {{
                "msg_id": "img2",
                "attachments": [
                    {{ "type": "photo", "payload": {{ "url": "{}/zalo-cdn/att.png" }} }}
                ]
            }}
        }}"#,
        server.uri()
    );

    ch.handle_webhook(&body).await.expect("webhook should succeed");

    let msgs = received.lock().expect("lock");
    assert_eq!(msgs.len(), 1, "image-via-attachments should dispatch once");
    assert_eq!(msgs[0].1, 1);
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
