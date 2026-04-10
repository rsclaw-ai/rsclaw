//! Integration tests for `QQBotChannel`.

use std::sync::Arc;

use rsclaw::channel::{Channel, OutboundMessage};
use rsclaw::channel::qq::QQBotChannel;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, path_regex},
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
            String,
            Vec<rsclaw::agent::registry::ImageAttachment>,
            Vec<rsclaw::agent::registry::FileAttachment>,
        ) + Send
        + Sync,
> {
    Arc::new(|_, _, _, _, _, _, _| {})
}

/// Helper: mount a token endpoint mock that returns a valid access token.
async fn mount_token_mock(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/app/getAppAccessToken"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "test_access_token_123",
            "expires_in": "7200"
        })))
        .mount(server)
        .await;
}

fn make_channel(api_base: &str, token_url: &str) -> QQBotChannel {
    init_crypto();
    QQBotChannel::new_with_overrides(
        "app_id_test",
        "app_secret_test",
        false,
        None,
        noop_on_message(),
        Some(api_base.to_owned()),
        Some(token_url.to_owned()),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn channel_name_is_qq() {
    init_crypto();
    let ch = QQBotChannel::new("appid", "secret", false, None, noop_on_message());
    assert_eq!(ch.name(), "qq");
}

#[tokio::test]
async fn send_group_message() {
    let server = MockServer::start().await;
    mount_token_mock(&server).await;

    Mock::given(method("POST"))
        .and(path_regex(r"/v2/groups/group_open_id_1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "msg1"})))
        .expect(1)
        .mount(&server)
        .await;

    let token_url = format!("{}/app/getAppAccessToken", server.uri());
    let ch = make_channel(&server.uri(), &token_url);

    let msg = OutboundMessage {
        target_id: "group_open_id_1".to_owned(),
        is_group: true,
        text: "Hello, QQ group!".to_owned(),
        reply_to: Some("orig_msg_id".to_owned()),
        images: vec![],
        ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed");
}

#[tokio::test]
async fn send_c2c_message() {
    let server = MockServer::start().await;
    mount_token_mock(&server).await;

    Mock::given(method("POST"))
        .and(path_regex(r"/v2/users/user_open_id_1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "msg2"})))
        .expect(1)
        .mount(&server)
        .await;

    let token_url = format!("{}/app/getAppAccessToken", server.uri());
    let ch = make_channel(&server.uri(), &token_url);

    let msg = OutboundMessage {
        target_id: "user_open_id_1".to_owned(),
        is_group: false,
        text: "Hello, QQ user!".to_owned(),
        reply_to: Some("orig_msg_id".to_owned()),
        images: vec![],
        ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed");
}

#[tokio::test]
async fn token_refresh_happens_automatically() {
    let server = MockServer::start().await;

    // Token endpoint should be called at least once.
    Mock::given(method("POST"))
        .and(path("/app/getAppAccessToken"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "refreshed_token",
            "expires_in": "7200"
        })))
        .expect(1..)
        .mount(&server)
        .await;

    // Message endpoint
    Mock::given(method("POST"))
        .and(path_regex(r"/v2/groups/grp1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "m1"})))
        .mount(&server)
        .await;

    let token_url = format!("{}/app/getAppAccessToken", server.uri());
    let ch = make_channel(&server.uri(), &token_url);

    let msg = OutboundMessage {
        target_id: "grp1".to_owned(),
        is_group: true,
        text: "Token test".to_owned(),
        reply_to: Some("mid".to_owned()),
        images: vec![],
        ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed");
}

#[tokio::test]
async fn send_guild_message() {
    let server = MockServer::start().await;
    mount_token_mock(&server).await;

    Mock::given(method("POST"))
        .and(path_regex(r"/channels/channel_123/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "gm1"})))
        .expect(1)
        .mount(&server)
        .await;

    let token_url = format!("{}/app/getAppAccessToken", server.uri());
    let ch = make_channel(&server.uri(), &token_url);

    let msg = OutboundMessage {
        target_id: "guild:channel_123".to_owned(),
        is_group: false,
        text: "Hello, guild channel!".to_owned(),
        reply_to: Some("orig_mid".to_owned()),
        images: vec![],
        ..Default::default()
    };

    ch.send(msg).await.expect("send should succeed");
}

#[tokio::test]
async fn send_fails_on_http_error() {
    let server = MockServer::start().await;
    mount_token_mock(&server).await;

    Mock::given(method("POST"))
        .and(path_regex(r"/v2/groups/grp1/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_string("error"))
        .mount(&server)
        .await;

    let token_url = format!("{}/app/getAppAccessToken", server.uri());
    let ch = make_channel(&server.uri(), &token_url);

    let msg = OutboundMessage {
        target_id: "grp1".to_owned(),
        is_group: true,
        text: "Should fail".to_owned(),
        reply_to: Some("mid".to_owned()),
        images: vec![],
        ..Default::default()
    };

    let result = ch.send(msg).await;
    assert!(result.is_err(), "should fail on HTTP 500");
}
