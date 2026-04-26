//! Integration test: graceful-restart event flow.
//!
//! Exercises the backend half of the restart system end-to-end:
//!   1. A live WS connection receives `restart.required` when a request is
//!      published into `restart_request_tx`.
//!   2. A connection that arrives AFTER a request was latched into
//!      `pending_restart` receives the latched event during handshake replay.
//!   3. `ShutdownCoordinator::begin_drain()` flips the public `is_draining`
//!      bit so callers can gate new work.
//!   4. A config-file edit drives `FileWatcher` to emit `RequiresRestart`,
//!      and the bridge that forwards it produces a `RestartRequest` whose
//!      `reason.kind == "config_changed"` reaches a connected WS client.
//!   5. A simulated BGE-download completion publishes a
//!      `ModelDownloaded` `RestartRequest` and a fresh WS connection sees
//!      the latched event during handshake replay.

mod common;

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use rsclaw::{
    events::{RestartReason, RestartRequest, RestartUrgency},
    gateway::{ConfigChange, FileWatcher},
};
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

fn assert_restart_payload_shape(value: &serde_json::Value, expected_kind: &str) {
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
        payload["reason"]["kind"], expected_kind,
        "reason kind should be {expected_kind}: {payload}"
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

    // The handshake function returns once `presence` is observed (step 6 of
    // the WS handshake), but the per-connection restart relay subscribes
    // later in the same handshake task (around step 8b). Yield a bit so the
    // task definitely reaches the `restart_request_tx.subscribe()` call
    // before we publish — otherwise this test races with a fresh tokio
    // runtime and occasionally drops the broadcast.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Publish AFTER the connection is fully established and its restart-relay
    // task has subscribed. Do not latch into `pending_restart` here — this
    // test specifically covers the live-broadcast path.
    let req = sample_request();
    handles
        .restart_request_tx
        .send(req.clone())
        .expect("publish restart");

    let frame = read_until_restart_required(&mut stream).await;
    assert_restart_payload_shape(&frame, "config_changed");
    assert_eq!(frame["payload"]["message"], req.message);

    // Tidy up. Closing on a half-shut server is best-effort.
    stream.close(None).await.ok();
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
    assert_restart_payload_shape(&frame, "config_changed");
    assert_eq!(frame["payload"]["message"], req.message);
    assert_eq!(frame["payload"]["at_ms"], req.at_ms);

    stream.close(None).await.ok();
}

#[tokio::test]
async fn begin_drain_flips_state_and_notifies() {
    // The server itself is not strictly required — the drain bit is owned by
    // `ShutdownCoordinator` directly — but we still spin one up so this test
    // asserts on the same coordinator instance the rest of the system holds.
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

#[tokio::test]
async fn file_watcher_change_emits_restart_required() {
    // Drives the full chain: file mutation -> FileWatcher::process_change ->
    // `ConfigChange::RequiresRestart` -> bridge -> publish_restart -> WS
    // replay. The FileWatcher half is exercised through the public API; the
    // bridge half lives behind `pub(crate) publish_restart` in startup.rs,
    // so the test inline-replicates the same two side-effects (latch write
    // + broadcast send) that the production bridge performs.

    let addr = free_addr();
    let handles = start_server_with_handles(addr).await;

    // Stage a temp config that parses cleanly. The exact port doesn't matter
    // — the watcher only cares whether `gateway.port` differs between two
    // successive parses.
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg_path = dir.path().join("rsclaw.json5");
    std::fs::write(&cfg_path, r#"{ "gateway": { "port": 18888 } }"#).expect("write initial cfg");

    let (mut watcher, mut rx) = FileWatcher::new(cfg_path.clone());

    // Mutate the file so `detect_restart_fields` will flag `gateway.port`.
    std::fs::write(&cfg_path, r#"{ "gateway": { "port": 19000 } }"#).expect("write updated cfg");

    // Drive the watcher manually — calling `process_change` directly bypasses
    // the 2 s polling tick.
    watcher.process_change().await;

    // Pull the change off the rx; must be `RequiresRestart` carrying the port
    // field. Anything else (FullReload, no event) is a regression.
    let change = rx.try_recv().expect("expected ConfigChange after edit");
    let fields = match change {
        ConfigChange::RequiresRestart(fields) => fields,
        other => panic!("expected RequiresRestart, got {other:?}"),
    };
    assert!(
        fields.iter().any(|f| f == "gateway.port"),
        "expected gateway.port to be flagged: {fields:?}"
    );

    // Inline-replicate the bridge in startup.rs (FileWatcher -> publish_restart):
    // write the latch, then broadcast. `publish_restart` itself is
    // `pub(crate)`, but the two side-effects are exactly these two lines.
    let req = RestartRequest::new(
        RestartReason::ConfigChanged {
            sections: fields.clone(),
        },
        RestartUrgency::Required,
        "Config changed; restart required.".to_owned(),
    );
    {
        let mut guard = handles
            .pending_restart
            .write()
            .expect("pending_restart lock");
        *guard = Some(req.clone());
    }
    // No live subscribers yet — `send` returning `Err` is normal here. The
    // latch is what carries the event to the WS that's about to connect.
    handles.restart_request_tx.send(req.clone()).ok();

    // Connect a fresh client; the latch replay path must surface the same
    // RestartRequest, including the `gateway.port` section.
    let mut stream = handshake(addr).await;
    let frame = read_until_restart_required(&mut stream).await;
    assert_restart_payload_shape(&frame, "config_changed");
    assert_eq!(frame["payload"]["urgency"], "required");
    let sections = frame["payload"]["reason"]["sections"]
        .as_array()
        .expect("sections array");
    assert!(
        sections.iter().any(|v| v.as_str() == Some("gateway.port")),
        "gateway.port not in sections: {sections:?}"
    );
    stream.close(None).await.ok();
}

#[tokio::test]
async fn bge_download_emits_restart_required() {
    // The BGE downloader runs as a `tokio::spawn` task at startup that calls
    // `publish_restart` with `RestartReason::ModelDownloaded { name }` once
    // the model finishes downloading. Driving a real download requires
    // network access plus a multi-hundred-MB GGUF, so this E2E stops at the
    // contract: a `ModelDownloaded` request published into the same handles
    // the downloader uses must reach a fresh WS via the latch replay path
    // and serialize with the snake_case wire shape the UI parses.

    let addr = free_addr();
    let handles = start_server_with_handles(addr).await;

    let req = RestartRequest::new(
        RestartReason::ModelDownloaded {
            name: "BAAI/bge-small-en-v1.5".to_owned(),
        },
        RestartUrgency::Recommended,
        "Embedding model downloaded; restart to load it.".to_owned(),
    );
    // `publish_restart` is pub(crate); the two side-effects below are an
    // inline replication. Latch first so a connection that races the send
    // still sees the event via replay.
    {
        let mut guard = handles
            .pending_restart
            .write()
            .expect("pending_restart lock");
        *guard = Some(req.clone());
    }
    // No live subscribers yet — `send` returning `Err` is normal here. The
    // latch is what matters for the replay path under test.
    handles.restart_request_tx.send(req.clone()).ok();

    let mut stream = handshake(addr).await;
    let frame = read_until_restart_required(&mut stream).await;

    assert_restart_payload_shape(&frame, "model_downloaded");
    assert_eq!(frame["payload"]["urgency"], "recommended");
    assert_eq!(frame["payload"]["message"], req.message);
    assert_eq!(
        frame["payload"]["reason"]["name"],
        "BAAI/bge-small-en-v1.5"
    );

    stream.close(None).await.ok();
}
