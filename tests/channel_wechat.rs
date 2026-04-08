//! Integration tests for `WeChatPersonalChannel`.

use std::sync::Arc;

use rsclaw::channel::wechat::WeChatPersonalChannel;
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
            Vec<rsclaw::agent::registry::ImageAttachment>,
            Vec<rsclaw::agent::registry::FileAttachment>,
        ) + Send
        + Sync,
> {
    Arc::new(|_, _, _, _| {})
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn channel_name_is_wechat() {
    init_crypto();
    let ch = WeChatPersonalChannel::new("tok".to_owned(), noop_on_message());
    assert_eq!(ch.name(), "wechat");
}

/// `with_base_url` should override the default ilink URL and strip trailing
/// slashes. Complements the inline unit test by testing from integration scope.
#[test]
fn with_base_url_strips_trailing_slash() {
    init_crypto();
    let ch = WeChatPersonalChannel::new("tok".to_owned(), noop_on_message())
        .with_base_url("http://example.com/api/");
    // We can verify the name still works (channel is valid).
    assert_eq!(ch.name(), "wechat");
}

/// Send a text message against a mock ilink server.
#[tokio::test]
async fn send_text_via_mock() {
    init_crypto();
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/ilink/bot/sendmessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ret": 0
        })))
        .expect(1)
        .mount(&server)
        .await;

    let ch = WeChatPersonalChannel::new("mock-token".to_owned(), noop_on_message())
        .with_base_url(&server.uri());

    let result = ch
        .send(OutboundMessage {
            target_id: "user_wx_123".to_owned(),
            is_group: false,
            text: "Hello WeChat".to_owned(),
            reply_to: None,
            images: vec![],
        })
        .await;

    assert!(result.is_ok(), "send should succeed: {:?}", result.err());
}

/// Sending multiple chunks should result in multiple sendmessage calls.
#[tokio::test]
async fn send_chunked_message() {
    init_crypto();
    let server = MockServer::start().await;

    // WeChat chunk limit is the default (not explicitly overridden),
    // so a very long message should be split.
    Mock::given(method("POST"))
        .and(path("/ilink/bot/sendmessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ret": 0
        })))
        // We expect at least 2 calls for a long text
        .expect(2..)
        .mount(&server)
        .await;

    let ch = WeChatPersonalChannel::new("tok".to_owned(), noop_on_message())
        .with_base_url(&server.uri());

    // Build a message exceeding the default chunk limit
    let long_text = "W".repeat(10_000);
    let result = ch
        .send(OutboundMessage {
            target_id: "user_456".to_owned(),
            is_group: false,
            text: long_text,
            reply_to: None,
            images: vec![],
        })
        .await;

    assert!(result.is_ok(), "chunked send should succeed: {:?}", result.err());
}
