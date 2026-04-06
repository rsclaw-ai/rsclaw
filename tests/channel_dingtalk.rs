//! Integration tests for `DingTalkChannel`.

use std::sync::Arc;

use rsclaw::channel::dingtalk::DingTalkChannel;
use rsclaw::channel::{Channel, OutboundMessage};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn init_crypto() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn noop_on_message() -> Arc<
    dyn Fn(String, String, String, bool, Vec<rsclaw::agent::registry::ImageAttachment>)
        + Send
        + Sync,
> {
    Arc::new(|_, _, _, _, _| {})
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn channel_name_is_dingtalk() {
    init_crypto();
    let ch = DingTalkChannel::new(
        "key",
        "secret",
        "robot_code",
        None,
        None,
        noop_on_message(),
    );
    assert_eq!(ch.name(), "dingtalk");
}

/// Token refresh: mock the /gettoken endpoint, then verify send_text_to_user
/// calls the right API with a valid token.
#[tokio::test]
async fn token_refresh_and_send_to_user() {
    init_crypto();
    let server = MockServer::start().await;

    // Mock gettoken (OAPI base)
    Mock::given(method("GET"))
        .and(path("/gettoken"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "mock_token_123",
            "expires_in": 7200,
            "errcode": 0,
            "errmsg": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    // Mock robot batch-send (API base)
    Mock::given(method("POST"))
        .and(path("/v1.0/robot/oToMessages/batchSend"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "processQueryKey": "abc"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let ch = DingTalkChannel::new(
        "test_key",
        "test_secret",
        "test_robot",
        Some(server.uri()),
        Some(server.uri()),
        noop_on_message(),
    );

    let result = ch
        .send(OutboundMessage {
            target_id: "user_001".to_owned(),
            is_group: false,
            text: "Hello DingTalk".to_owned(),
            reply_to: None,
            images: vec![],
        })
        .await;

    assert!(result.is_ok(), "send should succeed: {:?}", result.err());
}

/// Verify that long text is chunked at the 20000-char boundary and results
/// in multiple API calls.
#[tokio::test]
async fn send_chunked_20000() {
    init_crypto();
    let server = MockServer::start().await;

    // Mock gettoken
    Mock::given(method("GET"))
        .and(path("/gettoken"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "tok",
            "expires_in": 7200,
            "errcode": 0,
        })))
        .mount(&server)
        .await;

    // Mock batch-send -- expect exactly 2 calls for a 25000-char message
    Mock::given(method("POST"))
        .and(path("/v1.0/robot/oToMessages/batchSend"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(2)
        .mount(&server)
        .await;

    let ch = DingTalkChannel::new(
        "k",
        "s",
        "r",
        Some(server.uri()),
        Some(server.uri()),
        noop_on_message(),
    );

    // Build a 25000-char message (exceeds the 20000 limit -> 2 chunks)
    let long_text = "A".repeat(25_000);
    let result = ch
        .send(OutboundMessage {
            target_id: "user_002".to_owned(),
            is_group: false,
            text: long_text,
            reply_to: None,
            images: vec![],
        })
        .await;

    assert!(result.is_ok(), "chunked send should succeed: {:?}", result.err());
}

/// Group messages should call groupMessages/send instead of batchSend.
#[tokio::test]
async fn send_to_group() {
    init_crypto();
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/gettoken"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "tok",
            "expires_in": 7200,
            "errcode": 0,
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1.0/robot/groupMessages/send"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;

    let ch = DingTalkChannel::new(
        "k",
        "s",
        "r",
        Some(server.uri()),
        Some(server.uri()),
        noop_on_message(),
    );

    let result = ch
        .send(OutboundMessage {
            target_id: "conv_123".to_owned(),
            is_group: true,
            text: "Group hello".to_owned(),
            reply_to: None,
            images: vec![],
        })
        .await;

    assert!(result.is_ok(), "group send should succeed: {:?}", result.err());
}
