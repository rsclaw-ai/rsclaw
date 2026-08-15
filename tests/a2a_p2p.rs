//! Public API tests for ADR 0002 WebRTC direct-routing state.
//!
//! These tests exercise frame compatibility and route lifecycle. They do not
//! claim to prove ICE, DTLS/SCTP, TURN, or NAT traversal.

use rsclaw::a2a::{
    peer::PeerManager,
    relay::{RelayFrame, RelayHub, RouteMode},
};
use tokio::sync::mpsc;

#[test]
fn a2a_p2p_offer_frame_round_trips() {
    let frame = RelayFrame::PeerOffer {
        session_id: "session-1".into(),
        target_node: "node-b".into(),
        sdp: "v=0\r\n".into(),
        signature: Some("signature".into()),
    };
    let json = serde_json::to_string(&frame).expect("serialize peer offer");
    let decoded: RelayFrame = serde_json::from_str(&json).expect("deserialize peer offer");
    match decoded {
        RelayFrame::PeerOffer {
            session_id,
            target_node,
            sdp,
            signature,
        } => {
            assert_eq!(session_id, "session-1");
            assert_eq!(target_node, "node-b");
            assert_eq!(sdp, "v=0\r\n");
            assert_eq!(signature.as_deref(), Some("signature"));
        }
        other => panic!("expected PeerOffer, got {other:?}"),
    }
}

#[test]
fn a2a_p2p_answer_relay_frame_round_trips() {
    let frame = RelayFrame::PeerAnswerRelay {
        session_id: "session-2".into(),
        source_node: "node-b".into(),
        sdp: "v=0\r\n".into(),
        signature: None,
    };
    let json = serde_json::to_string(&frame).expect("serialize peer answer relay");
    let decoded: RelayFrame = serde_json::from_str(&json).expect("deserialize answer relay");
    assert!(matches!(
        decoded,
        RelayFrame::PeerAnswerRelay {
            session_id,
            source_node,
            ..
        } if session_id == "session-2" && source_node == "node-b"
    ));
}

#[test]
fn a2a_p2p_connected_frame_uses_session_not_direct_url() {
    let frame = RelayFrame::PeerConnected {
        peer_node: "node-b".into(),
        session_id: "session-3".into(),
    };
    let json = serde_json::to_string(&frame).expect("serialize peer connected");
    assert!(json.contains("\"session_id\":\"session-3\""));
    assert!(!json.contains("direct_url"));
}

#[test]
fn a2a_p2p_hub_route_remains_relayed() {
    let hub = RelayHub::new();
    hub.apply_route_lease("node-b", &["node-b/main".into()], 10_000, 1)
        .expect("apply route lease");
    let route = hub.route_for("node-b/main").expect("hub route");
    assert_eq!(route.mode, RouteMode::Relayed);
}

#[test]
fn a2a_p2p_direct_route_is_local_to_peer_manager() {
    let peer_manager = PeerManager::default();
    peer_manager
        .replace_routes("node-b", &["node-b/main".to_owned()])
        .expect("direct route lease");
    let route = peer_manager.route_for("node-b/main").expect("direct route");
    assert_eq!(route.mode, RouteMode::Direct);
    assert_eq!(route.node_id, "node-b");
}

#[test]
fn a2a_p2p_stale_generation_cannot_remove_replacement() {
    let peer_manager = PeerManager::default();
    let (first_tx, _first_rx) = mpsc::channel(4);
    let (second_tx, _second_rx) = mpsc::channel(4);
    let first = peer_manager.register_connection("node-b", "session-old", first_tx);
    let second = peer_manager.register_connection("node-b", "session-new", second_tx);
    peer_manager
        .replace_routes("node-b", &["node-b/main".to_owned()])
        .expect("direct route lease");

    peer_manager.unregister_connection("node-b", first);
    assert!(peer_manager.has_direct_connection("node-b"));
    assert!(peer_manager.route_for("node-b/main").is_some());

    peer_manager.unregister_connection("node-b", second);
    assert!(!peer_manager.has_direct_connection("node-b"));
    assert!(peer_manager.route_for("node-b/main").is_none());
}

#[test]
fn a2a_p2p_task_route_is_recorded_for_follow_up() {
    let peer_manager = PeerManager::default();
    peer_manager
        .record_task_route("task-42", "node-b/main")
        .expect("task route should be recorded");
    assert_eq!(
        peer_manager.route_for_task("task-42").as_deref(),
        Some("node-b/main")
    );
    assert!(peer_manager.route_for_task("unknown").is_none());
}

#[test]
fn a2a_p2p_relay_route_expires_without_direct_mutation() {
    let hub = RelayHub::new();
    hub.apply_route_lease("node-b", &["node-b/main".into()], 1, 1)
        .expect("apply short route lease");
    std::thread::sleep(std::time::Duration::from_millis(10));
    assert!(hub.route_for("node-b/main").is_none());
}
