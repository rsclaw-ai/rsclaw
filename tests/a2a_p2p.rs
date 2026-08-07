//! A2A P2P hole-punch integration tests — ADR 0002.
//!
//! Tests using only the public API: wire-format round-trips, route mode
//! transitions, PeerManager lifecycle, and the Symmetric NAT fallback path.

use rsclaw::a2a::relay::{
    Candidate, CandidateKind, RelayFrame, RelayHub, RouteMode,
};
use rsclaw::a2a::peer::PeerManager;
use tokio::sync::mpsc;
use serde_json;

// ---------------------------------------------------------------------------
// Candidate serde round-trips
// ---------------------------------------------------------------------------

#[test]
fn candidate_kind_serde_lowercase() {
    assert_eq!(serde_json::to_string(&CandidateKind::Host).unwrap(), "\"host\"");
    assert_eq!(serde_json::to_string(&CandidateKind::Srflx).unwrap(), "\"srflx\"");
    assert_eq!(serde_json::to_string(&CandidateKind::Relay).unwrap(), "\"relay\"");
}

#[test]
fn candidate_round_trips() {
    let c = Candidate {
        kind: CandidateKind::Srflx,
        url: "ws://203.0.113.1:45000/a2a/peer/ws".into(),
        priority: 90,
    };
    let json = serde_json::to_string(&c).unwrap();
    let c2: Candidate = serde_json::from_str(&json).unwrap();
    assert_eq!(c2.kind, CandidateKind::Srflx);
    assert_eq!(c2.url, c.url);
    assert_eq!(c2.priority, 90);
}

// ---------------------------------------------------------------------------
// RelayFrame P2P variants — wire-format round-trips
// ---------------------------------------------------------------------------

#[test]
fn peer_candidate_frame_roundtrip() {
    let frame = RelayFrame::PeerCandidate {
        target_node: "node-b".into(),
        candidates: vec![Candidate {
            kind: CandidateKind::Host,
            url: "ws://10.0.0.1:18889/a2a/peer/ws".into(),
            priority: 100,
        }],
    };
    let json = serde_json::to_string(&frame).unwrap();
    assert!(json.contains("\"peer_candidate\""));
    let decoded: RelayFrame = serde_json::from_str(&json).unwrap();
    match decoded {
        RelayFrame::PeerCandidate { target_node, candidates } => {
            assert_eq!(target_node, "node-b");
            assert_eq!(candidates.len(), 1);
            assert_eq!(candidates[0].kind, CandidateKind::Host);
        }
        other => panic!("expected PeerCandidate, got {other:?}"),
    }
}

#[test]
fn peer_candidate_relay_frame_roundtrip() {
    let frame = RelayFrame::PeerCandidateRelay {
        source_node: "spoke-a".into(),
        candidates: vec![
            Candidate { kind: CandidateKind::Host, url: "ws://a:18889/peer/ws".into(), priority: 100 },
            Candidate { kind: CandidateKind::Srflx, url: "ws://p:45000/peer/ws".into(), priority: 90 },
        ],
    };
    let json = serde_json::to_string(&frame).unwrap();
    assert!(json.contains("\"peer_candidate_relay\""));
    let decoded: RelayFrame = serde_json::from_str(&json).unwrap();
    match decoded {
        RelayFrame::PeerCandidateRelay { source_node, candidates } => {
            assert_eq!(source_node, "spoke-a");
            assert_eq!(candidates.len(), 2);
        }
        other => panic!("expected PeerCandidateRelay, got {other:?}"),
    }
}

#[test]
fn peer_connected_frame_roundtrip() {
    let frame = RelayFrame::PeerConnected {
        peer_node: "target-node".into(),
        direct_url: "ws://1.2.3.4:56789/a2a/peer/ws".into(),
    };
    let json = serde_json::to_string(&frame).unwrap();
    assert!(json.contains("\"peer_connected\""));
    let decoded: RelayFrame = serde_json::from_str(&json).unwrap();
    match decoded {
        RelayFrame::PeerConnected { peer_node, direct_url } => {
            assert_eq!(peer_node, "target-node");
            assert_eq!(direct_url, "ws://1.2.3.4:56789/a2a/peer/ws");
        }
        other => panic!("expected PeerConnected, got {other:?}"),
    }
}

#[test]
fn peer_variants_coexist_with_existing_frames() {
    let frames_json = &[
        r#"{"type":"hello","protocol":"v1","node_id":"n1","node_version":null,"agent_card":null,"capabilities":null}"#,
        r#"{"type":"ping","ts":42}"#,
        r#"{"type":"pong","ts":42}"#,
        r#"{"type":"peer_candidate","target_node":"b","candidates":[]}"#,
        r#"{"type":"peer_candidate_relay","source_node":"a","candidates":[]}"#,
        r#"{"type":"peer_connected","peer_node":"x","direct_url":"ws://x"}"#,
    ];
    for json in frames_json {
        let frame: RelayFrame = serde_json::from_str(json)
            .unwrap_or_else(|e| panic!("failed to parse '{json}': {e}"));
        let kind = match frame {
            RelayFrame::Hello { .. } => "hello",
            RelayFrame::Ping { .. } => "ping",
            RelayFrame::Pong { .. } => "pong",
            RelayFrame::PeerCandidate { .. } => "peer_candidate",
            RelayFrame::PeerCandidateRelay { .. } => "peer_candidate_relay",
            RelayFrame::PeerConnected { .. } => "peer_connected",
            other => panic!("unexpected variant: {other:?}"),
        };
        let expected = json.split('"').nth(3).unwrap();
        assert_eq!(kind, expected);
    }
}

