//! A2A direct-frame pipeline tests for ADR 0002.
//!
//! These tests use bounded in-process channels and therefore validate only
//! RelayFrame request/response/stream/cancel correlation. They are not ICE,
//! WebRTC, TURN, NAT traversal, or gateway end-to-end tests.

use std::{sync::Arc, time::Duration};

use rsclaw::a2a::{peer::PeerManager, relay::RelayFrame, types::JsonRpcResponse};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const PEER_NODE: &str = "node-b";
const PEER_AGENT: &str = "node-b/main";

#[tokio::test]
async fn a2a_direct_pipeline_unary_response_is_correlated() {
    let manager = Arc::new(PeerManager::default());
    let (outbound_tx, mut outbound_rx) = mpsc::channel(8);
    manager.register_connection(PEER_NODE, "session-unary", outbound_tx);
    manager.add_route(PEER_AGENT, PEER_NODE);

    let invoke_manager = Arc::clone(&manager);
    let invoke = tokio::spawn(async move {
        invoke_manager
            .invoke_jsonrpc(
                PEER_AGENT,
                "SendMessage",
                json!({"message": {"parts": [{"type": "text", "text": "hello"}]}}),
                "caller-a",
                PEER_NODE,
            )
            .await
    });

    let frame = tokio::time::timeout(Duration::from_secs(1), outbound_rx.recv())
        .await
        .expect("direct request timeout")
        .expect("direct request channel closed");
    let request_id = match frame {
        RelayFrame::Request {
            request_id, target, ..
        } => {
            assert_eq!(target, PEER_AGENT);
            request_id
        }
        other => panic!("expected Request, got {other:?}"),
    };
    manager.complete_pending(
        &request_id,
        JsonRpcResponse::ok(Value::Null, json!({"id": "task-1"})),
    );

    let response = tokio::time::timeout(Duration::from_secs(1), invoke)
        .await
        .expect("direct response timeout")
        .expect("invoke task panicked")
        .expect("direct invocation failed");
    assert_eq!(response.result.expect("response result")["id"], "task-1");
}

#[tokio::test]
async fn a2a_direct_pipeline_stream_event_records_task_route() {
    let manager = Arc::new(PeerManager::default());
    let (outbound_tx, mut outbound_rx) = mpsc::channel(8);
    manager.register_connection(PEER_NODE, "session-stream", outbound_tx);
    manager.add_route(PEER_AGENT, PEER_NODE);

    let invoke_manager = Arc::clone(&manager);
    let invoke = tokio::spawn(async move {
        invoke_manager
            .invoke_streaming(
                PEER_AGENT,
                "SendStreamingMessage",
                json!({"message": {"parts": [{"type": "text", "text": "stream"}]}}),
                "caller-a",
                PEER_NODE,
            )
            .await
    });
    let frame = tokio::time::timeout(Duration::from_secs(1), outbound_rx.recv())
        .await
        .expect("stream request timeout")
        .expect("stream request channel closed");
    let request_id = match frame {
        RelayFrame::Request { request_id, .. } => request_id,
        other => panic!("expected Request, got {other:?}"),
    };
    let (_, _, mut event_rx) = invoke
        .await
        .expect("invoke task panicked")
        .expect("invoke stream");

    let receivers = manager.forward_stream_event(
        &request_id,
        json!({
            "kind": "status-update",
            "taskId": "task-stream",
            "status": {"state": "TASK_STATE_COMPLETED"},
            "final": true
        }),
    );
    assert_eq!(receivers, 1);
    let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .expect("stream event timeout")
        .expect("stream event channel closed");
    assert_eq!(event["taskId"], "task-stream");
    assert_eq!(
        manager.route_for_task("task-stream").as_deref(),
        Some(PEER_AGENT)
    );
}

#[tokio::test]
async fn a2a_direct_pipeline_cancel_uses_same_request_id() {
    let manager = PeerManager::default();
    let (outbound_tx, mut outbound_rx) = mpsc::channel(8);
    manager.register_connection(PEER_NODE, "session-cancel", outbound_tx);

    let (_, _, _event_rx) = manager
        .invoke_streaming(
            PEER_AGENT,
            "SendStreamingMessage",
            json!({}),
            "caller-a",
            PEER_NODE,
        )
        .await
        .expect("start direct stream");
    let request = tokio::time::timeout(Duration::from_secs(1), outbound_rx.recv())
        .await
        .expect("request timeout")
        .expect("request channel closed");
    let request_id = match request {
        RelayFrame::Request { request_id, .. } => request_id,
        other => panic!("expected Request, got {other:?}"),
    };

    manager.send_cancel_to(PEER_NODE, &request_id);
    let cancel = tokio::time::timeout(Duration::from_secs(1), outbound_rx.recv())
        .await
        .expect("cancel timeout")
        .expect("cancel channel closed");
    assert!(matches!(
        cancel,
        RelayFrame::Cancel { request_id: cancel_id, .. } if cancel_id == request_id
    ));
}
