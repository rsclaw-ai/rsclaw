//! P2P direct-connection manager for A2A relay spoke nodes (ADR 0002).
//!
//! When two spoke nodes are behind NAT, the hub helps them exchange address
//! candidates (host, srflx, relay).  If a direct WS connection can be
//! established, data frames flow peer-to-peer instead of through the hub,
//! cutting latency in half.
//!
//! # Architecture
//!
//! ```text
//! Spoke A ←─WS(hub)─→ Hub ←─WS(hub)─→ Spoke B     (control channel, always)
//! Spoke A ←──────WS──────────→ Spoke B              (data channel, after punch)
//! ```
//!
//! `PeerManager` is the per-gateway singleton (one per `AppState`).
//! Inbound peer WS connections arrive at `GET /a2a/peer/ws` and are
//! registered here.  Outbound connections (spoke-initiated hole-punch
//! attempts) also land here after the WS handshake succeeds.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use anyhow::anyhow;
use axum::{
    extract::{
        Query, State,
        ws::{Message as AxumWsMessage, WebSocket, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::{
    a2a::{
        relay::{Candidate, CandidateKind, RelayFrame, RouteMode, StreamPending, validate_agent_ref},
        types::{JsonRpcRequest, JsonRpcResponse},
    },
    server::AppState,
};

/// Timeout for a peer->peer WS connect attempt during hole-punch.
const PEER_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long we cache a peer's candidate addresses before they are considered stale.
const CANDIDATE_TTL: Duration = Duration::from_secs(120);
/// Max time we wait for a peer-to-peer JSON-RPC call.
const PEER_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------
// PeerConnection — a single direct WS link to a remote spoke
// ---------------------------------------------------------------------------

/// An established direct WebSocket connection to a remote peer spoke node.
#[derive(Debug, Clone)]
struct PeerConnection {
    /// Send `RelayFrame` values to the remote peer over the direct WS.
    tx: mpsc::UnboundedSender<RelayFrame>,
    peer_node_id: String,
    connected_at: Instant,
}

// ---------------------------------------------------------------------------
// PeerManager — per-gateway singleton
// ---------------------------------------------------------------------------

/// Tracks all direct P2P connections and routes for the local gateway.
///
/// One instance lives in `AppState`.  It is populated:
/// - Inbound: when a remote spoke connects to our `GET /a2a/peer/ws`.
/// - Outbound: after we successfully hole-punch to a remote spoke.
pub struct PeerManager {
    /// peer_node_id → active direct connection
    direct_connections: DashMap<String, PeerConnection>,
    /// peer_node_id → cached candidate addresses (from PeerCandidateRelay)
    peer_candidates: DashMap<String, (Vec<Candidate>, Instant)>,
    /// agent_ref → route entry (populated after peer connection)
    routes: DashMap<String, super::relay::RouteEntry>,
    /// request_id → oneshot waiter (sync JSON-RPC)
    pending: DashMap<String, (oneshot::Sender<JsonRpcResponse>, String)>,
    /// request_id → broadcast sender (streaming RPC)
    stream_pending: DashMap<String, StreamPending>,
    /// task_id → agent_ref cache (sniffed from events/responses)
    task_routes: DashMap<String, String>,
    /// request_id → local task_id map for peer-initiated streaming tasks
    /// (mirrors RelayHub.spoke_stream_tasks for the peer channel).
    pub(crate) peer_stream_tasks: DashMap<String, String>,
    /// Request counter for generating unique request IDs.
    request_counter: AtomicU64,
}

impl Default for PeerManager {
    fn default() -> Self {
        Self {
            direct_connections: DashMap::new(),
            peer_candidates: DashMap::new(),
            routes: DashMap::new(),
            pending: DashMap::new(),
            stream_pending: DashMap::new(),
            task_routes: DashMap::new(),
            peer_stream_tasks: DashMap::new(),
            request_counter: AtomicU64::new(0),
        }
    }
}

impl PeerManager {
    // -- connection management --

    /// Register an inbound direct connection from a remote peer.
    /// Called by the peer WS handler after successful auth.
    pub fn register_connection(&self, peer_node_id: &str, tx: mpsc::UnboundedSender<RelayFrame>) {
        let old = self.direct_connections.insert(
            peer_node_id.to_owned(),
            PeerConnection {
                tx,
                peer_node_id: peer_node_id.to_owned(),
                connected_at: Instant::now(),
            },
        );
        if old.is_some() {
            info!(peer = %peer_node_id, "replaced existing peer connection");
        }
        info!(peer = %peer_node_id, "peer direct connection registered");
    }

    /// Remove a peer connection (called on WS close).
    pub fn unregister_connection(&self, peer_node_id: &str) {
        self.direct_connections.remove(peer_node_id);
        // Remove all routes belonging to this peer.
        let stale: Vec<String> = self
            .routes
            .iter()
            .filter(|entry| entry.node_id == peer_node_id)
            .map(|entry| entry.key().clone())
            .collect();
        for key in stale {
            self.routes.remove(&key);
        }
        // Resolve pending waiters.
        let pending_keys: Vec<String> = self
            .pending
            .iter()
            .filter_map(|e| (e.value().1 == peer_node_id).then(|| e.key().clone()))
            .collect();
        for k in pending_keys {
            if let Some((_, (tx, _))) = self.pending.remove(&k) {
                let _ = tx.send(JsonRpcResponse::err(
                    Value::Null,
                    -32004,
                    format!("peer '{peer_node_id}' disconnected"),
                ));
            }
        }
        info!(peer = %peer_node_id, "peer direct connection unregistered");
    }

    /// Returns true if we have a direct connection to this peer.
    pub fn has_direct_connection(&self, peer_node_id: &str) -> bool {
        self.direct_connections.contains_key(peer_node_id)
    }

    // -- candidate cache --

    /// Cache candidate addresses received from a remote peer via the hub.
    pub fn cache_candidates(&self, peer_node_id: &str, candidates: Vec<Candidate>) {
        self.peer_candidates
            .insert(peer_node_id.to_owned(), (candidates, Instant::now()));
    }

    /// Get non-expired cached candidates for a peer.
    pub fn get_candidates(&self, peer_node_id: &str) -> Option<Vec<Candidate>> {
        self.peer_candidates
            .get(peer_node_id)
            .and_then(|entry| {
                let (candidates, ts) = &*entry;
                if ts.elapsed() < CANDIDATE_TTL {
                    Some(candidates.clone())
                } else {
                    drop(entry);
                    self.peer_candidates.remove(peer_node_id);
                    None
                }
            })
    }

    // -- route management --

    /// Look up the route for an agent_ref. Checks direct peer routes first,
    /// then falls back to (external) relay hub routes.
    pub fn route_for(&self, agent_ref: &str) -> Option<super::relay::RouteEntry> {
        self.routes.get(agent_ref).map(|entry| {
            if entry.expires_at <= Instant::now() {
                drop(entry);
                self.routes.remove(agent_ref);
                return None;
            }
            Some(entry.clone())
        })?
    }

    /// Register a route via direct peer connection.
    pub fn add_route(&self, agent_ref: &str, node_id: &str) {
        self.routes.insert(
            agent_ref.to_owned(),
            super::relay::RouteEntry {
                agent_ref: agent_ref.to_owned(),
                node_id: node_id.to_owned(),
                epoch: 1,
                expires_at: Instant::now() + Duration::from_secs(300),
                mode: RouteMode::Direct,
            },
        );
    }

    // -- task routing --

    /// Record a task_id → agent_ref mapping (sniffed from responses/events).
    pub fn record_task_route(&self, task_id: &str, agent_ref: &str) {
        self.task_routes
            .insert(task_id.to_owned(), agent_ref.to_owned());
    }

    /// Look up agent_ref by task_id.
    pub fn route_for_task(&self, task_id: &str) -> Option<String> {
        self.task_routes.get(task_id).map(|e| e.clone())
    }

    // -- JSON-RPC invocation --

    /// Send a synchronous JSON-RPC request over the direct peer connection.
    pub async fn invoke_jsonrpc(
        &self,
        target: &str,
        method: &str,
        params: Value,
        principal: &str,
        peer_node_id: &str,
    ) -> anyhow::Result<JsonRpcResponse> {
        let conn = self
            .direct_connections
            .get(peer_node_id)
            .ok_or_else(|| anyhow::anyhow!("no direct connection to '{peer_node_id}'"))?;
        let request_id = format!(
            "peer:{}",
            self.request_counter.fetch_add(1, Ordering::Relaxed)
        );
        let (tx, rx) = oneshot::channel();
        self.pending
            .insert(request_id.clone(), (tx, peer_node_id.to_owned()));
        let frame = RelayFrame::Request {
            request_id: request_id.clone(),
            target: target.to_owned(),
            method: method.to_owned(),
            params,
            principal: principal.to_owned(),
            deadline_ms: PEER_REQUEST_TIMEOUT.as_millis() as u64,
        };
        if conn.tx.send(frame).is_err() {
            self.pending.remove(&request_id);
            anyhow::bail!("peer send to '{}' failed", peer_node_id);
        }
        drop(conn);
        match tokio::time::timeout(PEER_REQUEST_TIMEOUT, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err(anyhow::anyhow!("peer response channel closed")),
            Err(_) => {
                self.pending.remove(&request_id);
                Err(anyhow::anyhow!("peer request timed out"))
            }
        }
    }

    /// Complete a pending JSON-RPC call (called by the inbound frame handler).
    pub fn complete_pending(&self, request_id: &str, response: JsonRpcResponse) {
        if let Some((_, (tx, _))) = self.pending.remove(request_id) {
            let _ = tx.send(response);
        }
    }

    // -- streaming invocation --

    /// Send a streaming request over the direct peer connection.
    /// Returns (request_id, node_id, broadcast_receiver).
    pub async fn invoke_streaming(
        &self,
        target: &str,
        method: &str,
        params: Value,
        principal: &str,
        peer_node_id: &str,
    ) -> anyhow::Result<(String, String, broadcast::Receiver<Value>)> {
        let conn = self
            .direct_connections
            .get(peer_node_id)
            .ok_or_else(|| anyhow::anyhow!("no direct connection to '{peer_node_id}'"))?;
        let request_id = format!(
            "peer:stream:{}",
            self.request_counter.fetch_add(1, Ordering::Relaxed)
        );
        let (event_tx, event_rx) = broadcast::channel(128);
        self.stream_pending.insert(
            request_id.clone(),
            StreamPending {
                tx: event_tx,
                agent_ref: target.to_owned(),
                node_id: peer_node_id.to_owned(),
                deadline: Instant::now() + Duration::from_secs(1800),
            },
        );
        let frame = RelayFrame::Request {
            request_id: request_id.clone(),
            target: target.to_owned(),
            method: method.to_owned(),
            params,
            principal: principal.to_owned(),
            deadline_ms: PEER_REQUEST_TIMEOUT.as_millis() as u64,
        };
        if conn.tx.send(frame).is_err() {
            self.stream_pending.remove(&request_id);
            anyhow::bail!("peer send to '{}' failed", peer_node_id);
        }
        Ok((request_id, peer_node_id.to_owned(), event_rx))
    }

    /// Forward a streaming event to the broadcast subscriber.
    pub fn forward_stream_event(&self, request_id: &str, value: Value) -> usize {
        let Some(entry) = self.stream_pending.get(request_id) else {
            return 0;
        };
        if let Some(task_id) = value.get("taskId").and_then(|v| v.as_str()) {
            self.record_task_route(task_id, &entry.agent_ref);
        }
        entry.tx.send(value).unwrap_or(0)
    }

    /// Remove and drop a streaming entry (called when stream completes or is cancelled).
    pub fn complete_streaming(&self, request_id: &str) -> bool {
        self.stream_pending.remove(request_id).is_some()
    }

    /// Send a Cancel frame to an active streaming request on a peer.
    pub fn send_cancel_to(&self, peer_node_id: &str, request_id: &str) {
        let Some(conn) = self.direct_connections.get(peer_node_id) else {
            return;
        };
        let frame = RelayFrame::Cancel {
            request_id: request_id.to_owned(),
            task_id: None,
        };
        let _ = conn.tx.send(frame);
    }
}

// ---------------------------------------------------------------------------
// Hole-punch helper
// ---------------------------------------------------------------------------

/// Try to establish a direct WS connection to a remote peer using
/// the provided candidate addresses.
///
/// Tries candidates in priority order (highest first).  Returns the
/// URL that succeeded, or an error if all candidates failed.
///
/// This is called by the spoke's inbound frame handler when it receives
/// a `PeerCandidateRelay` frame from the hub (see `relay.rs`).
pub(crate) async fn try_hole_punch(
    state: &AppState,
    target_node: &str,
    candidates: &[Candidate],
) -> anyhow::Result<String> {
    let mut sorted: Vec<&Candidate> = candidates.iter().collect();
    sorted.sort_by_key(|c| std::cmp::Reverse(c.priority));

    // If there's already a direct connection, skip.
    if state.peer_manager.has_direct_connection(target_node) {
        return Err(anyhow::anyhow!(
            "already directly connected to '{target_node}'"
        ));
    }

    let node_id = state
        .config
        .gateway
        .a2a_relay
        .node_id
        .clone()
        .unwrap_or_default();
    let token = state
        .config
        .gateway
        .a2a_relay
        .token
        .clone()
        .unwrap_or_default();

    for candidate in sorted {
        debug!(
            kind = ?candidate.kind,
            url = %candidate.url,
            "trying peer candidate"
        );
        let mut url = candidate.url.clone();
        let sep = if url.contains('?') { '&' } else { '?' };
        url.push_str(&format!(
            "{sep}node_id={}&token={}",
            urlencoding::encode(&node_id),
            urlencoding::encode(&token),
        ));

        match tokio::time::timeout(
            PEER_CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async(&url),
        )
        .await
        {
            Ok(Ok((ws_stream, _))) => {
                let (write, read) = ws_stream.split();

                // Channel for outgoing frames to this peer.
                let (tx, mut rx) = mpsc::unbounded_channel::<RelayFrame>();

                // Register the outbound connection in PeerManager.
                let peer_mgr = state.peer_manager.clone();
                let peer_mgr_drop = state.peer_manager.clone();
                let peer_node = target_node.to_owned();
                peer_mgr.register_connection(&peer_node, tx);

                // Spawn writer task: RelayFrame → WS Text messages.
                {
                    let peer_node_w = peer_node.clone();
                    let peer_mgr_w = peer_mgr_drop.clone();
                    tokio::spawn(async move {
                        use futures::SinkExt;
                        let mut sink = write;
                        while let Some(frame) = rx.recv().await {
                            let Ok(text) = serde_json::to_string(&frame) else {
                                break;
                            };
                            if sink
                                .send(tokio_tungstenite::tungstenite::Message::Text(
                                    text.into(),
                                ))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        peer_mgr_w.unregister_connection(&peer_node_w);
                    });
                }

                // Spawn reader task: incoming WS frames → PeerManager dispatch.
                {
                    let peer_node_r = peer_node.clone();
                    let state_r = state.clone();
                    tokio::spawn(async move {
                        use futures::StreamExt;
                        let mut stream = read;
                        while let Some(msg_result) = stream.next().await {
                            let Ok(msg) = msg_result else {
                                break;
                            };
                            let tokio_tungstenite::tungstenite::Message::Text(text) = msg else {
                                continue;
                            };
                            let Ok(frame) = serde_json::from_str::<RelayFrame>(&text) else {
                                warn!(peer = %peer_node_r, "invalid peer frame");
                                continue;
                            };
                            handle_peer_frame(&state_r, &peer_node_r, frame);
                        }
                        state_r
                            .peer_manager
                            .unregister_connection(&peer_node_r);
                    });
                }

                return Ok(candidate.url.clone());
            }
            Ok(Err(e)) => {
                debug!(
                    url = %candidate.url,
                    error = %e,
                    "peer candidate failed"
                );
            }
            Err(_) => {
                debug!(url = %candidate.url, "peer candidate timed out");
            }
        }
    }

    Err(anyhow::anyhow!(
        "all {count} candidates failed for '{target_node}'",
        count = candidates.len()
    ))
}

/// Handle an inbound frame from a direct peer connection.
///
/// After hole-punch succeeds, the peer WS carries the same `RelayFrame`
/// protocol as the hub-spoke channel.  Inbound `Request` frames are
/// dispatched locally (same code path as `handle_spoke_request`);
/// `Response`/`Event` frames complete pending outbound calls in
/// `PeerManager`.
pub(crate) fn handle_peer_frame(state: &AppState, peer_node_id: &str, frame: RelayFrame) {
    match frame {
        RelayFrame::Request {
            request_id,
            target,
            method,
            params,
            principal,
            ..
        } => {
            // Inbound request from the remote peer — dispatch locally.
            // We need to route the response back through the peer WS,
            // not through the relay hub.
            let spoke_tx = {
                // Get the peer connection's tx so we can send response back.
                match state.peer_manager.direct_connections.get(peer_node_id) {
                    Some(conn) => conn.tx.clone(),
                    None => {
                        warn!(peer = %peer_node_id, "peer request arrived after disconnect");
                        return;
                    }
                }
            };
            let state_clone = state.clone();
            let peer_node = peer_node_id.to_owned();
            tokio::spawn(async move {
                let response = handle_peer_spoke_request(
                    &state_clone,
                    &peer_node,
                    &request_id,
                    &target,
                    &method,
                    params,
                    principal,
                    spoke_tx.clone(),
                )
                .await;
                if let Some(response) = response {
                    let _ = spoke_tx.send(RelayFrame::Response {
                        request_id,
                        response,
                    });
                }
            });
        }
        RelayFrame::Response {
            request_id,
            response,
        } => {
            if !state.peer_manager.complete_streaming(&request_id) {
                state.peer_manager.complete_pending(&request_id, response);
            }
        }
        RelayFrame::Event {
            request_id, result, ..
        } => {
            if state.peer_manager.forward_stream_event(&request_id, result) == 0 {
                debug!(request_id, "peer event for unknown stream");
            }
        }
        RelayFrame::Cancel { request_id, .. } => {
            // Cancel a peer-initiated streaming task. The tracking map
            // is per-PeerManager, mirroring RelayHub.spoke_stream_tasks.
            if let Some((_, task_id)) =
                state.peer_manager.peer_stream_tasks.remove(&request_id)
            {
                if let Some((_, token)) = state.task_cancels.remove(&task_id) {
                    token.cancel();
                }
            }
        }
        RelayFrame::Ping { ts } => {
            if let Some(conn) = state.peer_manager.direct_connections.get(peer_node_id) {
                let _ = conn.tx.send(RelayFrame::Pong { ts });
            }
        }
        RelayFrame::Pong { .. } => {}
        RelayFrame::RouteLease { agents, .. } => {
            // Peer advertises its local agents. Register routes in PeerManager.
            for agent_ref in &agents {
                state.peer_manager.add_route(agent_ref, peer_node_id);
            }
            debug!(
                peer = %peer_node_id,
                agents = agents.len(),
                "peer route lease registered"
            );
        }
        RelayFrame::Hello { .. } => {
            // Peer handshake — no-op for now (ADR Phase 5 adds AgentCard exchange).
        }
        other => {
            debug!(
                peer = %peer_node_id,
                frame = ?other,
                "peer frame ignored"
            );
        }
    }
}

/// Handle a `RelayFrame::Request` received over a direct peer connection.
/// Same logic as `handle_spoke_request` in relay.rs, but the response goes
/// back through the peer WS instead of the relay spoke_tx.
async fn handle_peer_spoke_request(
    state: &AppState,
    peer_node_id: &str,
    request_id: &str,
    target: &str,
    method: &str,
    params: Value,
    principal: String,
    spoke_tx: mpsc::UnboundedSender<RelayFrame>,
) -> Option<JsonRpcResponse> {
    let Some(local_agent) = super::relay::local_agent_from_ref(target, peer_node_id) else {
        return Some(JsonRpcResponse::err(
            Value::Null,
            -32003,
            format!("target not hosted here: {target}"),
        ));
    };
    let mut params = params;
    if let Some(metadata) = params.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        metadata.insert("agentId".to_owned(), Value::String(local_agent));
    }

    if method == "SendStreamingMessage" || method == "SubscribeToTask" {
        let caller = Some(super::auth::A2aIdentity {
            id: principal,
            scopes: Vec::new(),
        });
        let (task_id, event_rx) =
            super::streaming::spawn_streaming_task(state.clone(), caller, params).await;
        let request_id_for_relay = request_id.to_owned();
        state
            .peer_manager
            .peer_stream_tasks
            .insert(request_id_for_relay.clone(), task_id.clone());
        let peer_mgr = state.peer_manager.clone();
        tokio::spawn(async move {
            use futures::StreamExt;
            use tokio_stream::wrappers::BroadcastStream;

            let mut seq = 0u64;
            let mut stream = BroadcastStream::new(event_rx);
            while let Some(result) = stream.next().await {
                match result {
                    Ok(event) => {
                        let wire = event.to_wire_event();
                        if spoke_tx
                            .send(RelayFrame::Event {
                                request_id: request_id_for_relay.clone(),
                                seq,
                                result: wire,
                            })
                            .is_err()
                        {
                            break;
                        }
                        seq += 1;
                        if event.is_final() {
                            break;
                        }
                    }
                    Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                        warn!(lagged = n, "peer relay event lagged");
                    }
                }
            }
            peer_mgr
                .peer_stream_tasks
                .remove(&request_id_for_relay);
            let _ = spoke_tx.send(RelayFrame::Response {
                request_id: request_id_for_relay,
                response: JsonRpcResponse::ok(
                    Value::String(task_id),
                    serde_json::json!({"ok": true}),
                ),
            });
        });
        return None;
    }

    let req = JsonRpcRequest {
        jsonrpc: "2.0".to_owned(),
        id: Value::String(format!("peer:{}", uuid::Uuid::new_v4())),
        method: method.to_owned(),
        params,
    };
    let caller = Some(super::auth::A2aIdentity {
        id: principal,
        scopes: Vec::new(),
    });
    Some(
        super::server::a2a_rpc_handler_inner(state.clone(), caller, req)
            .await
            .0,
    )
}

// ---------------------------------------------------------------------------
// Local interface helper
// ---------------------------------------------------------------------------

/// Enumerate local network interface IP addresses.
/// Returns a Vec of (interface_name, ip_string).
/// Uses platform-specific shell commands (parity with crate::cmd::setup::detect_lan_ips).
pub(crate) fn local_ips() -> Vec<(String, String)> {
    let mut ips = Vec::new();

    if cfg!(windows) {
        #[allow(unused_mut)]
        let mut ipc = std::process::Command::new("ipconfig");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            ipc.creation_flags(0x08000000);
        }
        if let Ok(output) = ipc.output() {
            let text = String::from_utf8_lossy(&output.stdout);
            for line in text.lines() {
                let trimmed = line.trim();
                if let Some(pos) = trimmed.find(": ") {
                    let ip = trimmed[pos + 2..].trim();
                    if ip.contains('.') && !ip.starts_with("127.") && !ip.starts_with("169.254.") {
                        ips.push(("eth".to_owned(), ip.to_owned()));
                    }
                }
            }
        }
    } else {
        // macOS / Linux: ifconfig
        if let Ok(output) = std::process::Command::new("ifconfig").output() {
            let text = String::from_utf8_lossy(&output.stdout);
            let mut current_iface = String::new();
            for line in text.lines() {
                let trimmed = line.trim();
                if !trimmed.starts_with('\t') && !trimmed.starts_with(' ') {
                    // Interface header line.
                    if let Some(iface) = trimmed.split(':').next() {
                        current_iface = iface.to_owned();
                    }
                } else if let Some(rest) = trimmed.strip_prefix("inet ") {
                    let ip = rest.split_whitespace().next().unwrap_or("");
                    if !ip.starts_with("127.") && !ip.starts_with("169.254.") && !ip.is_empty() {
                        ips.push((current_iface.clone(), ip.to_owned()));
                    }
                }
            }
        }
        // Fallback for Linux: ip addr
        if ips.is_empty() {
            if let Ok(output) = std::process::Command::new("ip")
                .args(["addr", "show"])
                .output()
            {
                let text = String::from_utf8_lossy(&output.stdout);
                for line in text.lines() {
                    let trimmed = line.trim();
                    if let Some(rest) = trimmed.strip_prefix("inet ") {
                        let ip = rest.split('/').next().unwrap_or("");
                        if !ip.starts_with("127.") && !ip.starts_with("169.254.") && !ip.is_empty()
                        {
                            ips.push(("eth".to_owned(), ip.to_owned()));
                        }
                    }
                }
            }
        }
    }
    if ips.is_empty() {
        ips.push(("lo".to_owned(), "127.0.0.1".to_owned()));
    }
    ips
}

/// Collect address candidates for hole-punching (ADR 0002 §Step 1).
///
/// Returns:
/// - host candidates: WS URLs for each local non-loopback IP
/// - srflx candidates: STUN-derived public addresses
pub fn collect_candidates(
    host_port: u16,
    stun_urls: &[String],
) -> Vec<Candidate> {
    let mut candidates = Vec::new();

    // 1. Host candidates — local IPs.
    for (_iface, ip) in local_ips() {
        candidates.push(Candidate {
            kind: CandidateKind::Host,
            url: format!("ws://{ip}:{host_port}/a2a/peer/ws"),
            priority: 100,
        });
    }

    // 2. srflx candidates from STUN.
    let srflx = super::stun::gather_srflx_candidates(
        stun_urls,
        std::time::Duration::from_secs(2),
    );
    for (public_ip, public_port) in srflx {
        candidates.push(Candidate {
            kind: CandidateKind::Srflx,
            url: format!("ws://{public_ip}:{public_port}/a2a/peer/ws"),
            priority: 90,
        });
    }

    candidates
}

// ---------------------------------------------------------------------------
// Peer WebSocket handler — GET /a2a/peer/ws
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct PeerWsQuery {
    node_id: String,
    #[serde(default)]
    token: Option<String>,
}

/// WebSocket upgrade handler for `/a2a/peer/ws` (ADR 0002).
///
/// Called when a remote spoke initiates a direct P2P connection to us.
/// Auth uses the same relay token we use to connect to the hub (shared
/// secret between spoke nodes). Only spoke-mode nodes accept peer WS
/// connections.
pub(crate) async fn peer_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(query): Query<PeerWsQuery>,
    headers: HeaderMap,
) -> Response {
    let relay = &state.config.gateway.a2a_relay;

    // Only spoke-mode nodes accept direct peer connections.
    let is_spoke = matches!(
        relay.mode,
        rsclaw_config::runtime::A2aRelayModeRuntime::Spoke
    );
    if !is_spoke {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    }

    // Peer WS auth: the remote spoke must present a token matching our
    // own relay token (shared secret). node_id identifies the caller
    // but is not verified against a node list (spokes don't have one).
    let presented = query
        .token
        .as_deref()
        .or_else(|| bearer_token_peer(&headers));

    let valid = match relay.token.as_deref() {
        Some(our_token) if !our_token.is_empty() => {
            presented.is_some_and(|t| crate::server::constant_time_eq(our_token, t))
        }
        _ => false,
    };

    if !valid {
        tracing::warn!(
            peer_node = %query.node_id,
            "peer WS auth rejected: token mismatch or not configured"
        );
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    ws.on_upgrade(move |socket| handle_peer_socket(socket, state, query.node_id))
}

fn bearer_token_peer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

async fn handle_peer_socket(socket: WebSocket, state: AppState, peer_node_id: String) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<RelayFrame>();

    // Register in PeerManager.
    state.peer_manager.register_connection(&peer_node_id, tx);
    info!(peer = %peer_node_id, "peer direct connection established (inbound)");

    // Advertise local agents to the peer via RouteLease.
    let agents: Vec<String> = state
        .agents
        .all()
        .into_iter()
        .map(|agent| format!("{peer_node_id}/{}", agent.id))
        .collect();
    if let Some(conn) = state.peer_manager.direct_connections.get(&peer_node_id) {
        let _ = conn.tx.send(RelayFrame::RouteLease {
            node_id: state
                .config
                .gateway
                .a2a_relay
                .node_id
                .clone()
                .unwrap_or_default(),
            agents: agents.clone(),
            ttl_ms: 30_000,
            epoch: 1,
        });
    }

    // Register routes in PeerManager.
    for agent_ref in &agents {
        state.peer_manager.add_route(agent_ref, &peer_node_id);
    }

    // Writer: RelayFrame → WS Text.
    let writer_peer = peer_node_id.clone();
    let peer_mgr_w = state.peer_manager.clone();
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let Ok(text) = serde_json::to_string(&frame) else {
                break;
            };
            if sink
                .send(AxumWsMessage::Text(text.into()))
                .await
                .is_err()
            {
                break;
            }
        }
        peer_mgr_w.unregister_connection(&writer_peer);
    });

    // Reader: WS Text → handle_peer_frame.
    while let Some(msg) = stream.next().await {
        let Ok(msg) = msg else {
            break;
        };
        let AxumWsMessage::Text(text) = msg else {
            continue;
        };
        let Ok(frame) = serde_json::from_str::<RelayFrame>(&text) else {
            warn!(peer = %peer_node_id, "invalid peer frame");
            continue;
        };
        handle_peer_frame(&state, &peer_node_id, frame);
    }

    state.peer_manager.unregister_connection(&peer_node_id);
    info!(peer = %peer_node_id, "peer direct connection closed (inbound)");
}
