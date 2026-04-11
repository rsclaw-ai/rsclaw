//! Integration tests for `WeComChannel`.

use std::sync::Arc;

use rsclaw::channel::wecom::WeComChannel;
use rsclaw::channel::Channel;

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn channel_name_is_wecom() {
    init_crypto();
    let ch = WeComChannel::new("bot_id", "bot_secret", None, noop_on_message());
    assert_eq!(ch.name(), "wecom");
}

/// When no ws_url is given, the channel uses the default WeCom WS endpoint.
/// We cannot inspect the private field directly, but we can verify the channel
/// constructs without error using `None`.
#[test]
fn default_ws_url_accepted() {
    init_crypto();
    let ch = WeComChannel::new("id", "secret", None, noop_on_message());
    // Just verify the channel object is valid and has the right name.
    assert_eq!(ch.name(), "wecom");
}

/// When a custom ws_url is given, the channel should accept it without error.
#[test]
fn custom_ws_url_accepted() {
    init_crypto();
    let ch = WeComChannel::new(
        "id",
        "secret",
        Some("wss://custom.example.com/ws".to_owned()),
        noop_on_message(),
    );
    assert_eq!(ch.name(), "wecom");
}

/// Sending a message when the WebSocket is not connected should still not
/// panic -- the send path uses an mpsc channel that will fail gracefully.
#[tokio::test]
async fn send_without_ws_connection_does_not_panic() {
    init_crypto();
    let ch = WeComChannel::new("id", "secret", None, noop_on_message());

    let result = ch
        .send(rsclaw::channel::OutboundMessage {
            target_id: "user_123".to_owned(),
            is_group: false,
            text: "test message".to_owned(),
            reply_to: None,
            images: vec![],
        ..Default::default()
        })
        .await;

    // The send may fail (WS not connected), but it should not panic.
    // Whether it succeeds or fails depends on internal mpsc state.
    let _ = result;
}
