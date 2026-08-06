//! A2A P2P end-to-end pipeline tests — ADR 0002.
//!
//! Uses only the public API of `rsclaw::a2a::relay` and `rsclaw::a2a::peer`.
//! Simulates a hub + 2 spoke topology in-process with mpsc channels
//! standing in for real WebSocket connections.
//!
//! Validates the complete flow:
//!   1. Route lease → hub routes
//!   2. PeerCandidate → hub delivers PeerCandidateRelay
//!   3. PeerConnected → hub sets RouteMode::Direct
//!   4. JSON-RPC SendMessage via PeerManager
//!   5. SendStreamingMessage events via PeerManager
//!   6. Cancel propagation via PeerManager
//!   7. Peer disconnect → route cleanup
//!   8. Multi-peer routing coexistence
//!   9. Serialization consistency of all P2P RelayFrame types
//!  10. Candidate priority ordering

use std::sync::Arc;
use std::time::Duration;

use rsclaw::a2a::relay::{
    Candidate, CandidateKind, RelayFrame, RelayHub, RouteMode,
};
use rsclaw::a2a::peer::PeerManager;
use rsclaw::a2a::types::JsonRpcResponse;
use tokio::sync::mpsc;
use serde_json::{json, Value};

const TEST_NODE_A: &str = "node-a";
const TEST_NODE_B: &str = "node-b";

// ---------------------------------------------------------------------------
// Test 1: Full PeerCandidate → PeerCandidateRelay delivery
// ---------------------------------------------------------------------------

#[test]
fn e2e_peer_candidate_relay_through_hub() {
    let hub = Arc::new(RelayHub::new());

    let (tx_a, _rx_a) = mpsc::unbounded_channel::<axum::extract::ws::Message>();
    let (tx_b, _rx_b) = mpsc::unbounded_channel::<axum::extract::ws::Message>();
    hub.register_connection(TEST_NODE_A, tx_a, 1);
    hub.register_connection(TEST_NODE_B, tx_b, 1);

    // Spoke A advertises agents.
    hub.apply_route_lease(TEST_NODE_A, &[format!("{TEST_NODE_A}/agent1")], 10_000, 1).unwrap();
    hub.apply_route_lease(TEST_NODE_B, &[format!("{TEST_NODE_B}/agent2")], 10_000, 1).unwrap();

    // Hub delivers a PeerCandidateRelay (as if spoke-A sent PeerCandidate
    // via the hub WS handler, and the hub forwarded it).
    let candidates_a = vec![Candidate {
        kind: CandidateKind::Host,
        url: "ws://10.0.0.1:18901/a2a/peer/ws".into(),
        priority: 100,
    }];
    let forward = RelayFrame::PeerCandidateRelay {
        source_node: TEST_NODE_A.into(),
        candidates: candidates_a.clone(),
    };
    assert!(
        hub.send_to_node(TEST_NODE_B, &forward),
        "hub should deliver PeerCandidateRelay to node-b"
    );

    // Routes are still Relayed.
    assert_eq!(
        hub.route_for(&format!("{TEST_NODE_A}/agent1")).unwrap().mode,
        RouteMode::Relayed,
    );
    assert_eq!(
        hub.route_for(&format!("{TEST_NODE_B}/agent2")).unwrap().mode,
        RouteMode::Relayed,
    );
}

// ---------------------------------------------------------------------------
// Test 2: Route mode transition on PeerConnected
// ---------------------------------------------------------------------------

#[test]
fn e2e_route_mode_transitions_to_direct_after_punch() {
    let hub = RelayHub::new();
    let (tx_a, _rx_a) = mpsc::unbounded_channel::<axum::extract::ws::Message>();
    let (tx_b, _rx_b) = mpsc::unbounded_channel::<axum::extract::ws::Message>();
    hub.register_connection(TEST_NODE_A, tx_a, 1);
    hub.register_connection(TEST_NODE_B, tx_b, 1);

    hub.apply_route_lease(TEST_NODE_A, &["node-a/main".into()], 10_000, 1).unwrap();
    hub.apply_route_lease(TEST_NODE_B, &["node-b/coder".into()], 10_000, 1).unwrap();

    assert_eq!(hub.route_for("node-a/main").unwrap().mode, RouteMode::Relayed);
    assert_eq!(hub.route_for("node-b/coder").unwrap().mode, RouteMode::Relayed);

    // Simulate PeerConnected from spoke-A → spoke-B (punch succeeded).
    let updated = hub.set_routes_direct(TEST_NODE_B);
    assert_eq!(updated, 1, "only node-b's route should flip");

    assert_eq!(hub.route_for("node-b/coder").unwrap().mode, RouteMode::Direct);
    assert_eq!(hub.route_for("node-a/main").unwrap().mode, RouteMode::Relayed);
}

