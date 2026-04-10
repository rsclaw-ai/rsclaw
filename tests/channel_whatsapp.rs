//! Integration tests for `WhatsAppChannel`.

use std::sync::Arc;

use rsclaw::channel::{Channel, OutboundMessage};
use rsclaw::channel::whatsapp::{
    WhatsAppChannel, WebhookPayload, WhatsAppEntry, WhatsAppChange,
    WhatsAppValue, WhatsAppMessage, WhatsAppText,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path_regex},
};

fn init_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn noop_on_message() -> Arc<
    dyn Fn(String, String, Vec<rsclaw::agent::registry::ImageAttachment>) + Send + Sync,
> {
    Arc::new(|_, _, _| {})
}

fn make_channel(base_url: &str) -> WhatsAppChannel {
    init_crypto();
    WhatsAppChannel::with_api_base("phone123", "access-tok", Some(base_url.to_owned()), noop_on_message())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn channel_name_is_whatsapp() {
    init_crypto();
    let ch = WhatsAppChannel::new("123", "token", noop_on_message());
    assert_eq!(ch.name(), "whatsapp");
}

#[tokio::test]
async fn send_text_posts_to_messages_endpoint() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex(r"/phone123/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"messages": [{"id": "wamid.123"}]})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let msg = OutboundMessage {
        target_id: "447911123456".to_owned(),
        is_group: false,
        text: "Hello, WhatsApp!".to_owned(),
        reply_to: None,
        images: vec![],
    ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed");
}

#[tokio::test]
async fn send_chunked_4000() {
    let server = MockServer::start().await;

    // Text longer than 4000 chars should be split into 2 chunks.
    Mock::given(method("POST"))
        .and(path_regex(r"/phone123/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"messages": [{"id": "wamid.123"}]})),
        )
        .expect(2)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let long_text = "D".repeat(4000 + 100);
    let msg = OutboundMessage {
        target_id: "447911123456".to_owned(),
        is_group: false,
        text: long_text,
        reply_to: None,
        images: vec![],
    ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed");
}

#[tokio::test]
async fn send_image_uploads_then_sends() {
    let server = MockServer::start().await;

    // Text message
    Mock::given(method("POST"))
        .and(path_regex(r"/phone123/messages$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"messages": [{"id": "wamid.123"}]})),
        )
        .mount(&server)
        .await;

    // Media upload
    Mock::given(method("POST"))
        .and(path_regex(r"/phone123/media"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"id": "media_id_1"})),
        )
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());

    use base64::Engine;
    let fake_png = vec![0x89, 0x50, 0x4E, 0x47];
    let b64 = base64::engine::general_purpose::STANDARD.encode(&fake_png);
    let data_uri = format!("data:image/png;base64,{b64}");

    let msg = OutboundMessage {
        target_id: "447911123456".to_owned(),
        is_group: false,
        text: "Check this image".to_owned(),
        reply_to: None,
        images: vec![data_uri],
    ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed");
}

#[tokio::test]
async fn webhook_text_dispatches_to_callback() {
    use std::sync::Mutex;

    let received: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(vec![]));
    let rx = Arc::clone(&received);

    init_crypto();
    let ch = WhatsAppChannel::new(
        "phone123",
        "access-tok",
        Arc::new(move |from, text, _images| {
            rx.lock().expect("lock").push((from, text));
        }),
    );

    let payload = WebhookPayload {
        entry: vec![WhatsAppEntry {
            changes: vec![WhatsAppChange {
                value: WhatsAppValue {
                    messages: Some(vec![WhatsAppMessage {
                        from: "447911123456".to_owned(),
                        id: "wamid.xxx".to_owned(),
                        kind: "text".to_owned(),
                        text: Some(WhatsAppText {
                            body: "hello from whatsapp".to_owned(),
                        }),
                        image: None,
                        audio: None,
                        video: None,
                        document: None,
                    }]),
                },
            }],
        }],
    };

    ch.handle_webhook(&payload).await;

    let msgs = received.lock().expect("lock");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].0, "447911123456");
    assert_eq!(msgs[0].1, "hello from whatsapp");
}

#[tokio::test]
async fn http_error_returns_err() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex(r"/phone123/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Server Error"))
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());
    let msg = OutboundMessage {
        target_id: "447911123456".to_owned(),
        is_group: false,
        text: "Hello".to_owned(),
        reply_to: None,
        images: vec![],
    ..Default::default()
    };

    let result = ch.send(msg).await;
    assert!(result.is_err(), "should fail on HTTP 500");
}
