//! Shared channel mock tools for integration tests.

#![allow(dead_code, unused_imports)]

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Message builders
// ---------------------------------------------------------------------------

/// Build a simple text message JSON value.
pub fn text_msg(user: &str, text: &str) -> Value {
    json!({
        "user": user,
        "text": text,
        "ts": "1700000000.000001",
        "type": "message",
    })
}

/// Build a group/channel message JSON value.
pub fn group_msg(user: &str, text: &str, channel: &str) -> Value {
    json!({
        "user": user,
        "text": text,
        "channel": channel,
        "ts": "1700000000.000002",
        "type": "message",
    })
}

/// Build a message that is a reply to another message.
pub fn msg_with_reply(user: &str, text: &str, reply_to_ts: &str) -> Value {
    json!({
        "user": user,
        "text": text,
        "ts": "1700000000.000003",
        "type": "message",
        "thread_ts": reply_to_ts,
    })
}

/// Build a message with image attachments.
pub fn msg_with_images(user: &str, text: &str, image_urls: &[&str]) -> Value {
    let files: Vec<Value> = image_urls
        .iter()
        .map(|url| {
            json!({
                "url_private": url,
                "mimetype": "image/png",
            })
        })
        .collect();
    json!({
        "user": user,
        "text": text,
        "ts": "1700000000.000004",
        "type": "message",
        "files": files,
    })
}

// ---------------------------------------------------------------------------
// HTTP mock helpers
// ---------------------------------------------------------------------------

/// Mount a mock that responds with 200 and the given JSON body.
pub async fn mount_ok_json(server: &MockServer, endpoint: &str, body: &Value) {
    Mock::given(method("POST"))
        .and(path(endpoint))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

/// Mount a mock that responds with 429 rate limit.
pub async fn mount_rate_limit(server: &MockServer, endpoint: &str) {
    Mock::given(method("POST"))
        .and(path(endpoint))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_string(r#"{"error":"rate_limited","retry_after":1}"#)
                .insert_header("retry-after", "1"),
        )
        .mount(server)
        .await;
}

/// Mount a mock that responds with the given error status code.
pub async fn mount_error(server: &MockServer, endpoint: &str, status: u16, body: &str) {
    Mock::given(method("POST"))
        .and(path(endpoint))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------
// WebSocket mock
// ---------------------------------------------------------------------------

/// Actions that a mock WebSocket server can perform.
#[derive(Debug, Clone)]
pub enum WsAction {
    /// Send a text frame to the client.
    SendText(String),
    /// Send a JSON value as text.
    SendJson(Value),
    /// Close the connection.
    Close,
}

/// A minimal mock WebSocket server handle.
pub struct MockWsServer {
    pub addr: std::net::SocketAddr,
    pub url: String,
    _handle: tokio::task::JoinHandle<()>,
}

/// Start a mock WebSocket server that sends the given actions in order
/// upon receiving any connection.
pub async fn start_mock_ws(actions: Vec<WsAction>) -> MockWsServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ws");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("ws://{addr}");

    let handle = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let ws = tokio_tungstenite::accept_async(stream)
                .await
                .expect("ws accept");
            let (mut writer, _reader) = futures::StreamExt::split(ws);
            for action in actions {
                match action {
                    WsAction::SendText(text) => {
                        use futures::SinkExt;
                        let _ = writer
                            .send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
                            .await;
                    }
                    WsAction::SendJson(val) => {
                        use futures::SinkExt;
                        let text = serde_json::to_string(&val).unwrap_or_default();
                        let _ = writer
                            .send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
                            .await;
                    }
                    WsAction::Close => {
                        use futures::SinkExt;
                        let _ = writer.close().await;
                        return;
                    }
                }
            }
        }
    });

    MockWsServer {
        addr,
        url,
        _handle: handle,
    }
}