// ---------------------------------------------------------------------------
// Test 3: P2P JSON-RPC via simulated peer connection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_jsonrpc_over_simulated_peer_direct_connection() {
    let peer_mgr_a = Arc::new(PeerManager::default());

    // a_to_b: A sends frames to B
    // b_to_a: B sends frames back to A  
    let (a_to_b_tx, mut a_to_b_rx) = mpsc::unbounded_channel::<RelayFrame>();
    let (b_to_a_tx, mut b_to_a_rx) = mpsc::unbounded_channel::<RelayFrame>();

    peer_mgr_a.register_connection(TEST_NODE_B, a_to_b_tx);

    // Spoke A sends a JSON-RPC request to spoke B.
    let pm_a = peer_mgr_a.clone();
    let invoke = tokio::spawn(async move {
        pm_a.invoke_jsonrpc(
            "node-b/agent2",
            "SendMessage",
            json!({"message": {"role": "user", "parts": [{"kind": "text", "text": "hello p2p"}]}}),
            "caller-a",
            TEST_NODE_B,
        )
        .await
    });

    // Spoke B receives the Request frame.
    let frame = tokio::time::timeout(Duration::from_millis(200), a_to_b_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let request_id = match &frame {
        RelayFrame::Request { request_id, target, .. } => {
            assert_eq!(target, "node-b/agent2");
            request_id.clone()
        }
        other => panic!("expected Request, got {other:?}"),
    };

    // B sends Response back through b_to_a channel.
    b_to_a_tx.send(RelayFrame::Response {
        request_id: request_id.clone(),
        response: JsonRpcResponse::ok(
            Value::String("task-1".into()),
            json!({"id": "task-1", "contextId": "ctx"}),
        ),
    }).unwrap();

    // A receives the Response — dispatch to peer_mgr_a.complete_pending.
    let resp_frame = tokio::time::timeout(Duration::from_millis(200), b_to_a_rx.recv())
        .await
        .unwrap()
        .unwrap();
    match &resp_frame {
        RelayFrame::Response { request_id: rid, response } => {
            assert_eq!(rid, &request_id);
            peer_mgr_a.complete_pending(rid, response.clone());
        }
        other => panic!("expected Response, got {other:?}"),
    }

    let response = invoke.await.unwrap().unwrap();
    assert_eq!(response.result.unwrap()["id"], "task-1");
}

// ---------------------------------------------------------------------------
// Test 4: P2P streaming events over simulated peer connection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_streaming_over_simulated_peer_direct_connection() {
    let peer_mgr_a = Arc::new(PeerManager::default());

    let (a_to_b_tx, mut a_to_b_rx) = mpsc::unbounded_channel::<RelayFrame>();
    let (b_to_a_tx, mut b_to_a_rx) = mpsc::unbounded_channel::<RelayFrame>();

    peer_mgr_a.register_connection(TEST_NODE_B, a_to_b_tx);

    // A sends a streaming request.
    let pm_a = peer_mgr_a.clone();
    let stream_invoke = tokio::spawn(async move {
        pm_a.invoke_streaming(
            "node-b/agent2",
            "SendStreamingMessage",
            json!({"message": {"role": "user", "parts": [{"kind": "text", "text": "stream test"}]}}),
            "caller-a",
            TEST_NODE_B,
        )
        .await
    });

    // B receives the Request frame from a_to_b.
    let frame = tokio::time::timeout(Duration::from_millis(200), a_to_b_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let stream_request_id = match &frame {
        RelayFrame::Request { request_id, .. } => request_id.clone(),
        other => panic!("expected Request, got {other:?}"),
    };

    // A's invoke_streaming returns (request_id, node_id, event_rx).
    let (_sr, _sn, mut event_rx) = stream_invoke.await.unwrap().unwrap();

    // B sends streaming events back through b_to_a channel → A's PeerManager.
    let events = [
        ("submitted", "t-stream-1", false),
        ("working", "t-stream-1", false),
        ("completed", "t-stream-1", true),
    ];
    for (i, (state, task_id, is_final)) in events.iter().enumerate() {
        let wire = json!({
            "kind": "status-update",
            "taskId": task_id,
            "contextId": "ctx-1",
            "status": {"state": state},
            "final": is_final,
        });
        b_to_a_tx.send(RelayFrame::Event {
            request_id: stream_request_id.clone(),
            seq: i as u64,
            result: wire.clone(),
        }).unwrap();

        // Drain the event from b_to_a channel and forward to A's PeerManager.
        let ev_frame = tokio::time::timeout(Duration::from_millis(100), b_to_a_rx.recv())
            .await
            .unwrap()
            .unwrap();
        if let RelayFrame::Event { request_id: rid, result, .. } = &ev_frame {
            assert_eq!(rid, &stream_request_id);
            let count = peer_mgr_a.forward_stream_event(rid, result.clone());
            assert_eq!(count, 1, "event {i} must reach subscriber");
        }
    }

    // A receives all 3 events.
    for i in 0..3 {
        let ev = tokio::time::timeout(Duration::from_millis(100), event_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("event {i} timed out"))
            .unwrap();
        assert_eq!(ev["kind"], "status-update");
        if i == 2 {
            assert!(ev["final"].as_bool().unwrap_or(false));
        }
    }

    // Task route recorded on A's PeerManager from stream events.
    assert_eq!(
        peer_mgr_a.route_for_task("t-stream-1").as_deref(),
        Some("node-b/agent2")
    );
}

