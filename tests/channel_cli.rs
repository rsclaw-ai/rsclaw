//! Integration tests for `CliChannel`.

use std::sync::Arc;

use rsclaw::channel::cli::{CliChannel, CLI_CHANNEL_NAME, CLI_PEER_ID};
use rsclaw::channel::{Channel, OutboundMessage};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Channel name constant and method should both return "cli".
#[test]
fn channel_name_is_cli() {
    let ch = CliChannel::new(Arc::new(|_, _| {}));
    assert_eq!(ch.name(), "cli");
    assert_eq!(ch.name(), CLI_CHANNEL_NAME);
}

/// CLI_PEER_ID should be "cli_user".
#[test]
fn peer_id_constant() {
    assert_eq!(CLI_PEER_ID, "cli_user");
}

/// Sending a plain text message should succeed (writes to stdout).
#[tokio::test]
async fn send_text_ok() {
    let ch = CliChannel::new(Arc::new(|_, _| {}));
    let result = ch
        .send(OutboundMessage {
            target_id: CLI_PEER_ID.to_owned(),
            is_group: false,
            text: "integration test output".to_owned(),
            reply_to: None,
            images: vec![],
            ..Default::default()
        })
        .await;
    assert!(result.is_ok(), "send should not error: {:?}", result.err());
}

/// Sending a message with images should still succeed -- images are silently
/// ignored (debug log only).
#[tokio::test]
async fn send_ignores_images() {
    let ch = CliChannel::new(Arc::new(|_, _| {}));
    let result = ch
        .send(OutboundMessage {
            target_id: CLI_PEER_ID.to_owned(),
            is_group: false,
            text: "text with image".to_owned(),
            reply_to: None,
            images: vec!["data:image/png;base64,iVBOR...".to_owned()],
        ..Default::default()
        })
        .await;
    assert!(result.is_ok(), "send with images should not error: {:?}", result.err());
}

/// Sending with reply_to set should still succeed.
#[tokio::test]
async fn send_with_reply_to() {
    let ch = CliChannel::new(Arc::new(|_, _| {}));
    let result = ch
        .send(OutboundMessage {
            target_id: CLI_PEER_ID.to_owned(),
            is_group: false,
            text: "replying".to_owned(),
            reply_to: Some("prev_msg_id".to_owned()),
            images: vec![],
            ..Default::default()
        })
        .await;
    assert!(result.is_ok());
}

/// Empty text should still send successfully (just a newline).
#[tokio::test]
async fn send_empty_text() {
    let ch = CliChannel::new(Arc::new(|_, _| {}));
    let result = ch
        .send(OutboundMessage {
            target_id: CLI_PEER_ID.to_owned(),
            is_group: false,
            text: String::new(),
            reply_to: None,
            images: vec![],
            ..Default::default()
        })
        .await;
    assert!(result.is_ok());
}
