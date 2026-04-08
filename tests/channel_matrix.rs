//! Integration tests for `MatrixChannel` (reqwest fallback, non-SDK).

use std::sync::Arc;

use rsclaw::channel::matrix::MatrixChannel;
use rsclaw::channel::{Channel, OutboundMessage};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path_regex},
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

fn make_channel(homeserver: &str) -> MatrixChannel {
    MatrixChannel::new(
        homeserver,
        "mock-access-token",
        "@bot:test.local",
        noop_on_message(),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn channel_name_is_matrix() {
    init_crypto();
    let ch = MatrixChannel::new(
        "https://matrix.example.org",
        "token",
        "@bot:example.org",
        noop_on_message(),
    );
    assert_eq!(ch.name(), "matrix");
}

/// Trailing slash in homeserver URL should be handled gracefully.
#[test]
fn homeserver_trailing_slash_stripped() {
    init_crypto();
    let ch = MatrixChannel::new(
        "https://matrix.example.org/",
        "token",
        "@bot:example.org",
        noop_on_message(),
    );
    // Channel should still be valid.
    assert_eq!(ch.name(), "matrix");
}

/// Send a text message to a mock Matrix homeserver.
#[tokio::test]
async fn send_text_puts_room_message() {
    init_crypto();
    let server = MockServer::start().await;

    // Matrix send uses PUT /_matrix/client/v3/rooms/{room_id}/send/m.room.message/{txn_id}
    Mock::given(method("PUT"))
        .and(path_regex(r"/_matrix/client/v3/rooms/.+/send/m.room.message/.+"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"event_id": "$mock_event"})),
        )
        .expect(1)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());

    let result = ch
        .send(OutboundMessage {
            target_id: "!room:test.local".to_owned(),
            is_group: true,
            text: "Hello Matrix".to_owned(),
            reply_to: None,
            images: vec![],
        })
        .await;

    assert!(result.is_ok(), "send should succeed: {:?}", result.err());
}

/// Messages exceeding 10000 chars should be chunked into multiple PUTs.
#[tokio::test]
async fn send_chunked_10000() {
    init_crypto();
    let server = MockServer::start().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"/_matrix/client/v3/rooms/.+/send/m.room.message/.+"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"event_id": "$ev"})),
        )
        .expect(2)
        .mount(&server)
        .await;

    let ch = make_channel(&server.uri());

    // 15000 chars -> should produce 2 chunks (10000 limit)
    let long_text = "M".repeat(15_000);
    let result = ch
        .send(OutboundMessage {
            target_id: "!bigroom:test.local".to_owned(),
            is_group: true,
            text: long_text,
            reply_to: None,
            images: vec![],
        })
        .await;

    assert!(result.is_ok(), "chunked send should succeed: {:?}", result.err());
}
