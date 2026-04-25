//! Integration test: graceful-restart event flow.
//!
//! Exercises the backend half of the restart system end-to-end:
//!   1. A live WS connection receives `restart.required` when a request is
//!      published into `restart_request_tx`.
//!   2. A connection that arrives AFTER a request was latched into
//!      `pending_restart` receives the latched event during handshake replay.
//!   3. `ShutdownCoordinator::begin_drain()` flips the public `is_draining`
//!      bit so callers can gate new work.

mod common;

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use rsclaw::events::{RestartReason, RestartRequest, RestartUrgency};
use serde_json::json;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

use common::{free_addr, start_server_with_handles};

const RECV_TIMEOUT: Duration = Duration::from_secs(3);

/// Open a WS connection and complete the handshake. Returns the connected
/// stream once the server has acknowledged `connect` with a `res` frame and
/// the trailing `presence` event has been observed.
///
/// Frames consumed before this returns:
///   - `connect.challenge` (event)
///   - `res` for our `connect` request
///   - `presence` (event)
///
/// Any leftover frames (notably the latched `restart.required` replay) are
/// left on the stream for the caller to read.
async fn handshake(
    addr: std::net::SocketAddr,
) -> tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
> {
    let url = format!("ws://{addr}/");
    let (mut stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");

    // 1. Read `connect.challenge`.
    let challenge = timeout(RECV_TIMEOUT, stream.next())
        .await
        .expect("challenge timeout")
        .expect("challenge stream end")
        .expect("challenge ws err");
    let challenge_text = match challenge {
        Message::Text(t) => t.to_string(),
        other => panic!("expected text challenge, got {other:?}"),
    };
    let challenge_json: serde_json::Value =
        serde_json::from_str(&challenge_text).expect("challenge json");
    assert_eq!(challenge_json["event"], "connect.challenge");

    // 2. Send connect request.
    let req = json!({
        "type": "req",
        "id": "1",
        "method": "connect",
        "params": {
            "minProtocol": 3,
            "maxProtocol": 3,
            "deviceId": "test-device",
        }
    });
    stream
        .send(Message::Text(req.to_string().into()))
        .await
        .expect("send connect");

    // 3. Read `res` for connect — must be ok.
    let res = timeout(RECV_TIMEOUT, stream.next())
        .await
        .expect("res timeout")
        .expect("res stream end")
        .expect("res ws err");
    let res_text = match res {
        Message::Text(t) => t.to_string(),
        other => panic!("expected text res, got {other:?}"),
    };
    let res_json: serde_json::Value = serde_json::from_str(&res_text).expect("res json");
    assert_eq!(res_json["type"], "res");
    assert_eq!(res_json["ok"], true, "connect should succeed: {res_json}");

    // 4. Read `presence` event.
    let presence = timeout(RECV_TIMEOUT, stream.next())
        .await
        .expect("presence timeout")
        .expect("presence stream end")
        .expect("presence ws err");
    let presence_text = match presence {
        Message::Text(t) => t.to_string(),
        other => panic!("expected text presence, got {other:?}"),
    };
    let presence_json: serde_json::Value =
        serde_json::from_str(&presence_text).expect("presence json");
    assert_eq!(presence_json["event"], "presence");

    stream
}

/// Drain frames until one with `event == "restart.required"` is seen, then
/// return its parsed JSON. Times out at [`RECV_TIMEOUT`].
async fn read_until_restart_required(
    stream: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + RECV_TIMEOUT;
    loop {
        let frame = timeout(deadline.saturating_duration_since(tokio::time::Instant::now()), stream.next())
            .await
            .expect("restart frame timeout")
            .expect("restart stream end")
            .expect("restart ws err");
        let text = match frame {
            Message::Text(t) => t.to_string(),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected frame: {other:?}"),
        };
        let value: serde_json::Value = serde_json::from_str(&text).expect("frame json");
        if value.get("event").and_then(|v| v.as_str()) == Some("restart.required") {
            return value;
        }
    }
}

fn sample_request() -> RestartRequest {
    RestartRequest::new(
        RestartReason::ConfigChanged {
            sections: vec!["gateway".to_owned()],
        },
        RestartUrgency::Recommended,
        "Config changed; restart to apply.".to_owned(),
    )
}

fn assert_restart_payload_shape(value: &serde_json::Value) {
    assert_eq!(value["type"], "event");
    assert_eq!(value["event"], "restart.required");

    let payload = value
        .get("payload")
        .expect("frame missing payload");
    // RestartRequest uses default (snake_case) serde naming, unlike the
    // surrounding EventFrame which is camelCase.
    assert!(
        payload["at_ms"].is_u64(),
        "at_ms missing or not u64: {payload}"
    );
    assert!(payload["urgency"].is_string(), "urgency missing: {payload}");
    assert!(
        payload["message"].is_string(),
        "message missing: {payload}"
    );
    assert!(
        payload["reason"].is_object(),
        "reason missing or not object: {payload}"
    );
    assert_eq!(
        payload["reason"]["kind"], "config_changed",
        "reason kind should be config_changed: {payload}"
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_restart_event_reaches_connected_ws() {
    let addr = free_addr();
    let handles = start_server_with_handles(addr).await;

    let mut stream = handshake(addr).await;

    // Publish AFTER the connection is fully established and its restart-relay
    // task has subscribed. Do not latch into `pending_restart` here — this
    // test specifically covers the live-broadcast path.
    let req = sample_request();
    handles
        .restart_request_tx
        .send(req.clone())
        .expect("publish restart");

    let frame = read_until_restart_required(&mut stream).await;
    assert_restart_payload_shape(&frame);
    assert_eq!(frame["payload"]["message"], req.message);

    // Tidy up.
    let _ = stream.close(None).await;
}

#[tokio::test]
async fn pending_restart_replayed_on_new_connection() {
    let addr = free_addr();
    let handles = start_server_with_handles(addr).await;

    // Latch a pending request BEFORE any WS client connects. The replay path
    // reads from `pending_restart`, not the broadcast channel, so we only
    // need to populate the RwLock.
    let req = sample_request();
    {
        let mut guard = handles
            .pending_restart
            .write()
            .expect("pending_restart lock");
        *guard = Some(req.clone());
    }

    let mut stream = handshake(addr).await;

    let frame = read_until_restart_required(&mut stream).await;
    assert_restart_payload_shape(&frame);
    assert_eq!(frame["payload"]["message"], req.message);
    assert_eq!(frame["payload"]["at_ms"], req.at_ms);

    let _ = stream.close(None).await;
}

#[tokio::test]
async fn drain_blocks_new_requests() {
    // Spinning up the server is not required for this case — the drain bit is
    // owned by `ShutdownCoordinator` itself and the test asserts only on the
    // public surface that downstream callers actually gate on.
    let addr = free_addr();
    let handles = start_server_with_handles(addr).await;

    assert!(
        !handles.shutdown.is_draining(),
        "fresh coordinator should not be draining"
    );

    handles.shutdown.begin_drain();

    assert!(
        handles.shutdown.is_draining(),
        "begin_drain() should set is_draining = true"
    );

    // Idempotent — second call must not flip back or panic.
    handles.shutdown.begin_drain();
    assert!(handles.shutdown.is_draining());

    // `notified` returns immediately once draining; cap at the recv timeout
    // so a regression in that fast-path manifests as a test failure rather
    // than a hang.
    timeout(RECV_TIMEOUT, handles.shutdown.notified())
        .await
        .expect("notified should return immediately while draining");
}
