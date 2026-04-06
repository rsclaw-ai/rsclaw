//! Integration tests for `FeishuChannel`.

use std::sync::Arc;

use rsclaw::channel::feishu::FeishuChannel;
use rsclaw::channel::{Channel, OutboundMessage};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn init_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn noop_on_message() -> Arc<
    dyn Fn(
            String,
            String,
            String,
            bool,
            Vec<rsclaw::agent::registry::ImageAttachment>,
            Vec<rsclaw::agent::registry::FileAttachment>,
        ) + Send
        + Sync,
> {
    Arc::new(|_, _, _, _, _, _| {})
}

fn make_channel(api_base: &str) -> FeishuChannel {
    let mut ch = FeishuChannel::new("app_id", "app_secret", vec![], noop_on_message());
    ch.api_base_override = Some(api_base.to_owned());
    ch
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn channel_name_is_feishu() {
    init_crypto();
    let ch = FeishuChannel::new("id", "secret", vec![], noop_on_message());
    assert_eq!(ch.name(), "feishu");
}

#[test]
fn default_brand_is_feishu() {
    init_crypto();
    let ch = FeishuChannel::new("id", "secret", vec![], noop_on_message());
    assert_eq!(ch.brand, "feishu");
}

/// Brand can be changed to "lark" after construction.
#[test]
fn brand_can_be_set_to_lark() {
    init_crypto();
    let mut ch = FeishuChannel::new("id", "secret", vec![], noop_on_message());
    ch.brand = "lark".to_owned();
    assert_eq!(ch.brand, "lark");
}

/// Test tenant token refresh via mock, then sending a text message.
#[tokio::test]
async fn tenant_token_refresh_and_send_text() {
    init_crypto();
    let server = MockServer::start().await;

    // Mock token endpoint
    Mock::given(method("POST"))
        .and(path("/auth/v3/tenant_access_token/internal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "msg": "ok",
            "tenant_access_token": "t-mock-token",
            "expire": 7200
        })))
        .expect(1)
        .mount(&server)
        .await;

    // Mock send message endpoint
    Mock::given(method("POST"))
        .and(path("/im/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "msg": "ok",
            "data": { "message_id": "om_mock" }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());

    let result = ch
        .send(OutboundMessage {
            target_id: "oc_chat_123".to_owned(),
            is_group: false,
            text: "Hello Feishu".to_owned(),
            reply_to: None,
            images: vec![],
        })
        .await;

    assert!(result.is_ok(), "send should succeed: {:?}", result.err());
}

/// Text exceeding 4000 chars should be chunked into multiple sends.
#[tokio::test]
async fn send_chunked_4000() {
    init_crypto();
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/auth/v3/tenant_access_token/internal"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "msg": "ok",
            "tenant_access_token": "t-tok",
            "expire": 7200
        })))
        .mount(&server)
        .await;

    // Expect 2 message sends for a 6000-char message (4000 limit)
    Mock::given(method("POST"))
        .and(path("/im/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "msg": "ok",
            "data": { "message_id": "om_x" }
        })))
        .expect(2)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());

    let long_text = "B".repeat(6_000);
    let result = ch
        .send(OutboundMessage {
            target_id: "oc_chat_456".to_owned(),
            is_group: false,
            text: long_text,
            reply_to: None,
            images: vec![],
        })
        .await;

    assert!(result.is_ok(), "chunked send should succeed: {:?}", result.err());
}

/// Verify the api_base_override is honoured (not hitting real Feishu).
#[test]
fn api_base_override_applied() {
    init_crypto();
    let mut ch = FeishuChannel::new("id", "secret", vec![], noop_on_message());
    ch.api_base_override = Some("http://localhost:9999".to_owned());
    assert_eq!(ch.api_base_override.as_deref(), Some("http://localhost:9999"));
}

/// WS URL override is honoured.
#[test]
fn ws_url_override_applied() {
    init_crypto();
    let mut ch = FeishuChannel::new("id", "secret", vec![], noop_on_message());
    ch.ws_url_override = Some("ws://localhost:8080".to_owned());
    assert_eq!(ch.ws_url_override.as_deref(), Some("ws://localhost:8080"));
}