// ---------------------------------------------------------------------------
// Test 5: Cancel propagation over simulated peer connection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_cancel_propagation_over_peer() {
    let peer_mgr_a = Arc::new(PeerManager::default());
    let (a_to_b_tx, mut a_to_b_rx) = mpsc::unbounded_channel::<RelayFrame>();
    let (_b_to_a_tx, _b_to_a_rx) = mpsc::unbounded_channel::<RelayFrame>();

    peer_mgr_a.register_connection(TEST_NODE_B, a_to_b_tx);
    // Note: in a real scenario, peer_mgr_b would be on the remote machine.
    // Here we test the Cancel frame delivery path only.

    // Start a streaming request from A.
    let pm_a = peer_mgr_a.clone();
    let stream_invoke = tokio::spawn(async move {
        pm_a.invoke_streaming(
            "node-b/agent2",
            "SendStreamingMessage",
            json!({"message": {"role": "user", "parts": [{"kind": "text", "text": "will cancel"}]}}),
            "caller-a",
            TEST_NODE_B,
        )
        .await
    });

    // Drain the Request frame sent by invoke_streaming.
    let _frame = tokio::time::timeout(Duration::from_millis(200), a_to_b_rx.recv())
        .await
        .unwrap()
        .unwrap();

    let (_stream_request_id, _stream_node_id, _event_rx) =
        stream_invoke.await.unwrap().unwrap();

    // A sends Cancel.
    peer_mgr_a.send_cancel_to(TEST_NODE_B, &_stream_request_id);

    // B receives Cancel frame.
    let cancel_frame = tokio::time::timeout(Duration::from_millis(200), a_to_b_rx.recv())
        .await
        .unwrap()
        .unwrap();
    match cancel_frame {
        RelayFrame::Cancel { request_id, .. } => {
            assert_eq!(request_id, _stream_request_id);
        }
        other => panic!("expected Cancel, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Test 6: Peer disconnect cleans up routes
// ---------------------------------------------------------------------------

#[test]
fn e2e_peer_disconnect_cleans_routes() {
    let peer_mgr = PeerManager::default();
    let (tx_b, _rx_b) = mpsc::unbounded_channel::<RelayFrame>();

    peer_mgr.register_connection(TEST_NODE_B, tx_b);
    peer_mgr.add_route("node-b/agent2", TEST_NODE_B);

    assert!(peer_mgr.has_direct_connection(TEST_NODE_B));
    assert!(peer_mgr.route_for("node-b/agent2").is_some());

    peer_mgr.unregister_connection(TEST_NODE_B);

    assert!(!peer_mgr.has_direct_connection(TEST_NODE_B));
    assert!(peer_mgr.route_for("node-b/agent2").is_none());
}

// ---------------------------------------------------------------------------
// Test 7: Multi-peer direct connection routing
// ---------------------------------------------------------------------------

#[test]
fn e2e_multi_peer_routing() {
    let peer_mgr = PeerManager::default();

    let (tx_x, _rx_x) = mpsc::unbounded_channel::<RelayFrame>();
    let (tx_y, _rx_y) = mpsc::unbounded_channel::<RelayFrame>();

    peer_mgr.register_connection("peer-x", tx_x);
    peer_mgr.register_connection("peer-y", tx_y);
    peer_mgr.add_route("peer-x/main", "peer-x");
    peer_mgr.add_route("peer-y/coder", "peer-y");

    assert!(peer_mgr.route_for("peer-x/main").is_some());
    assert!(peer_mgr.route_for("peer-y/coder").is_some());
    assert!(peer_mgr.route_for("peer-z/main").is_none());

    peer_mgr.unregister_connection("peer-x");

    assert!(!peer_mgr.has_direct_connection("peer-x"));
    assert!(peer_mgr.has_direct_connection("peer-y"));
    assert!(peer_mgr.route_for("peer-x/main").is_none());
    assert!(peer_mgr.route_for("peer-y/coder").is_some());
}

// ---------------------------------------------------------------------------
// Test 8: Serialization consistency — all P2P types
// ---------------------------------------------------------------------------

#[test]
fn e2e_all_p2p_frames_serde_consistent() {
    let roundtrip = |json: &str| -> RelayFrame {
        serde_json::from_str(json).unwrap_or_else(|e| panic!("deser fail: {e}"))
    };

    // PeerCandidate
    let f = roundtrip(
        r#"{"type":"peer_candidate","target_node":"b","candidates":[{"kind":"host","url":"ws://x","priority":100}]}"#,
    );
    assert!(matches!(f, RelayFrame::PeerCandidate { .. }));

    // PeerCandidateRelay
    let f = roundtrip(r#"{"type":"peer_candidate_relay","source_node":"a","candidates":[]}"#);
    assert!(matches!(f, RelayFrame::PeerCandidateRelay { .. }));

    // PeerConnected
    let f = roundtrip(r#"{"type":"peer_connected","peer_node":"x","direct_url":"ws://x"}"#);
    assert!(matches!(f, RelayFrame::PeerConnected { .. }));

    // Existing frames still work.
    let f = roundtrip(r#"{"type":"pong","ts":42}"#);
    assert!(matches!(f, RelayFrame::Pong { .. }));
}

// ---------------------------------------------------------------------------
// Test 9: Candidate priority ordering
// ---------------------------------------------------------------------------

#[test]
fn e2e_candidate_priority_order() {
    let candidates = vec![
        Candidate { kind: CandidateKind::Srflx, url: "ws://pub/a2a/peer/ws".into(), priority: 90 },
        Candidate { kind: CandidateKind::Host, url: "ws://lan/a2a/peer/ws".into(), priority: 100 },
        Candidate { kind: CandidateKind::Host, url: "ws://lan2/a2a/peer/ws".into(), priority: 50 },
    ];
    let mut sorted = candidates.clone();
    sorted.sort_by_key(|c| std::cmp::Reverse(c.priority));

    assert_eq!(sorted[0].kind, CandidateKind::Host);
    assert_eq!(sorted[0].priority, 100);
    assert_eq!(sorted[1].kind, CandidateKind::Srflx);
    assert_eq!(sorted[1].priority, 90);
    assert_eq!(sorted[2].priority, 50);
}

// ---------------------------------------------------------------------------
// Test 10: Hub routes coexist Relayed + Direct
// ---------------------------------------------------------------------------

#[test]
fn e2e_hub_routes_coexist_relayed_and_direct() {
    let hub = RelayHub::new();
    let (tx_a, _rx_a) = mpsc::unbounded_channel::<axum::extract::ws::Message>();
    let (tx_b, _rx_b) = mpsc::unbounded_channel::<axum::extract::ws::Message>();

    hub.register_connection("a", tx_a, 1);
    hub.register_connection("b", tx_b, 1);

    hub.apply_route_lease("a", &["a/main".into()], 10_000, 1).unwrap();
    hub.apply_route_lease("b", &["b/coder".into()], 10_000, 1).unwrap();
    hub.apply_route_lease("b", &["b/tester".into()], 10_000, 1).unwrap();

    hub.set_routes_direct("b");

    assert_eq!(hub.route_for("a/main").unwrap().mode, RouteMode::Relayed);
    assert_eq!(hub.route_for("b/coder").unwrap().mode, RouteMode::Direct);
    assert_eq!(hub.route_for("b/tester").unwrap().mode, RouteMode::Direct);
}