// ---------------------------------------------------------------------------
// Route mode transitions (public API)
// ---------------------------------------------------------------------------

#[test]
fn routes_start_relayed_by_default() {
    let hub = RelayHub::new();
    hub.apply_route_lease("alpha", &["alpha/main".to_owned()], 10_000, 1).unwrap();
    let route = hub.route_for("alpha/main").expect("route");
    assert_eq!(route.mode, RouteMode::Relayed);
}

#[test]
fn set_routes_direct_is_idempotent() {
    let hub = RelayHub::new();
    hub.apply_route_lease("a", &["a/main".to_owned()], 10_000, 1).unwrap();
    assert_eq!(hub.set_routes_direct("a"), 1);
    assert_eq!(hub.set_routes_direct("a"), 0);
}

#[test]
fn set_routes_direct_noop_on_unknown_node() {
    let hub = RelayHub::new();
    assert_eq!(hub.set_routes_direct("ghost"), 0);
}

#[test]
fn send_to_node_rejects_disconnected_node() {
    let hub = RelayHub::new();
    assert!(!hub.send_to_node("ghost-node", &RelayFrame::Ping { ts: 1 }));
}

// ---------------------------------------------------------------------------
// PeerManager route resolution (public API)
// ---------------------------------------------------------------------------

#[test]
fn peer_manager_add_and_lookup_route() {
    let pm = PeerManager::default();
    pm.add_route("peer-z/agent", "peer-z");
    let route = pm.route_for("peer-z/agent").expect("route");
    assert_eq!(route.mode, RouteMode::Direct);
    assert_eq!(route.node_id, "peer-z");
}

#[test]
fn peer_manager_route_missing_for_unknown_target() {
    let pm = PeerManager::default();
    assert!(pm.route_for("unknown/main").is_none());
}

#[test]
fn peer_manager_routes_are_always_direct() {
    let pm = PeerManager::default();
    pm.add_route("peer-x/main", "peer-x");
    let route = pm.route_for("peer-x/main").expect("route");
    assert_eq!(route.mode, RouteMode::Direct);
}

#[test]
fn peer_manager_routes_cleaned_on_unregister() {
    let pm = PeerManager::default();
    let (tx, _rx) = mpsc::unbounded_channel();
    pm.register_connection("node-k", tx);
    pm.add_route("node-k/agent", "node-k");
    assert!(pm.route_for("node-k/agent").is_some());
    pm.unregister_connection("node-k");
    assert!(pm.route_for("node-k/agent").is_none());
}

#[test]
fn peer_manager_candidate_cache() {
    let pm = PeerManager::default();
    let candidates = vec![Candidate {
        kind: CandidateKind::Host,
        url: "ws://10.0.0.1:18889/a2a/peer/ws".into(),
        priority: 100,
    }];
    pm.cache_candidates("peer-c", candidates.clone());
    let got = pm.get_candidates("peer-c").expect("fresh");
    assert_eq!(got.len(), 1);
    assert!(pm.get_candidates("unknown").is_none());
}

#[test]
fn peer_manager_task_routes() {
    let pm = PeerManager::default();
    pm.record_task_route("task-42", "peer-q/main");
    assert_eq!(pm.route_for_task("task-42").as_deref(), Some("peer-q/main"));
    assert!(pm.route_for_task("unknown").is_none());
}

#[test]
fn peer_manager_connection_lifecycle() {
    let pm = PeerManager::default();
    let (tx, _rx) = mpsc::unbounded_channel();
    pm.register_connection("peer-a", tx);
    assert!(pm.has_direct_connection("peer-a"));
    pm.unregister_connection("peer-a");
    assert!(!pm.has_direct_connection("peer-a"));
}

#[test]
fn peer_manager_replace_connection() {
    let pm = PeerManager::default();
    let (tx1, _rx1) = mpsc::unbounded_channel();
    let (tx2, _rx2) = mpsc::unbounded_channel();
    pm.register_connection("peer-x", tx1);
    pm.register_connection("peer-x", tx2);
    assert!(pm.has_direct_connection("peer-x"));
}

// ---------------------------------------------------------------------------
// Hub route lease + expiry
// ---------------------------------------------------------------------------

#[test]
fn relayed_route_expires_on_ttl() {
    let hub = RelayHub::new();
    hub.apply_route_lease("timed", &["timed/main".to_owned()], 1, 1).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));
    assert!(hub.route_for("timed/main").is_none());
}

// ---------------------------------------------------------------------------
// Symmetric NAT fallback — relay routes work without PeerConnected
// ---------------------------------------------------------------------------

#[test]
fn relay_routes_stay_relayed_on_no_peer_connected() {
    let hub = RelayHub::new();
    hub.apply_route_lease("node-m", &["node-m/main".to_owned()], 30_000, 1).unwrap();
    let route = hub.route_for("node-m/main").expect("route");
    assert_eq!(route.mode, RouteMode::Relayed);
}
