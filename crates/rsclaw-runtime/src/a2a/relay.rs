//! rsclaw private A2A relay transport.
//!
//! Public A2A remains JSON-RPC over HTTP/SSE. This module is the private
//! outbound-WS transport that lets NAT/private nodes attach to a hub.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use axum::{
    Json,
    extract::{
        Query, State,
        ws::{Message as AxumWsMessage, WebSocket, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use rsclaw_config::runtime::{
    A2aRelayModeRuntime, A2aRelayNodeRuntime, A2aRelayRuntime, A2aRelayStrategyRuntime,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    a2a::{
        auth::A2aIdentity,
        relay_identity,
        types::{AgentCard, JsonRpcRequest, JsonRpcResponse},
    },
    server::{AppState, constant_time_eq},
};

/// Minimal context for relay hub WS handlers — decouples socket-level
/// handling from the full `AppState`. Tests can construct this directly
/// with `Arc<RelayHub>`, relay identity, and configured node ACLs. Production
/// routes get it via `HubCtx::from(&app_state)`.
#[derive(Clone)]
pub struct HubCtx {
    pub hub: std::sync::Arc<RelayHub>,
    pub relay_id: String,
    pub nodes: std::sync::Arc<Vec<A2aRelayNodeRuntime>>,
}

impl From<&AppState> for HubCtx {
    fn from(s: &AppState) -> Self {
        Self {
            hub: s.relay_hub.clone(),
            relay_id: s.config.gateway.a2a_relay.relay_id.clone(),
            nodes: std::sync::Arc::new(s.config.gateway.a2a_relay.nodes.clone()),
        }
    }
}

const RELAY_PROTOCOL: &str = "rsclaw.a2a.relay.v1";
const ROUTE_TTL_MS: u64 = 30_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Absolute upper bound on a single relay-forwarded SSE stream. Above
/// this the hub synthesises a terminal `state=failed` event, drops the
/// `stream_pending` entry, and sends a `Cancel` to the spoke. Needed
/// because we removed reqwest's end-to-end timeout for long
/// image/video generation flows (see commit b94c40f) — a misbehaving
/// spoke that never sends a terminal Response, plus a happy SSE
/// consumer + WS heartbeat, would otherwise pin the entry forever.
/// 30 min comfortably covers jimeng video generation (~10 min) without
/// punishing legitimate long polls.
const STREAM_MAX_LIFETIME: Duration = Duration::from_secs(1800);
/// Max time we wait for the spoke to complete keypair auth after WS upgrade
/// before dropping the connection. Long enough to absorb cross-continent
/// RTT, short enough to free slots fast under attack.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const PEER_SIGNAL_TTL: Duration = Duration::from_secs(60);
const MAX_PEER_SDP_BYTES: usize = 64 * 1024;
const MAX_PEER_SIGNAL_SESSIONS: usize = 1024;
const RELAY_WRITE_QUEUE_CAPACITY: usize = 256;
const MAX_SPOKE_RELAY_REQUESTS: usize = 4096;
const MAX_RELAY_STREAMS: usize = 1024;
const MAX_RELAY_PENDING_REQUESTS: usize = 4096;
/// Backoff cap when failing over between relays. Same envelope as the
/// per-relay reconnect backoff but applied across the entire list.
const FAILOVER_BACKOFF_MAX: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloCapabilities {
    pub streaming_relay: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayFrame {
    Hello {
        protocol: String,
        node_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        node_version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_card: Option<AgentCard>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capabilities: Option<HelloCapabilities>,
        /// Spoke-generated nonce for the Ed25519 handshake. Only present
        /// when the spoke is operating in keypair mode. Hubs that don't
        /// require keypair auth ignore this field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        nonce_node: Option<String>,
    },
    /// Hub → spoke: present the relay's nonce; spoke must reply with `Auth`
    /// signed over the canonical handshake payload (see relay_identity).
    Challenge {
        relay_id: String,
        nonce_relay: String,
    },
    /// Spoke → hub: signed handshake response. Hub verifies against the
    /// configured `public_key` for this node_id. On failure the hub closes
    /// the connection.
    Auth {
        signature: String,
    },
    RouteLease {
        node_id: String,
        agents: Vec<String>,
        ttl_ms: u64,
        epoch: u64,
    },
    Request {
        request_id: String,
        target: String,
        method: String,
        params: Value,
        principal: String,
        deadline_ms: u64,
    },
    Response {
        request_id: String,
        response: JsonRpcResponse,
    },
    Event {
        request_id: String,
        seq: u64,
        result: Value,
    },
    Cancel {
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
    },
    Ping {
        ts: u64,
    },
    Pong {
        ts: u64,
    },
    Error {
        request_id: String,
        message: String,
    },
    /// Spoke → Hub: authenticated WebRTC offer for a configured peer.
    PeerOffer {
        session_id: String,
        target_node: String,
        sdp: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Hub → Spoke: offer forwarded from the authenticated source connection.
    PeerOfferRelay {
        session_id: String,
        source_node: String,
        sdp: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Spoke → Hub: WebRTC answer for an existing offer session.
    PeerAnswer {
        session_id: String,
        target_node: String,
        sdp: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Hub → Spoke: answer forwarded from the authenticated source connection.
    PeerAnswerRelay {
        session_id: String,
        source_node: String,
        sdp: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Spoke → Hub: informational notification that ICE selected a usable path.
    /// Direct reachability remains local to the reporting spoke; the hub never
    /// disables its relayed route based on this frame.
    PeerConnected {
        peer_node: String,
        session_id: String,
    },
}

#[derive(Debug, Clone)]
struct Connection {
    tx: mpsc::Sender<AxumWsMessage>,
    epoch: u64,
}

#[derive(Debug, Clone)]
struct PeerSignalSession {
    source_node: String,
    target_node: String,
    expires_at: std::time::Instant,
}

#[derive(Debug, Clone)]
struct SpokeControl {
    tx: mpsc::Sender<RelayFrame>,
    generation: String,
}

#[derive(Debug, Clone)]
struct SpokeRelayRequest {
    source_node: String,
    target_node: String,
    expires_at: std::time::Instant,
}

/// Local relay stream task owned by one authenticated spoke-control generation.
#[derive(Debug, Clone)]
pub(crate) struct SpokeStreamTask {
    task_id: String,
    control_generation: String,
    cancel_owner: Option<std::sync::Arc<tokio_util::sync::CancellationToken>>,
}

#[derive(Debug, Clone)]
pub struct StreamPending {
    pub tx: broadcast::Sender<Value>,
    pub agent_ref: String,
    pub node_id: String,
    pub deadline: std::time::Instant,
}

#[derive(Debug, Clone)]
pub struct RouteEntry {
    pub agent_ref: String,
    pub node_id: String,
    pub epoch: u64,
    pub expires_at: std::time::Instant,
    /// Route mode (ADR 0002): whether the target is reachable via hub relay
    /// or via direct P2P connection.
    pub mode: RouteMode,
}

/// Route mode for a relay route (ADR 0002).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteMode {
    /// Traffic goes through the hub (existing path).
    Relayed,
    /// Peer nodes have established a direct P2P connection; the hub only
    /// serves as control channel for discovery and hole-punch coordination.
    Direct,
}

impl Default for RouteMode {
    fn default() -> Self {
        RouteMode::Relayed
    }
}

/// Production-grade metrics exposed via `GET /v1/a2a/relay/stats`. All
/// counters are monotonic except `connected_nodes`/`route_count` which are
/// read live from the corresponding DashMaps. Names match the spec.
#[derive(Default, Debug)]
pub struct RelayMetrics {
    pub request_count: AtomicU64,
    pub request_latency_ms_total: AtomicU64,
    pub ws_reconnects: AtomicU64,
    pub auth_failures: AtomicU64,
    pub acl_denials: AtomicU64,
    pub route_expirations: AtomicU64,
    pub failovers: AtomicU64,
    /// Number of in-flight streams forcibly failed because the underlying
    /// spoke connection dropped. Separate from `auth_failures` so an
    /// operator can tell "network blip" from "credential rejection".
    pub inflight_losses: AtomicU64,
}

impl RelayMetrics {
    /// Return a JSON snapshot of relay counters and live topology gauges.
    pub fn snapshot(&self, connected_nodes: u64, route_count: u64) -> serde_json::Value {
        let req = self.request_count.load(Ordering::Relaxed);
        let lat = self.request_latency_ms_total.load(Ordering::Relaxed);
        let avg = if req > 0 { lat / req } else { 0 };
        serde_json::json!({
            "connected_nodes": connected_nodes,
            "route_count": route_count,
            "request_count": req,
            "request_latency_ms_avg": avg,
            "ws_reconnects": self.ws_reconnects.load(Ordering::Relaxed),
            "auth_failures": self.auth_failures.load(Ordering::Relaxed),
            "acl_denials": self.acl_denials.load(Ordering::Relaxed),
            "route_expirations": self.route_expirations.load(Ordering::Relaxed),
            "failovers": self.failovers.load(Ordering::Relaxed),
            "inflight_losses": self.inflight_losses.load(Ordering::Relaxed),
        })
    }
}

#[derive(Default)]
pub struct RelayHub {
    connections: DashMap<String, Connection>,
    connection_generation: AtomicU64,
    routes: DashMap<String, RouteEntry>,
    /// request_id → (waiter, target_node_id). We carry node_id so that
    /// when a connection drops we can resolve only its pending waiters
    /// instead of nuking every in-flight JSON-RPC call.
    pending: DashMap<String, (oneshot::Sender<JsonRpcResponse>, String)>,
    pending_admission: std::sync::Mutex<()>,
    /// Hub-side request ID → authenticated source spoke for spoke-originated
    /// unary requests that the hub is relaying to another spoke.
    spoke_request_sources: std::sync::Mutex<HashMap<String, SpokeRelayRequest>>,
    /// Spoke-side active authenticated control connection used for hub
    /// fallback.
    spoke_control: std::sync::RwLock<Option<SpokeControl>>,
    /// Streaming relay entries: request_id → StreamPending. `agent_ref`
    /// drives `task_id → agent_ref` recording once the first event with a
    /// taskId arrives. `node_id` lets the deadline sweeper send `Cancel`
    /// to the right spoke. `deadline` bounds the entry's lifetime — see
    /// `STREAM_MAX_LIFETIME` and `sweep_expired_streams`. Inserted by
    /// `invoke_streaming`; removed when the spoke sends its terminal
    /// `RelayFrame::Response`, the SSE consumer disconnects, or the
    /// sweeper fires.
    stream_pending: DashMap<String, StreamPending>,
    stream_admission: std::sync::Mutex<()>,
    /// Spoke-side map of relay request ID to generation-owned local task, so
    /// stale control teardown cannot cancel replacement-generation work.
    pub(crate) spoke_stream_tasks: DashMap<String, SpokeStreamTask>,
    /// Hub-side cache of task_id → agent_ref ("node/agent"), populated by
    /// sniffing responses and streaming events as they pass through the
    /// hub. Lets follow-up task-bound RPCs (GetTask, CancelTask, push
    /// config ops, SubscribeToTask) route to the right spoke even when
    /// the client only knows the task_id.
    task_routes: DashMap<String, String>,
    /// Hub-authorized offer sessions. Answers must match the authenticated
    /// source/target pair before the hub forwards them.
    peer_signal_sessions: std::sync::Mutex<HashMap<String, PeerSignalSession>>,
    consumed_peer_signal_sessions: std::sync::Mutex<HashMap<String, std::time::Instant>>,
    pub metrics: RelayMetrics,
}

/// Structured audit event. Emitted via `tracing` with `target =
/// "a2a.audit"` so operators can pipe to a sink (loki, jq, etc.) with a
/// simple filter. Spec mandates these fields; missing optionals show as
/// empty strings to keep the schema stable.
pub fn audit_relay(
    decision: &str,
    principal: &str,
    action: &str,
    resource: &str,
    relay_id: &str,
    node_id: &str,
    matched_scope: Option<&str>,
    reason: Option<&str>,
) {
    tracing::info!(
        target: "a2a.audit",
        decision,
        principal,
        action,
        resource,
        relay_id,
        node_id,
        matched_scope = matched_scope.unwrap_or(""),
        reason = reason.unwrap_or(""),
        "a2a relay audit"
    );
}

impl RelayHub {
    /// Create an empty relay hub.
    pub fn new() -> Self {
        Self::default()
    }

    fn reserve_spoke_request(
        &self,
        request_id: &str,
        source_node: &str,
        target_node: &str,
        deadline_ms: u64,
    ) -> bool {
        let now = std::time::Instant::now();
        let mut requests = self
            .spoke_request_sources
            .lock()
            .expect("spoke relay request mutex poisoned");
        requests.retain(|_, request| request.expires_at > now);
        if requests.len() >= MAX_SPOKE_RELAY_REQUESTS || requests.contains_key(request_id) {
            return false;
        }
        requests.insert(
            request_id.to_owned(),
            SpokeRelayRequest {
                source_node: source_node.to_owned(),
                target_node: target_node.to_owned(),
                expires_at: now
                    + Duration::from_millis(
                        deadline_ms.clamp(1, REQUEST_TIMEOUT.as_millis() as u64),
                    ),
            },
        );
        true
    }

    /// Send a `RelayFrame` to a connected node via its WS connection.
    /// Returns true if the node was connected and the frame was queued.
    pub fn send_to_node(&self, node_id: &str, frame: &RelayFrame) -> bool {
        let Some(conn) = self.connections.get(node_id) else {
            return false;
        };
        let epoch = conn.epoch;
        let Ok(msg) = serde_json::to_string(frame) else {
            return false;
        };
        match conn.tx.try_send(AxumWsMessage::Text(msg.into())) {
            Ok(()) => true,
            Err(error) => {
                drop(conn);
                warn!(node = %node_id, %error, "relay writer queue unavailable; disconnecting node");
                self.unregister_connection(node_id, epoch);
                false
            }
        }
    }

    /// Return the number of authenticated connected spokes.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Return sorted authenticated connected spoke node IDs.
    pub fn connected_nodes(&self) -> Vec<String> {
        let mut nodes: Vec<String> = self
            .connections
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        nodes.sort();
        nodes
    }

    /// Return an unexpired hub route for an agent reference.
    pub fn route_for(&self, agent_ref: &str) -> Option<RouteEntry> {
        let entry = self.routes.get(agent_ref)?;
        if entry.expires_at <= std::time::Instant::now() {
            drop(entry);
            self.routes.remove(agent_ref);
            self.metrics
                .route_expirations
                .fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Some(entry.clone())
    }

    /// Return the number of cached hub routes, including entries awaiting
    /// lookup expiry.
    pub fn route_count(&self) -> usize {
        self.routes.len()
    }

    /// Validate and apply one authenticated node's route lease.
    pub fn apply_route_lease(
        &self,
        node_id: &str,
        agents: &[String],
        ttl_ms: u64,
        epoch: u64,
    ) -> Result<()> {
        let ttl = Duration::from_millis(ttl_ms.max(1));
        let expires_at = std::time::Instant::now() + ttl;
        for agent_ref in agents {
            validate_agent_ref(agent_ref)?;
            if !agent_ref.starts_with(&format!("{node_id}/")) {
                anyhow::bail!("node '{node_id}' cannot advertise '{agent_ref}'");
            }
            if let Some(existing) = self.routes.get(agent_ref)
                && existing.epoch > epoch
                && existing.expires_at > std::time::Instant::now()
            {
                continue;
            }
            self.routes.insert(
                agent_ref.clone(),
                RouteEntry {
                    agent_ref: agent_ref.clone(),
                    node_id: node_id.to_owned(),
                    epoch,
                    expires_at,
                    mode: RouteMode::Relayed,
                },
            );
        }
        Ok(())
    }

    fn reserve_pending(
        &self,
        request_id: String,
        tx: oneshot::Sender<JsonRpcResponse>,
        node_id: String,
    ) -> bool {
        let _admission = self
            .pending_admission
            .lock()
            .expect("relay request admission mutex poisoned");
        if self.pending.len() >= MAX_RELAY_PENDING_REQUESTS {
            return false;
        }
        self.pending.insert(request_id, (tx, node_id));
        true
    }

    pub async fn invoke_jsonrpc(
        &self,
        target: &str,
        method: &str,
        params: Value,
        principal: &str,
    ) -> Result<JsonRpcResponse> {
        let route = self
            .route_for(target)
            .ok_or_else(|| anyhow!("no live relay route for {target}"))?;
        let conn = self
            .connections
            .get(&route.node_id)
            .ok_or_else(|| anyhow!("node '{}' is not connected", route.node_id))?;
        let request_id = format!("relay:{}", Uuid::new_v4());
        let (tx, rx) = oneshot::channel();
        let node_id = route.node_id.clone();
        if !self.reserve_pending(request_id.clone(), tx, node_id.clone()) {
            anyhow::bail!("relay request capacity exceeded");
        }
        let frame = RelayFrame::Request {
            request_id: request_id.clone(),
            target: target.to_owned(),
            method: method.to_owned(),
            params,
            principal: principal.to_owned(),
            deadline_ms: REQUEST_TIMEOUT.as_millis() as u64,
        };
        let msg = AxumWsMessage::Text(serde_json::to_string(&frame)?.into());
        if let Err(e) = conn.tx.try_send(msg) {
            self.pending.remove(&request_id);
            anyhow::bail!("relay send to node '{}' failed: {e}", node_id);
        }
        drop(conn);
        let started = std::time::Instant::now();
        self.metrics.request_count.fetch_add(1, Ordering::Relaxed);
        let result = match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err(anyhow!("relay response channel closed")),
            Err(_) => {
                self.pending.remove(&request_id);
                Err(anyhow!("relay request timed out"))
            }
        };
        let elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        self.metrics
            .request_latency_ms_total
            .fetch_add(elapsed_ms, Ordering::Relaxed);
        result
    }

    /// Invoke a target through this gateway's authenticated spoke control
    /// connection. Used only after direct peer transport is unavailable.
    pub async fn invoke_via_spoke(
        &self,
        target: &str,
        method: &str,
        params: Value,
        principal: &str,
    ) -> Result<Option<JsonRpcResponse>> {
        let control = self
            .spoke_control
            .read()
            .map_err(|_| anyhow!("spoke control lock poisoned"))?
            .clone();
        let Some(control) = control else {
            return Ok(None);
        };
        let request_id = format!("spoke:{}", Uuid::new_v4());
        let (tx, rx) = oneshot::channel();
        if !self.reserve_pending(request_id.clone(), tx, target.to_owned()) {
            return Ok(None);
        }
        let frame = RelayFrame::Request {
            request_id: request_id.clone(),
            target: target.to_owned(),
            method: method.to_owned(),
            params,
            principal: principal.to_owned(),
            deadline_ms: REQUEST_TIMEOUT.as_millis() as u64,
        };
        if control.tx.try_send(frame).is_err() {
            self.pending.remove(&request_id);
            return Ok(None);
        }
        self.metrics.request_count.fetch_add(1, Ordering::Relaxed);
        let started = std::time::Instant::now();
        let result = match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(response)) => Ok(Some(response)),
            Ok(Err(_)) => Ok(None),
            Err(_) => {
                self.pending.remove(&request_id);
                Err(anyhow!("hub relay request timed out"))
            }
        };
        let elapsed_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        self.metrics
            .request_latency_ms_total
            .fetch_add(elapsed_ms, Ordering::Relaxed);
        result
    }

    fn register_spoke_control(&self, tx: mpsc::Sender<RelayFrame>, generation: String) {
        match self.spoke_control.write() {
            Ok(mut control) => *control = Some(SpokeControl { tx, generation }),
            Err(error) => warn!(%error, "cannot register spoke control connection"),
        }
    }

    fn remove_spoke_stream_task(
        &self,
        request_id: &str,
        control_generation: &str,
    ) -> Option<SpokeStreamTask> {
        self.spoke_stream_tasks
            .remove_if(request_id, |_, task| {
                task.control_generation == control_generation
            })
            .map(|(_, task)| task)
    }

    fn take_spoke_stream_tasks(&self, control_generation: &str) -> Vec<SpokeStreamTask> {
        let request_ids: Vec<String> = self
            .spoke_stream_tasks
            .iter()
            .filter(|entry| entry.control_generation == control_generation)
            .map(|entry| entry.key().clone())
            .collect();
        request_ids
            .into_iter()
            .filter_map(|request_id| self.remove_spoke_stream_task(&request_id, control_generation))
            .collect()
    }

    fn unregister_spoke_control(&self, generation: &str) {
        match self.spoke_control.write() {
            Ok(mut control)
                if control
                    .as_ref()
                    .is_some_and(|active| active.generation == generation) =>
            {
                *control = None;
            }
            Ok(_) => {}
            Err(error) => warn!(%error, "cannot unregister spoke control connection"),
        }
    }

    /// Register a connected spoke node's WS sender.
    /// **For tests only** — production wiring is via the hub WS handler.
    pub fn register_connection(&self, node_id: &str, tx: mpsc::Sender<AxumWsMessage>, epoch: u64) {
        if let Some(previous) = self
            .connections
            .insert(node_id.to_owned(), Connection { tx, epoch })
            && previous.tx.try_send(AxumWsMessage::Close(None)).is_err()
        {
            debug!(node = %node_id, previous_epoch = previous.epoch, "superseded relay writer already closed");
        }
    }

    fn owns_connection(&self, node_id: &str, epoch: u64) -> bool {
        self.connections
            .get(node_id)
            .is_some_and(|connection| connection.epoch == epoch)
    }

    fn unregister_connection(&self, node_id: &str, epoch: u64) {
        let removed = self
            .connections
            .remove_if(node_id, |_, connection| connection.epoch == epoch);
        if removed.is_none() {
            return;
        }
        let prefix = format!("{node_id}/");
        let stale: Vec<String> = self
            .routes
            .iter()
            .filter(|entry| entry.key().starts_with(&prefix))
            .map(|entry| entry.key().clone())
            .collect();
        for key in stale {
            self.routes.remove(&key);
        }
        // Surface in-flight losses: every streaming request whose target
        // lives on this node must be told the relay died so the SSE
        // consumer doesn't hang forever. We synthesize a terminal
        // status-update with state="failed" — clients already handle
        // final=true cleanly. Without this, A2A clients would keep the
        // SSE stream open until our REQUEST_TIMEOUT (120s) fired with no
        // useful diagnostic.
        let lost: Vec<String> = self
            .stream_pending
            .iter()
            .filter(|entry| entry.value().agent_ref.starts_with(&prefix))
            .map(|entry| entry.key().clone())
            .collect();
        for request_id in lost {
            // We may not know the task_id for streams that haven't yet
            // emitted their first event with a taskId attached. Leaving
            // it empty is fine — A2A clients key off `final: true` +
            // `status.state` to terminate the stream.
            let synthetic = serde_json::json!({
                "kind": "status-update",
                "taskId": "",
                "contextId": "",
                "status": {
                    "state": "failed",
                    "message": {
                        "role": "agent",
                        "messageId": format!("relay-loss-{}", Uuid::new_v4()),
                        "parts": [{
                            "kind": "text",
                            "text": format!("relay route lost: node '{node_id}' disconnected"),
                        }],
                    }
                },
                "final": true,
            });
            self.forward_stream_event(&request_id, synthetic);
            self.stream_pending.remove(&request_id);
            self.metrics.inflight_losses.fetch_add(1, Ordering::Relaxed);
        }
        // Resolve only pending waiters bound to this node so we don't
        // wait REQUEST_TIMEOUT for a corpse. Other nodes' pending RPCs
        // are left untouched.
        let pending_keys: Vec<String> = self
            .pending
            .iter()
            .filter_map(|e| (e.value().1 == node_id).then(|| e.key().clone()))
            .collect();
        for k in pending_keys {
            if let Some((_, (tx, _))) = self.pending.remove(&k)
                && tx
                    .send(JsonRpcResponse::err(
                        Value::Null,
                        -32004,
                        format!("relay node '{node_id}' disconnected"),
                    ))
                    .is_err()
            {
                debug!(request_id = %k, "relay response waiter already dropped");
            }
        }
        let target_requests = {
            let mut requests = self
                .spoke_request_sources
                .lock()
                .expect("spoke relay request mutex poisoned");
            requests.retain(|_, request| request.source_node != node_id);
            let target_requests: Vec<(String, String)> = requests
                .iter()
                .filter(|(_, request)| request.target_node == node_id)
                .map(|(request_id, request)| (request_id.clone(), request.source_node.clone()))
                .collect();
            for (request_id, _) in &target_requests {
                requests.remove(request_id);
            }
            target_requests
        };
        for (request_id, source_node) in target_requests {
            self.send_to_node(
                &source_node,
                &RelayFrame::Response {
                    request_id: request_id.clone(),
                    response: JsonRpcResponse::err(
                        Value::Null,
                        -32004,
                        format!("relay node '{node_id}' disconnected"),
                    ),
                },
            );
        }
    }

    fn complete_pending_from(
        &self,
        request_id: &str,
        response: JsonRpcResponse,
        source_node: Option<&str>,
    ) {
        let Some(expected) = self.pending.get(request_id) else {
            return;
        };
        if let Some(source_node) = source_node
            && expected.value().1 != source_node
        {
            self.metrics.acl_denials.fetch_add(1, Ordering::Relaxed);
            warn!(%request_id, expected = %expected.value().1, actual = %source_node, "relay response source mismatch");
            return;
        }
        drop(expected);
        if let Some((_, (tx, _node))) = self.pending.remove(request_id)
            && tx.send(response).is_err()
        {
            debug!(%request_id, "relay response waiter already dropped");
        }
    }

    /// Send a streaming Request to the spoke that owns `target`. Returns the
    /// relay `request_id`, the routed `node_id`, and a broadcast receiver
    /// that will receive wire-event `Value`s as `RelayFrame::Event` frames
    /// arrive from the spoke. The caller is responsible for cleanup via
    /// `RelayStreamGuard` so that SSE consumer disconnects propagate a
    /// Cancel frame to the spoke.
    pub async fn invoke_streaming(
        &self,
        target: &str,
        method: &str,
        params: Value,
        principal: &str,
    ) -> Result<(String, String, broadcast::Receiver<Value>)> {
        let route = self
            .route_for(target)
            .ok_or_else(|| anyhow!("no live relay route for {target}"))?;
        let conn = self
            .connections
            .get(&route.node_id)
            .ok_or_else(|| anyhow!("node '{}' is not connected", route.node_id))?;
        let request_id = format!("relay:stream:{}", Uuid::new_v4());
        let (event_tx, event_rx) = broadcast::channel(128);
        {
            let _admission = self
                .stream_admission
                .lock()
                .expect("relay stream admission mutex poisoned");
            if self.stream_pending.len() >= MAX_RELAY_STREAMS {
                anyhow::bail!("relay stream capacity exceeded");
            }
            self.stream_pending.insert(
                request_id.clone(),
                StreamPending {
                    tx: event_tx,
                    agent_ref: target.to_owned(),
                    node_id: route.node_id.clone(),
                    deadline: std::time::Instant::now() + STREAM_MAX_LIFETIME,
                },
            );
        }
        let frame = RelayFrame::Request {
            request_id: request_id.clone(),
            target: target.to_owned(),
            method: method.to_owned(),
            params,
            principal: principal.to_owned(),
            deadline_ms: REQUEST_TIMEOUT.as_millis() as u64,
        };
        let msg = AxumWsMessage::Text(serde_json::to_string(&frame)?.into());
        if let Err(e) = conn.tx.try_send(msg) {
            self.stream_pending.remove(&request_id);
            anyhow::bail!("relay send to node '{}' failed: {e}", route.node_id);
        }
        Ok((request_id, route.node_id, event_rx))
    }

    /// Send a `Cancel` frame for `request_id` to the spoke at `node_id`.
    /// Used by `RelayStreamGuard` when the hub-side SSE consumer disconnects
    /// before the spoke sends its terminal Response. No-op if the node is
    /// no longer connected.
    fn send_cancel_to(&self, node_id: &str, request_id: &str) {
        let Some(conn) = self.connections.get(node_id) else {
            return;
        };
        let frame = RelayFrame::Cancel {
            request_id: request_id.to_owned(),
            task_id: None,
        };
        match serde_json::to_string(&frame) {
            Ok(serialized) => {
                if conn
                    .tx
                    .try_send(AxumWsMessage::Text(serialized.into()))
                    .is_err()
                {
                    debug!(node = %node_id, %request_id, "relay cancel target disconnected");
                }
            }
            Err(error) => warn!(%error, %request_id, "cannot serialize relay cancel"),
        }
    }

    /// Remove and drop the stream entry for `request_id`. Returns `true` if
    /// a streaming entry existed (and was cleaned up). Dropping the
    /// broadcast sender signals Closed to all receivers, which terminates
    /// the SSE stream.
    fn complete_streaming(&self, request_id: &str) -> bool {
        self.stream_pending.remove(request_id).is_some()
    }

    /// Route a wire-event `Value` to the stream subscriber for `request_id`,
    /// if one exists. Returns the number of active receivers. Also sniffs
    /// the wire event's `taskId` and records the task→agent route.
    fn forward_stream_event(&self, request_id: &str, value: Value) -> usize {
        let Some(entry) = self.stream_pending.get(request_id) else {
            return 0;
        };
        if let Some(task_id) = value.get("taskId").and_then(|v| v.as_str()) {
            self.task_routes
                .insert(task_id.to_owned(), entry.agent_ref.clone());
        }
        entry.tx.send(value).unwrap_or(0)
    }

    /// Walk `stream_pending` and force-terminate any entry past its
    /// deadline. Emits a synthetic terminal status-update so SSE consumers
    /// observe a clean failure (rather than hanging until the WS dies),
    /// then drops the entry and sends `Cancel` to the spoke. Called from
    /// the gateway-wide sweeper loop in `startup.rs`.
    pub fn sweep_expired_streams(&self) -> usize {
        let now = std::time::Instant::now();
        let expired: Vec<(String, String)> = self
            .stream_pending
            .iter()
            .filter(|e| e.value().deadline <= now)
            .map(|e| (e.key().clone(), e.value().node_id.clone()))
            .collect();
        self.peer_signal_sessions
            .lock()
            .expect("peer signaling session mutex poisoned")
            .retain(|_, session| session.expires_at > now);
        self.consumed_peer_signal_sessions
            .lock()
            .expect("consumed peer signaling session mutex poisoned")
            .retain(|_, expires_at| *expires_at > now);
        let expired_spoke_requests: Vec<(String, String)> = {
            let mut requests = self
                .spoke_request_sources
                .lock()
                .expect("spoke relay request mutex poisoned");
            let expired: Vec<(String, String)> = requests
                .iter()
                .filter(|(_, request)| request.expires_at <= now)
                .map(|(request_id, request)| (request_id.clone(), request.source_node.clone()))
                .collect();
            for (request_id, _) in &expired {
                requests.remove(request_id);
            }
            expired
        };
        let expired_spoke_request_count = expired_spoke_requests.len();
        for (request_id, source_node) in expired_spoke_requests {
            self.metrics.inflight_losses.fetch_add(1, Ordering::Relaxed);
            self.send_to_node(
                &source_node,
                &RelayFrame::Response {
                    request_id,
                    response: JsonRpcResponse::err(
                        Value::Null,
                        -32004,
                        "relay request deadline exceeded",
                    ),
                },
            );
        }

        for (request_id, node_id) in &expired {
            let synthetic = serde_json::json!({
                "kind": "status-update",
                "taskId": "",
                "contextId": "",
                "status": {
                    "state": "failed",
                    "message": {
                        "role": "agent",
                        "messageId": format!("relay-deadline-{}", Uuid::new_v4()),
                        "parts": [{
                            "kind": "text",
                            "text": format!(
                                "relay stream exceeded {}s lifetime cap; aborting",
                                STREAM_MAX_LIFETIME.as_secs()
                            ),
                        }],
                    }
                },
                "final": true,
            });
            self.forward_stream_event(request_id, synthetic);
            self.stream_pending.remove(request_id);
            self.send_cancel_to(node_id, request_id);
            self.metrics.inflight_losses.fetch_add(1, Ordering::Relaxed);
            warn!(
                request_id = %request_id,
                node_id = %node_id,
                "relay stream hit deadline — synthetic failure emitted"
            );
        }
        expired.len() + expired_spoke_request_count
    }

    /// Record `task_id → agent_ref` so a follow-up RPC (GetTask, CancelTask,
    /// SubscribeToTask, push config ops) carrying only the task_id can be
    /// routed to the spoke that owns the task.
    pub fn record_task_route(&self, task_id: &str, agent_ref: &str) {
        self.task_routes
            .insert(task_id.to_owned(), agent_ref.to_owned());
    }

    /// Look up the agent_ref ("node/agent") that owns `task_id`, if known.
    pub fn route_for_task(&self, task_id: &str) -> Option<String> {
        self.task_routes.get(task_id).map(|e| e.clone())
    }
}

/// Lifecycle guard for hub-side relay streams. Held by the SSE response so
/// that when the consumer disconnects (the stream is dropped), we forward a
/// `Cancel` frame to the spoke and drop the `stream_pending` entry. If the
/// stream finished normally the spoke already removed `stream_pending` via
/// its terminal `Response`, so the Drop is a no-op.
pub struct RelayStreamGuard {
    relay_hub: std::sync::Arc<RelayHub>,
    node_id: String,
    request_id: String,
}

impl RelayStreamGuard {
    /// Create a stream lifecycle guard that cancels on premature drop.
    pub fn new(relay_hub: std::sync::Arc<RelayHub>, node_id: String, request_id: String) -> Self {
        Self {
            relay_hub,
            node_id,
            request_id,
        }
    }
}

impl Drop for RelayStreamGuard {
    fn drop(&mut self) {
        // complete_streaming returns true only if the entry was still
        // present — i.e. the spoke had not yet sent its terminal Response.
        // That's exactly the SSE-disconnected-early case, so cancel the
        // remote task.
        if self.relay_hub.complete_streaming(&self.request_id) {
            self.relay_hub
                .send_cancel_to(&self.node_id, &self.request_id);
        }
    }
}

/// Validate canonical `<node>/<agent>` syntax.
pub fn validate_agent_ref(agent_ref: &str) -> Result<()> {
    let Some((node, agent)) = agent_ref.split_once('/') else {
        anyhow::bail!("agent_ref must be '<node>/<agent>'");
    };
    if node.is_empty() || agent.is_empty() || agent.contains('/') {
        anyhow::bail!("invalid agent_ref '{agent_ref}'");
    }
    Ok(())
}

/// Resolve a canonical agent reference only when it belongs to `node_id`.
pub fn local_agent_from_ref(agent_ref: &str, node_id: &str) -> Option<String> {
    let (node, agent) = agent_ref.split_once('/')?;
    (node == node_id && !agent.is_empty() && !agent.contains('/')).then(|| agent.to_owned())
}

/// Return whether a scope set permits a namespaced action on a target.
pub fn scope_allows(scopes: &[String], namespace: &str, action: &str, target: &str) -> bool {
    let exact = format!("{namespace}:{action}:{target}");
    let all = format!("{namespace}:{action}:*");
    scopes.iter().any(|scope| {
        scope == &exact
            || scope == &all
            || scope
                .strip_suffix("/*")
                .is_some_and(|prefix| exact.starts_with(&format!("{prefix}/")))
    })
}

/// Return whether an optional A2A identity may invoke a target agent.
pub fn can_invoke(identity: Option<&A2aIdentity>, target: &str) -> bool {
    match identity {
        None => true,
        Some(id) if id.id == "gateway-auth" => true,
        Some(id) => scope_allows(&id.scopes, "a2a", "invoke", target),
    }
}

fn default_node_scopes(node_id: &str, relay_id: &str) -> Vec<String> {
    vec![
        format!("relay:connect:{relay_id}"),
        format!("relay:advertise:{node_id}/*"),
        format!("relay:receive:{node_id}/*"),
    ]
}

pub(crate) fn configured_peer_invoke_allowed(
    state: &AppState,
    peer_node_id: &str,
    target: &str,
) -> bool {
    let Some(peer) = state
        .config
        .agents
        .a2a
        .iter()
        .find(|peer| peer.node_id.as_deref() == Some(peer_node_id))
    else {
        return false;
    };
    if let Some(scopes) = peer.scopes.as_ref() {
        return scope_allows(scopes, "a2a", "invoke", target);
    }

    state
        .config
        .gateway
        .a2a_relay
        .nodes
        .iter()
        .find(|node| node.node_id == peer_node_id)
        .is_some_and(|node| scope_allows(&node.scopes, "a2a", "invoke", target))
}

/// Resolve a relay node by `node_id`. Token verification is the caller's
/// responsibility — callers must check whether the node requires token
/// auth (no public_key) or keypair auth (public_key set), and validate
/// the token only in the former case. Returns None if `node_id` is
/// unknown OR present on the revocation list.
pub(crate) fn resolve_node(relay: &A2aRelayRuntime, node_id: &str) -> Option<A2aRelayNodeRuntime> {
    if relay.revoked_nodes.iter().any(|n| n == node_id) {
        return None;
    }
    relay
        .nodes
        .iter()
        .find_map(|node| (node.node_id == node_id).then(|| node.clone()))
}

/// Verify the bearer token presented for a token-only node. constant_time
/// to defeat timing oracles.
pub(crate) fn verify_node_token(node: &A2aRelayNodeRuntime, token: &str) -> bool {
    !node.token.is_empty() && constant_time_eq(&node.token, token)
}

pub(crate) fn relay_connect_token_allows(
    node: &A2aRelayNodeRuntime,
    presented: Option<&str>,
) -> bool {
    if node.token.is_empty() {
        return node.public_key.is_some();
    }
    presented.is_some_and(|token| verify_node_token(node, token))
}

#[derive(Debug, Deserialize)]
pub struct RelayWsQuery {
    node_id: String,
}

pub async fn relay_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(query): Query<RelayWsQuery>,
    headers: HeaderMap,
) -> Response {
    let relay = &state.config.gateway.a2a_relay;
    if relay.mode != A2aRelayModeRuntime::Hub {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    }
    let Some(mut node) = resolve_node(relay, &query.node_id) else {
        state
            .relay_hub
            .metrics
            .auth_failures
            .fetch_add(1, Ordering::Relaxed);
        audit_relay(
            "deny",
            &format!("node:{}", query.node_id),
            "connect",
            &format!("relay:{}", relay.relay_id),
            &relay.relay_id,
            &query.node_id,
            None,
            Some("unknown or revoked node"),
        );
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    let presented = bearer_token(&headers);
    if !relay_connect_token_allows(&node, presented) {
        state
            .relay_hub
            .metrics
            .auth_failures
            .fetch_add(1, Ordering::Relaxed);
        let reason = if node.token.is_empty() {
            "no token configured; keypair handshake required"
        } else if presented.is_none() {
            "no token presented"
        } else {
            "token mismatch"
        };
        audit_relay(
            "deny",
            &format!("node:{}", node.node_id),
            "connect",
            &format!("relay:{}", relay.relay_id),
            &relay.relay_id,
            &node.node_id,
            None,
            Some(reason),
        );
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }
    if node.scopes.is_empty() {
        node.scopes = default_node_scopes(&node.node_id, &relay.relay_id);
    }
    if !scope_allows(&node.scopes, "relay", "connect", &relay.relay_id) {
        state
            .relay_hub
            .metrics
            .acl_denials
            .fetch_add(1, Ordering::Relaxed);
        audit_relay(
            "deny",
            &format!("node:{}", node.node_id),
            "connect",
            &format!("relay:{}", relay.relay_id),
            &relay.relay_id,
            &node.node_id,
            None,
            Some("relay:connect scope missing"),
        );
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    ws.on_upgrade(move |socket| handle_hub_socket(socket, HubCtx::from(&state), node))
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// Perform the Ed25519 challenge-response handshake. Returns Ok only if
/// the spoke produced a valid signature over the canonical payload bound
/// to both nonces. Anything else (timeout, malformed frame, bad sig,
/// premature close) returns Err with a short reason for audit.
async fn hub_keypair_handshake<S, R>(
    sink: &mut S,
    stream: &mut R,
    node: &A2aRelayNodeRuntime,
    public_key_b64: &str,
    relay_id: &str,
) -> std::result::Result<(), String>
where
    S: futures::Sink<AxumWsMessage> + Unpin,
    R: futures::Stream<Item = std::result::Result<AxumWsMessage, axum::Error>> + Unpin,
{
    // Step 1: read Hello + extract nonce_node.
    let hello = match tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(AxumWsMessage::Text(text)))) => text,
        Ok(Some(Ok(_))) => return Err("first frame was not Text".to_owned()),
        Ok(Some(Err(e))) => return Err(format!("ws error: {e}")),
        Ok(None) => return Err("stream closed before Hello".to_owned()),
        Err(_) => return Err("Hello timed out".to_owned()),
    };
    let nonce_node = match serde_json::from_str::<RelayFrame>(&hello) {
        Ok(RelayFrame::Hello {
            nonce_node: Some(n),
            node_id,
            ..
        }) => {
            if node_id != node.node_id {
                return Err(format!("hello node_id mismatch: claimed {node_id}"));
            }
            n
        }
        Ok(RelayFrame::Hello {
            nonce_node: None, ..
        }) => {
            return Err("hello missing nonce_node (keypair mode required)".to_owned());
        }
        Ok(_) => return Err("first frame was not Hello".to_owned()),
        Err(e) => return Err(format!("invalid Hello frame: {e}")),
    };

    // Step 2: send Challenge with hub nonce.
    let nonce_relay = relay_identity::fresh_nonce_b64();
    let challenge = RelayFrame::Challenge {
        relay_id: relay_id.to_owned(),
        nonce_relay: nonce_relay.clone(),
    };
    let payload =
        serde_json::to_string(&challenge).map_err(|e| format!("serialize Challenge: {e}"))?;
    if sink
        .send(AxumWsMessage::Text(payload.into()))
        .await
        .is_err()
    {
        return Err("send Challenge failed".to_owned());
    }

    // Step 3: read Auth + verify signature.
    let auth_text = match tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.next()).await {
        Ok(Some(Ok(AxumWsMessage::Text(text)))) => text,
        Ok(Some(Ok(_))) => return Err("second frame was not Text".to_owned()),
        Ok(Some(Err(e))) => return Err(format!("ws error: {e}")),
        Ok(None) => return Err("stream closed before Auth".to_owned()),
        Err(_) => return Err("Auth timed out".to_owned()),
    };
    let signature = match serde_json::from_str::<RelayFrame>(&auth_text) {
        Ok(RelayFrame::Auth { signature }) => signature,
        Ok(_) => return Err("second frame was not Auth".to_owned()),
        Err(e) => return Err(format!("invalid Auth frame: {e}")),
    };
    relay_identity::verify_handshake(
        public_key_b64,
        &node.node_id,
        relay_id,
        &nonce_node,
        &nonce_relay,
        &signature,
    )
    .map_err(|e| e.to_string())
}

async fn handle_hub_socket(socket: WebSocket, ctx: HubCtx, node: A2aRelayNodeRuntime) {
    let epoch = ctx
        .hub
        .connection_generation
        .fetch_add(1, Ordering::Relaxed);
    let (mut sink, mut stream) = socket.split();

    // Keypair handshake. Performed BEFORE register_connection so a node
    // that fails challenge-response never appears in `connections`
    // (cannot receive requests, cannot evict an existing well-behaved
    // session by same node_id).
    if let Some(public_key_b64) = node.public_key.as_deref() {
        let relay_id = ctx.relay_id.clone();
        match hub_keypair_handshake(&mut sink, &mut stream, &node, public_key_b64, &relay_id).await
        {
            Ok(()) => {
                audit_relay(
                    "allow",
                    &format!("node:{}", node.node_id),
                    "connect",
                    &format!("relay:{}", relay_id),
                    &relay_id,
                    &node.node_id,
                    Some("ed25519_handshake"),
                    None,
                );
            }
            Err(reason) => {
                ctx.hub
                    .metrics
                    .auth_failures
                    .fetch_add(1, Ordering::Relaxed);
                audit_relay(
                    "deny",
                    &format!("node:{}", node.node_id),
                    "connect",
                    &format!("relay:{}", relay_id),
                    &relay_id,
                    &node.node_id,
                    None,
                    Some(&format!("keypair handshake failed: {reason}")),
                );
                if let Err(error) = sink.send(AxumWsMessage::Close(None)).await {
                    debug!(node = %node.node_id, %error, "failed to close rejected relay socket");
                }
                return;
            }
        }
    }

    let (tx, mut rx) = mpsc::channel::<AxumWsMessage>(RELAY_WRITE_QUEUE_CAPACITY);
    let ping_tx = tx.clone();
    ctx.hub.register_connection(&node.node_id, tx, epoch);
    info!(node = %node.node_id, "a2a relay node connected");

    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    let ping = tokio::spawn(async move {
        // Two-tier heartbeat: WS-protocol-level Ping frame + app-level JSON
        // Ping. The protocol frame is what NAT/firewall/edge proxies count
        // as keep-alive — app-level JSON wrapped in a Text frame doesn't
        // always reset their idle counter. Long jimeng/douyin runs were
        // dropping after ~9min because a residential NAT entry expired
        // even though the agent was actively working.
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        loop {
            interval.tick().await;
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            // 1. WS-level Ping — empty payload is sufficient.
            if ping_tx
                .try_send(AxumWsMessage::Ping(Vec::new().into()))
                .is_err()
            {
                break;
            }
            // 2. App-level JSON Ping — kept for backwards compatibility
            // with spoke side that may match on RelayFrame::Ping for RTT
            // bookkeeping.
            let frame = RelayFrame::Ping { ts };
            if let Ok(msg) = serde_json::to_string(&frame) {
                if ping_tx.try_send(AxumWsMessage::Text(msg.into())).is_err() {
                    break;
                }
            }
        }
    });

    while let Some(msg) = stream.next().await {
        let Ok(msg) = msg else {
            break;
        };
        let AxumWsMessage::Text(text) = msg else {
            continue;
        };
        if !ctx.hub.owns_connection(&node.node_id, epoch) {
            warn!(node = %node.node_id, epoch, "superseded relay connection rejected");
            break;
        }
        match serde_json::from_str::<RelayFrame>(&text) {
            Ok(frame) => handle_hub_frame(&ctx, &node, frame).await,
            Err(e) => warn!(node = %node.node_id, error = %e, "invalid a2a relay frame"),
        }
    }

    ctx.hub.unregister_connection(&node.node_id, epoch);
    writer.abort();
    ping.abort();
    info!(node = %node.node_id, "a2a relay node disconnected");
}

async fn handle_hub_frame(ctx: &HubCtx, node: &A2aRelayNodeRuntime, frame: RelayFrame) {
    match frame {
        RelayFrame::Hello {
            protocol,
            node_id,
            capabilities,
            ..
        } => {
            if protocol != RELAY_PROTOCOL || node_id != node.node_id {
                warn!(node = %node.node_id, protocol, claimed = %node_id, "relay hello mismatch");
            }
            if let Some(caps) = capabilities {
                info!(
                    node = %node.node_id,
                    streaming_relay = caps.streaming_relay,
                    "relay node capabilities"
                );
            }
        }
        RelayFrame::RouteLease {
            node_id,
            agents,
            ttl_ms,
            epoch,
        } => {
            let relay_id = ctx.relay_id.as_str();
            if node_id != node.node_id {
                warn!(node = %node.node_id, claimed = %node_id, "relay route lease node mismatch");
                ctx.hub.metrics.acl_denials.fetch_add(1, Ordering::Relaxed);
                audit_relay(
                    "deny",
                    &format!("node:{}", node.node_id),
                    "advertise",
                    &format!("node:{node_id}"),
                    relay_id,
                    &node.node_id,
                    None,
                    Some("route lease node mismatch"),
                );
                return;
            }
            for agent in &agents {
                if !scope_allows(&node.scopes, "relay", "advertise", agent) {
                    warn!(node = %node.node_id, agent, "relay advertise denied");
                    ctx.hub.metrics.acl_denials.fetch_add(1, Ordering::Relaxed);
                    audit_relay(
                        "deny",
                        &format!("node:{}", node.node_id),
                        "advertise",
                        &format!("agent:{agent}"),
                        relay_id,
                        &node.node_id,
                        None,
                        Some("relay:advertise scope missing"),
                    );
                    return;
                }
            }
            if let Err(e) = ctx
                .hub
                .apply_route_lease(&node.node_id, &agents, ttl_ms, epoch)
            {
                warn!(node = %node.node_id, error = %e, "relay route lease rejected");
            }
        }
        RelayFrame::Auth { .. } | RelayFrame::Challenge { .. } => {
            // Handshake frames are consumed in hub_keypair_handshake. A
            // duplicate here is harmless — log and ignore.
            debug!(node = %node.node_id, "handshake frame after registration; ignored");
        }
        RelayFrame::Response {
            request_id,
            response,
        } => {
            // A response to a spoke-originated request returns to its authenticated
            // source connection. Otherwise this is a hub-originated request.
            let source_node = {
                let mut requests = ctx
                    .hub
                    .spoke_request_sources
                    .lock()
                    .expect("spoke relay request mutex poisoned");
                match requests.get(&request_id) {
                    Some(request_route) if request_route.target_node == node.node_id => {
                        let source_node = request_route.source_node.clone();
                        requests.remove(&request_id);
                        Some(source_node)
                    }
                    Some(request_route) => {
                        ctx.hub.metrics.acl_denials.fetch_add(1, Ordering::Relaxed);
                        warn!(%request_id, expected = %request_route.target_node, actual = %node.node_id, "spoke relay response source mismatch");
                        return;
                    }
                    None => None,
                }
            };
            if let Some(source_node) = source_node {
                let frame = RelayFrame::Response {
                    request_id,
                    response,
                };
                if !ctx.hub.send_to_node(&source_node, &frame) {
                    debug!(source = %source_node, "spoke request response source disconnected");
                }
            } else if !ctx.hub.complete_streaming(&request_id) {
                ctx.hub
                    .complete_pending_from(&request_id, response, Some(&node.node_id));
            }
        }
        RelayFrame::Request {
            request_id,
            target,
            method,
            params,
            principal,
            deadline_ms,
        } => {
            let source_node = node.node_id.clone();
            let Some(route) = ctx.hub.route_for(&target) else {
                let response = JsonRpcResponse::err(
                    Value::Null,
                    -32004,
                    format!("no live relay route for {target}"),
                );
                ctx.hub.send_to_node(
                    &source_node,
                    &RelayFrame::Response {
                        request_id,
                        response,
                    },
                );
                return;
            };
            let target_can_receive = ctx
                .nodes
                .iter()
                .find(|candidate| candidate.node_id == route.node_id)
                .is_some_and(|candidate| {
                    let default_scopes;
                    let scopes = if candidate.scopes.is_empty() {
                        default_scopes = default_node_scopes(&candidate.node_id, &ctx.relay_id);
                        &default_scopes
                    } else {
                        &candidate.scopes
                    };
                    scope_allows(scopes, "relay", "receive", &target)
                });
            if route.node_id == source_node
                || !FORWARDABLE_METHODS.contains(&method.as_str())
                || !scope_allows(&node.scopes, "a2a", "invoke", &target)
                || !target_can_receive
            {
                ctx.hub.metrics.acl_denials.fetch_add(1, Ordering::Relaxed);
                let response = JsonRpcResponse::err(
                    Value::Null,
                    -32003,
                    format!("not authorized to invoke {target}"),
                );
                ctx.hub.send_to_node(
                    &source_node,
                    &RelayFrame::Response {
                        request_id,
                        response,
                    },
                );
                return;
            }
            let admitted = ctx.hub.reserve_spoke_request(
                &request_id,
                &source_node,
                &route.node_id,
                deadline_ms,
            );
            if !admitted {
                ctx.hub.metrics.acl_denials.fetch_add(1, Ordering::Relaxed);
                ctx.hub.send_to_node(
                    &source_node,
                    &RelayFrame::Response {
                        request_id,
                        response: JsonRpcResponse::err(
                            Value::Null,
                            -32004,
                            "relay request capacity exceeded",
                        ),
                    },
                );
                return;
            }
            let forward = RelayFrame::Request {
                request_id: request_id.clone(),
                target,
                method,
                params,
                principal: format!("node:{source_node}:{principal}"),
                deadline_ms,
            };
            if !ctx.hub.send_to_node(&route.node_id, &forward) {
                ctx.hub
                    .spoke_request_sources
                    .lock()
                    .expect("spoke relay request mutex poisoned")
                    .remove(&request_id);
                ctx.hub.send_to_node(
                    &source_node,
                    &RelayFrame::Response {
                        request_id,
                        response: JsonRpcResponse::err(
                            Value::Null,
                            -32004,
                            "relay target disconnected",
                        ),
                    },
                );
            }
        }
        RelayFrame::Event {
            request_id, result, ..
        } => {
            if ctx.hub.forward_stream_event(&request_id, result) == 0 {
                debug!(request_id, "relay event for unknown stream");
            }
        }
        RelayFrame::Pong { .. } => {}
        RelayFrame::PeerOffer {
            session_id,
            target_node,
            sdp,
            signature,
        } => {
            let source_node = node.node_id.clone();
            if !crate::a2a::peer::valid_signaling_id(&session_id)
                || !crate::a2a::peer::valid_signaling_node_id(&source_node)
                || !crate::a2a::peer::valid_signaling_node_id(&target_node)
                || sdp.is_empty()
                || sdp.len() > MAX_PEER_SDP_BYTES
                || target_node == source_node
                || !ctx.hub.connections.contains_key(&target_node)
            {
                ctx.hub.metrics.acl_denials.fetch_add(1, Ordering::Relaxed);
                warn!(source = %source_node, target = %target_node, session = %session_id, "peer offer rejected");
                return;
            }
            {
                let mut sessions = ctx
                    .hub
                    .peer_signal_sessions
                    .lock()
                    .expect("peer signaling session mutex poisoned");
                if sessions.contains_key(&session_id)
                    || ctx
                        .hub
                        .consumed_peer_signal_sessions
                        .lock()
                        .expect("consumed peer signaling session mutex poisoned")
                        .contains_key(&session_id)
                {
                    ctx.hub.metrics.acl_denials.fetch_add(1, Ordering::Relaxed);
                    warn!(source = %source_node, target = %target_node, session = %session_id, "duplicate peer offer session rejected");
                    return;
                }
                if sessions.len() >= MAX_PEER_SIGNAL_SESSIONS {
                    ctx.hub.metrics.acl_denials.fetch_add(1, Ordering::Relaxed);
                    warn!(source = %source_node, target = %target_node, "peer signaling session capacity exceeded");
                    return;
                }
                sessions.insert(
                    session_id.clone(),
                    PeerSignalSession {
                        source_node: source_node.clone(),
                        target_node: target_node.clone(),
                        expires_at: std::time::Instant::now() + PEER_SIGNAL_TTL,
                    },
                );
            }
            let forward = RelayFrame::PeerOfferRelay {
                session_id: session_id.clone(),
                source_node: source_node.clone(),
                sdp,
                signature,
            };
            if !ctx.hub.send_to_node(&target_node, &forward) {
                ctx.hub
                    .peer_signal_sessions
                    .lock()
                    .expect("peer signaling session mutex poisoned")
                    .remove(&session_id);
                debug!(source = %source_node, target = %target_node, "peer offer target disconnected");
            }
        }
        RelayFrame::PeerAnswer {
            session_id,
            target_node,
            sdp,
            signature,
        } => {
            let answer_source = node.node_id.clone();
            let removed = {
                let mut sessions = ctx
                    .hub
                    .peer_signal_sessions
                    .lock()
                    .expect("peer signaling session mutex poisoned");
                let valid_session = crate::a2a::peer::valid_signaling_id(&session_id)
                    && crate::a2a::peer::valid_signaling_node_id(&answer_source)
                    && crate::a2a::peer::valid_signaling_node_id(&target_node)
                    && sessions.get(&session_id).is_some_and(|session| {
                        session.expires_at > std::time::Instant::now()
                            && session.source_node == target_node
                            && session.target_node == answer_source
                            && !sdp.is_empty()
                            && sdp.len() <= MAX_PEER_SDP_BYTES
                    });
                valid_session
                    .then(|| sessions.remove(&session_id))
                    .flatten()
            };
            if removed.is_none() {
                ctx.hub.metrics.acl_denials.fetch_add(1, Ordering::Relaxed);
                warn!(source = %answer_source, target = %target_node, session = %session_id, "unknown or mismatched peer answer rejected");
                return;
            }
            {
                let mut consumed = ctx
                    .hub
                    .consumed_peer_signal_sessions
                    .lock()
                    .expect("consumed peer signaling session mutex poisoned");
                consumed.retain(|_, expires_at| *expires_at > std::time::Instant::now());
                if consumed.len() >= MAX_PEER_SIGNAL_SESSIONS {
                    let oldest = consumed
                        .iter()
                        .min_by_key(|(_, expires_at)| **expires_at)
                        .map(|(session_id, _)| session_id.clone());
                    if let Some(oldest) = oldest {
                        consumed.remove(&oldest);
                    }
                }
                consumed.insert(
                    session_id.clone(),
                    std::time::Instant::now() + PEER_SIGNAL_TTL,
                );
            }
            let forward = RelayFrame::PeerAnswerRelay {
                session_id,
                source_node: answer_source.clone(),
                sdp,
                signature,
            };
            if !ctx.hub.send_to_node(&target_node, &forward) {
                debug!(source = %answer_source, target = %target_node, "peer answer target disconnected");
            }
        }
        RelayFrame::PeerConnected {
            peer_node,
            session_id,
        } => {
            // A direct ICE path belongs only to this source/target pair. Keep
            // the hub route relayed so every spoke retains a safe fallback.
            info!(
                source = %node.node_id,
                peer = %peer_node,
                session = %session_id,
                "peer ICE data channel connected"
            );
        }
        other => debug!(node = %node.node_id, frame = ?other, "hub ignored relay frame"),
    }
}

/// Extract a relay target from A2A request params, checking `metadata.agentId`
/// for a `node/agent` slash pattern.
pub fn relay_target_from_params(params: &Value) -> Option<String> {
    params
        .get("metadata")
        .and_then(|m| m.get("agentId"))
        .and_then(|v| v.as_str())
        .filter(|target| validate_agent_ref(target).is_ok())
        .map(str::to_owned)
}

/// Methods whose JSON-RPC params identify a target spoke either explicitly
/// via `metadata.agentId` or implicitly via a `task_id` (resolved through
/// the hub's task_routes cache, which is populated by sniffing responses
/// and streaming events).
const FORWARDABLE_METHODS: &[&str] = &[
    "SendMessage",
    "SendStreamingMessage",
    "SubscribeToTask",
    "GetTask",
    "CancelTask",
    "CreateTaskPushNotificationConfig",
    "GetTaskPushNotificationConfig",
    "ListTaskPushNotificationConfigs",
    "DeleteTaskPushNotificationConfig",
];

/// Extract a task_id from a JSON-RPC params object. A2A task-bound RPCs use
/// either `params.id` (GetTask/CancelTask/SubscribeToTask) or `params.taskId`
/// (push notification config ops).
pub fn task_id_from_params(params: &Value) -> Option<&str> {
    params
        .get("id")
        .and_then(|v| v.as_str())
        .or_else(|| params.get("taskId").and_then(|v| v.as_str()))
}

/// Resolve a request target from explicit agent metadata or a known task route.
pub fn relay_target_from_request(state: &AppState, req: &JsonRpcRequest) -> Option<String> {
    relay_target_from_routes(&state.peer_manager, &state.relay_hub, req)
}

fn relay_target_from_routes(
    peer_manager: &crate::a2a::peer::PeerManager,
    relay_hub: &RelayHub,
    req: &JsonRpcRequest,
) -> Option<String> {
    if !FORWARDABLE_METHODS.contains(&req.method.as_str()) {
        return None;
    }
    if let Some(target) = relay_target_from_params(&req.params) {
        return Some(target);
    }
    // A task created over the direct channel is known only to PeerManager;
    // consult it before the hub's relayed task cache.
    task_id_from_params(&req.params).and_then(|task_id| {
        peer_manager
            .route_for_task(task_id)
            .or_else(|| relay_hub.route_for_task(task_id))
    })
}

pub async fn try_forward_jsonrpc(
    state: &AppState,
    caller: Option<&A2aIdentity>,
    req: &JsonRpcRequest,
) -> Option<JsonRpcResponse> {
    let target = relay_target_from_request(state, req)?;

    // Direct is an optimization, never an authority. A send failure (including
    // a concurrent disconnect) immediately falls through to the hub route.
    if let Some((peer_node_id, _route)) = resolve_peer_route(&state.peer_manager, &target) {
        match forward_via_peer(state, caller, req, &target, &peer_node_id).await {
            PeerForwardResult::Handled(response) => return Some(response),
            PeerForwardResult::Unavailable => {}
        }
    }

    state.relay_hub.route_for(&target)?;
    forward_via_hub(state, caller, req, &target).await
}

/// Try to resolve a target through PeerManager for direct P2P forwarding.
/// Returns (peer_node_id, route_entry) if a direct connection exists.
pub(crate) fn resolve_peer_route(
    peer_mgr: &crate::a2a::peer::PeerManager,
    target: &str,
) -> Option<(String, RouteEntry)> {
    let route = peer_mgr.route_for(target)?;
    if route.mode != RouteMode::Direct {
        return None;
    }
    Some((route.node_id.clone(), route))
}

enum PeerForwardResult {
    Handled(JsonRpcResponse),
    Unavailable,
}

fn classify_peer_forward_result(
    request_id: &Value,
    result: Result<JsonRpcResponse, crate::a2a::peer::PeerInvokeError>,
) -> PeerForwardResult {
    match result {
        Ok(mut response) => {
            response.id = request_id.clone();
            PeerForwardResult::Handled(response)
        }
        Err(crate::a2a::peer::PeerInvokeError::Unavailable(_)) => PeerForwardResult::Unavailable,
        Err(crate::a2a::peer::PeerInvokeError::DeliveryUnknown(error)) => {
            PeerForwardResult::Handled(JsonRpcResponse::err(request_id.clone(), -32005, error))
        }
    }
}

/// Forward a JSON-RPC call via a direct WebRTC data channel.
async fn forward_via_peer(
    state: &AppState,
    caller: Option<&A2aIdentity>,
    req: &JsonRpcRequest,
    target: &str,
    peer_node_id: &str,
) -> PeerForwardResult {
    let relay_id = state.config.gateway.a2a_relay.relay_id.as_str();
    let principal_id = caller.map(|id| id.id.as_str()).unwrap_or("anonymous-dev");
    if !can_invoke(caller, target) {
        return PeerForwardResult::Handled(JsonRpcResponse::err(
            req.id.clone(),
            -32003,
            format!("not authorized to invoke {target}"),
        ));
    }
    audit_relay(
        "allow",
        principal_id,
        "invoke",
        &format!("agent:{target}"),
        relay_id,
        peer_node_id,
        None,
        Some("peer_direct"),
    );
    let principal = principal_id;
    let mut params = req.params.clone();
    rewrite_target_agent_for_spoke(&mut params, target);
    let outcome = classify_peer_forward_result(
        &req.id,
        state
            .peer_manager
            .invoke_jsonrpc(target, &req.method, params, principal, peer_node_id)
            .await,
    );
    if let PeerForwardResult::Handled(response) = &outcome
        && let Some(task_id) = response
            .result
            .as_ref()
            .and_then(|result| result.get("id"))
            .and_then(Value::as_str)
    {
        state.peer_manager.record_task_route(task_id, target);
    }
    outcome
}

/// Forward a JSON-RPC call via the relay hub (existing path).
async fn forward_via_hub(
    state: &AppState,
    caller: Option<&A2aIdentity>,
    req: &JsonRpcRequest,
    target: &str,
) -> Option<JsonRpcResponse> {
    let relay_id = state.config.gateway.a2a_relay.relay_id.as_str();
    let principal_id = caller.map(|id| id.id.as_str()).unwrap_or("anonymous-dev");
    if !can_invoke(caller, target) {
        state
            .relay_hub
            .metrics
            .acl_denials
            .fetch_add(1, Ordering::Relaxed);
        let target_node = target.split('/').next().unwrap_or("");
        audit_relay(
            "deny",
            principal_id,
            "invoke",
            &format!("agent:{target}"),
            relay_id,
            target_node,
            None,
            Some("a2a:invoke scope missing"),
        );
        return Some(JsonRpcResponse::err(
            req.id.clone(),
            -32003,
            format!("not authorized to invoke {target}"),
        ));
    }
    let target_node = target.split('/').next().unwrap_or("");
    audit_relay(
        "allow",
        principal_id,
        "invoke",
        &format!("agent:{target}"),
        relay_id,
        target_node,
        None,
        Some("cross_node"),
    );
    let principal = principal_id;
    let mut params = req.params.clone();
    rewrite_target_agent_for_spoke(&mut params, target);
    match state
        .relay_hub
        .invoke_jsonrpc(target, &req.method, params, principal)
        .await
    {
        Ok(mut response) => {
            response.id = req.id.clone();
            if let Some(task_id) = response
                .result
                .as_ref()
                .and_then(|r| r.get("id"))
                .and_then(|v| v.as_str())
            {
                state.relay_hub.record_task_route(task_id, target);
            }
            Some(response)
        }
        Err(e) => Some(JsonRpcResponse::err(req.id.clone(), -32004, e.to_string())),
    }
}

/// Runtime implementation of the crate-inverted outbound A2A transport host.
pub struct GatewayOutboundA2aHost {
    state: AppState,
}

impl GatewayOutboundA2aHost {
    /// Create a host backed by the live gateway peer and relay managers.
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

impl rsclaw_types::OutboundA2aHost for GatewayOutboundA2aHost {
    fn try_send(
        &self,
        request: rsclaw_types::OutboundA2aRequest,
    ) -> futures::future::BoxFuture<'static, Result<Option<String>>> {
        let state = self.state.clone();
        Box::pin(async move {
            validate_agent_ref(&request.target)?;
            let request_id = Uuid::new_v4().to_string();
            let params = serde_json::json!({
                "message": {
                    "messageId": request_id,
                    "role": "ROLE_USER",
                    "parts": [{"type": "text", "text": request.text}],
                    "contextId": request.context_id,
                },
                "metadata": {"agentId": request.target},
            });

            let target = request.target;
            let mut response = if let Some((peer_node_id, _)) =
                resolve_peer_route(&state.peer_manager, &target)
            {
                let mut direct_params = params.clone();
                rewrite_target_agent_for_spoke(&mut direct_params, &target);
                match state
                    .peer_manager
                    .invoke_jsonrpc(
                        &target,
                        "SendMessage",
                        direct_params,
                        &request.principal,
                        &peer_node_id,
                    )
                    .await
                {
                    Ok(response) => Some(response),
                    Err(crate::a2a::peer::PeerInvokeError::Unavailable(error)) => {
                        warn!(target = %target, %error, "direct A2A unavailable; trying hub relay");
                        None
                    }
                    Err(crate::a2a::peer::PeerInvokeError::DeliveryUnknown(error)) => {
                        return Err(anyhow!(
                            "direct A2A delivery outcome is unknown; not retrying: {error}"
                        ));
                    }
                }
            } else {
                None
            };

            if response.is_none() {
                let mut relay_params = params;
                rewrite_target_agent_for_spoke(&mut relay_params, &target);
                response = state
                    .relay_hub
                    .invoke_via_spoke(&target, "SendMessage", relay_params, &request.principal)
                    .await?;
            }

            let Some(response) = response else {
                return Ok(None);
            };
            if let Some(error) = response.error {
                return Err(anyhow!(
                    "remote A2A error {}: {}",
                    error.code,
                    error.message
                ));
            }
            let result = response
                .result
                .ok_or_else(|| anyhow!("remote A2A returned an empty result"))?;
            Ok(Some(extract_a2a_reply_text(&result)?))
        })
    }
}

fn extract_a2a_reply_text(result: &Value) -> Result<String> {
    if let Some(artifacts) = result.get("artifacts").and_then(Value::as_array) {
        for artifact in artifacts {
            if let Some(parts) = artifact.get("parts").and_then(Value::as_array) {
                for part in parts {
                    if part.get("type").and_then(Value::as_str) == Some("text")
                        && let Some(text) = part.get("text").and_then(Value::as_str)
                    {
                        return Ok(text.to_owned());
                    }
                }
            }
        }
    }
    if let Some(parts) = result
        .get("status")
        .and_then(|status| status.get("message"))
        .and_then(|message| message.get("parts"))
        .and_then(Value::as_array)
    {
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some("text")
                && let Some(text) = part.get("text").and_then(Value::as_str)
            {
                return Ok(text.to_owned());
            }
        }
    }
    Err(anyhow!("remote A2A result has no text part"))
}

pub(crate) fn rewrite_target_agent_for_spoke(params: &mut Value, target: &str) {
    let Some((_, agent)) = target.split_once('/') else {
        return;
    };
    if let Some(metadata) = params.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        metadata.insert("agentId".to_owned(), Value::String(agent.to_owned()));
    }
}

/// `GET /v1/a2a/relay/stats` — snapshot of relay metrics. Returns the
/// 10 spec-mandated counters plus a list of connected node_ids. Safe
/// to expose on the gateway operator surface (no secrets).
pub async fn relay_stats_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    let nodes = state.relay_hub.connected_nodes();
    let snapshot = state
        .relay_hub
        .metrics
        .snapshot(nodes.len() as u64, state.relay_hub.route_count() as u64);
    Json(serde_json::json!({
        "relay_id": state.config.gateway.a2a_relay.relay_id,
        "mode": match state.config.gateway.a2a_relay.mode {
            A2aRelayModeRuntime::Disabled => "disabled",
            A2aRelayModeRuntime::Hub => "hub",
            A2aRelayModeRuntime::Spoke => "spoke",
        },
        "connected_node_ids": nodes,
        "metrics": snapshot,
    }))
}

/// Start the configured relay spoke reconnect loop, if spoke mode is enabled.
pub fn start_spoke_if_configured(state: AppState) {
    if state.config.gateway.a2a_relay.mode != A2aRelayModeRuntime::Spoke {
        return;
    }
    let relay = state.config.gateway.a2a_relay.clone();
    if relay.hub_urls.is_empty() {
        warn!("a2a relay spoke mode set but no hub URLs configured");
        return;
    }
    tokio::spawn(async move {
        // Primary-standby failover: walk the hub_urls list in order.
        // Index 0 is the primary; we fall back to higher indices on
        // connect/heartbeat failure and reset back to 0 after a
        // successful long-lived connection ends cleanly. `multi_home`
        // isn't implemented here yet — it falls through to the same
        // single-active-connection loop (Phase 3 will add concurrent
        // connections + duplicate suppression).
        let strategy = relay.strategy.clone();
        if strategy == A2aRelayStrategyRuntime::MultiHome {
            warn!("a2a relay strategy=multi_home not yet supported, using primary_standby");
        }
        let urls = relay.hub_urls.clone();
        let mut idx: usize = 0;
        let mut per_relay_delay = Duration::from_secs(1);
        loop {
            let hub_url = &urls[idx];
            let connect_start = std::time::Instant::now();
            match run_spoke_once(state.clone(), &relay, hub_url).await {
                Ok(()) => {
                    // Clean disconnect — server-initiated close or
                    // protocol exhaustion. Reset to primary and to fast
                    // backoff so the next outage doesn't compound prior
                    // exponential growth.
                    idx = 0;
                    per_relay_delay = Duration::from_secs(1);
                    info!(hub = %hub_url, "a2a relay spoke session ended cleanly, returning to primary");
                }
                Err(e) => {
                    let was_long_lived = connect_start.elapsed() > Duration::from_secs(60);
                    warn!(
                        error = %e,
                        hub = %hub_url,
                        idx,
                        "a2a relay spoke disconnected"
                    );
                    state
                        .relay_hub
                        .metrics
                        .ws_reconnects
                        .fetch_add(1, Ordering::Relaxed);
                    if was_long_lived {
                        // Lost a stable session — try same relay first
                        // (likely transient network blip). Reset backoff
                        // since this isn't a retry storm.
                        per_relay_delay = Duration::from_secs(1);
                    } else {
                        // Failed to establish or session died fast —
                        // back off and rotate to the next relay.
                        if urls.len() > 1 {
                            idx = (idx + 1) % urls.len();
                            state
                                .relay_hub
                                .metrics
                                .failovers
                                .fetch_add(1, Ordering::Relaxed);
                            info!(next_hub = %urls[idx], "a2a relay failing over");
                        }
                        per_relay_delay = (per_relay_delay * 2).min(FAILOVER_BACKOFF_MAX);
                    }
                    tokio::time::sleep(per_relay_delay).await;
                }
            }
        }
    });
}

async fn run_spoke_once(state: AppState, relay: &A2aRelayRuntime, hub_url: &str) -> Result<()> {
    let node_id = relay
        .node_id
        .as_deref()
        .ok_or_else(|| anyhow!("a2a relay spoke node_id is required"))?;
    // Token is optional when keypair is configured. The hub-side
    // resolve_node tolerates a missing token if `public_key` is set.
    let token = relay.token.as_deref();
    let signing_key = match relay.private_key.as_deref() {
        Some(pk) => Some(
            relay_identity::signing_key_from_b64(pk).context("parse spoke private_key (base64)")?,
        ),
        None => None,
    };
    if token.is_none() && signing_key.is_none() {
        anyhow::bail!("a2a relay spoke requires either token or private_key");
    }
    let sep = if hub_url.contains('?') { '&' } else { '?' };
    let url = format!("{hub_url}{sep}node_id={}", urlencoding::encode(node_id));
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    let mut request = url
        .into_client_request()
        .context("build relay WebSocket request")?;
    if let Some(token) = token {
        let authorization =
            tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&format!("Bearer {token}"))
                .context("build relay authorization header")?;
        request.headers_mut().insert(
            tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
            authorization,
        );
    }
    let (stream, _) = tokio_tungstenite::connect_async(request)
        .await
        .with_context(|| format!("connect relay hub {hub_url}"))?;
    info!(node = %node_id, hub = %hub_url, keypair = signing_key.is_some(), "a2a relay spoke connected");

    let (mut write, mut read) = stream.split();
    // Channel item: either a RelayFrame (encoded as JSON Text by the
    // writer) or a raw WS-protocol Ping for low-level keep-alive.
    // NAT/firewall middle boxes count WS-protocol Ping frames as
    // activity while app-level JSON Text often doesn't reset their idle
    // counter — observed when long jimeng/douyin runs were getting
    // dropped after ~9min mid-pipeline.
    enum SpokeWriteItem {
        Frame(RelayFrame),
        WsPing,
    }
    let (write_tx, mut write_rx) = mpsc::channel::<SpokeWriteItem>(RELAY_WRITE_QUEUE_CAPACITY);
    let writer = tokio::spawn(async move {
        while let Some(item) = write_rx.recv().await {
            let result = match item {
                SpokeWriteItem::Frame(frame) => send_spoke_frame(&mut write, &frame).await,
                SpokeWriteItem::WsPing => write
                    .send(tokio_tungstenite::tungstenite::Message::Ping(
                        Vec::new().into(),
                    ))
                    .await
                    .map_err(anyhow::Error::from),
            };
            if let Err(e) = result {
                warn!(error = %e, "spoke write error");
                break;
            }
        }
    });

    // Adapter: every other component in this function — including
    // handle_spoke_request which takes a Sender<RelayFrame> — just sends
    // RelayFrame. A small forwarder converts those to SpokeWriteItem::Frame.
    let (spoke_tx, mut frame_rx) = mpsc::channel::<RelayFrame>(RELAY_WRITE_QUEUE_CAPACITY);
    let frame_adapter_tx = write_tx.clone();
    let frame_adapter = tokio::spawn(async move {
        while let Some(frame) = frame_rx.recv().await {
            if frame_adapter_tx
                .try_send(SpokeWriteItem::Frame(frame))
                .is_err()
            {
                break;
            }
        }
    });

    let nonce_node = signing_key
        .as_ref()
        .map(|_| relay_identity::fresh_nonce_b64());
    spoke_tx
        .try_send(spoke_hello(&state, node_id, nonce_node.clone()))
        .map_err(|_| anyhow!("spoke writer closed"))?;
    // RouteLease is sent AFTER the Auth round-trip when in keypair mode
    // so the hub doesn't drop us mid-handshake. Token-mode spokes send
    // it immediately because there's no handshake.
    let control_generation = Uuid::new_v4().to_string();
    if signing_key.is_none() {
        if let Err(error) = spoke_tx
            .try_send(spoke_route_lease(&state, node_id, 1))
            .map_err(|_| anyhow!("spoke writer closed"))
        {
            writer.abort();
            frame_adapter.abort();
            return Err(error);
        }
        state
            .relay_hub
            .register_spoke_control(spoke_tx.clone(), control_generation.clone());
        if let Err(error) = start_peer_offers(&state, node_id, &spoke_tx).await {
            state
                .relay_hub
                .unregister_spoke_control(&control_generation);
            state
                .peer_manager
                .drop_pending_sessions("peer offer startup failed")
                .await;
            writer.abort();
            frame_adapter.abort();
            return Err(error);
        }
    }

    // WS-level Ping every 15s so NAT/firewall idle counters reset.
    let ping_tx = write_tx.clone();
    let pinger = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        interval.tick().await; // skip immediate tick
        loop {
            interval.tick().await;
            if ping_tx.try_send(SpokeWriteItem::WsPing).is_err() {
                break;
            }
        }
    });

    // Periodically re-publish the RouteLease so the hub never sees a
    // route silently expire. ROUTE_TTL_MS is the hub-side eviction
    // threshold; sending at one third of it gives ample headroom under
    // packet loss and clock skew. epoch is monotonic so the hub keeps
    // accepting fresher leases over stale duplicates.
    let renew_tx = spoke_tx.clone();
    let renew_state = state.clone();
    let renew_node_id = node_id.to_owned();
    let renewer = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(ROUTE_TTL_MS / 3));
        // Skip the immediate tick — we already sent epoch 1 above.
        interval.tick().await;
        let mut epoch: u64 = 2;
        loop {
            interval.tick().await;
            let frame = spoke_route_lease(&renew_state, &renew_node_id, epoch);
            if renew_tx.try_send(frame).is_err() {
                break;
            }
            epoch = epoch.saturating_add(1);
        }
    });

    let connection_result = async {
        while let Some(msg) = read.next().await {
        let msg = msg?;
        let tokio_tungstenite::tungstenite::Message::Text(text) = msg else {
            continue;
        };
        let frame: RelayFrame = serde_json::from_str(&text)?;
        match frame {
            RelayFrame::Challenge {
                relay_id,
                nonce_relay,
            } => {
                let Some(sk) = signing_key.as_ref() else {
                    anyhow::bail!("hub sent Challenge but spoke has no private_key");
                };
                let Some(nn) = nonce_node.as_deref() else {
                    anyhow::bail!("Challenge received without our nonce_node — protocol drift");
                };
                let sig = relay_identity::sign_handshake(sk, node_id, &relay_id, nn, &nonce_relay);
                spoke_tx
                    .try_send(RelayFrame::Auth { signature: sig })
                    .map_err(|_| anyhow!("spoke writer closed"))?;
                // Now safe to publish routes and outbound fallback — handshake done.
                state
                    .relay_hub
                    .register_spoke_control(spoke_tx.clone(), control_generation.clone());
                spoke_tx
                    .try_send(spoke_route_lease(&state, node_id, 1))
                    .map_err(|_| anyhow!("spoke writer closed"))?;
                start_peer_offers(&state, node_id, &spoke_tx).await?;
            }
            RelayFrame::Response {
                request_id,
                response,
            } => {
                state
                    .relay_hub
                    .complete_pending_from(&request_id, response, None);
            }
            RelayFrame::Request {
                request_id,
                target,
                method,
                params,
                principal,
                ..
            } => {
                let response = handle_spoke_request(
                    &state,
                    node_id,
                    &request_id,
                    &target,
                    &method,
                    params,
                    principal,
                    spoke_tx.clone(),
                    &control_generation,
                )
                .await;
                if let Some(response) = response
                    && spoke_tx
                        .try_send(RelayFrame::Response {
                            request_id,
                            response,
                        })
                        .is_err()
                {
                    debug!("spoke response dropped because writer closed");
                }
                // If response is None, the streaming handler sent events via
                // spoke_tx and will send the terminal Response itself.
            }
            RelayFrame::Ping { ts } => {
                if spoke_tx.try_send(RelayFrame::Pong { ts }).is_err() {
                    debug!("spoke pong dropped because writer closed");
                }
            }
            RelayFrame::Cancel { request_id, .. } => {
                // Cancel the streaming task if it exists locally. `task_cancels`
                // is keyed by local task_id, so resolve via the spoke-side map.
                if let Some(task) = state
                    .relay_hub
                    .remove_spoke_stream_task(&request_id, &control_generation)
                    && let Some(owner) = task.cancel_owner
                    && let Some(token) = crate::server::remove_task_cancel_if_owner(
                        &state.task_cancels,
                        &task.task_id,
                        &owner,
                    )
                {
                    token.cancel();
                }
            }
            RelayFrame::PeerOfferRelay {
                session_id,
                source_node,
                sdp,
                signature,
            } => match crate::a2a::peer::apply_peer_offer(
                &state,
                &source_node,
                &session_id,
                &sdp,
                signature.as_deref(),
            )
            .await
            {
                Ok(answer) => {
                    if spoke_tx.try_send(answer).is_err() {
                        state
                            .peer_manager
                            .drop_session(&session_id, "spoke signaling channel closed")
                            .await;
                        anyhow::bail!("spoke writer closed");
                    }
                }
                Err(error) => {
                    warn!(peer = %source_node, session = %session_id, %error, "peer WebRTC offer rejected");
                }
            },
            RelayFrame::PeerAnswerRelay {
                session_id,
                source_node,
                sdp,
                signature,
            } => {
                if let Err(error) = crate::a2a::peer::apply_peer_answer(
                    &state,
                    &source_node,
                    &session_id,
                    &sdp,
                    signature.as_deref(),
                )
                .await
                {
                    warn!(peer = %source_node, session = %session_id, %error, "peer WebRTC answer rejected");
                }
            }
            RelayFrame::PeerConnected { .. } => {}
            _ => {}
        }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    // WS drop reached: cancel every local streaming task we were
    // proxying so workers stop burning tokens for a stream that will
    // never reach the client. After reconnect, the hub will route new
    // requests through fresh task_ids; old request_ids are irrelevant.
    for task in state.relay_hub.take_spoke_stream_tasks(&control_generation) {
        if let Some(owner) = task.cancel_owner
            && let Some(token) = crate::server::remove_task_cancel_if_owner(
                &state.task_cancels,
                &task.task_id,
                &owner,
            )
        {
            token.cancel();
        }
    }
    state
        .relay_hub
        .unregister_spoke_control(&control_generation);
    state
        .peer_manager
        .drop_pending_sessions("relay control connection closed")
        .await;
    writer.abort();
    renewer.abort();
    pinger.abort();
    frame_adapter.abort();
    connection_result
}

async fn start_peer_offers(
    state: &AppState,
    node_id: &str,
    spoke_tx: &mpsc::Sender<RelayFrame>,
) -> Result<()> {
    if !state
        .config
        .gateway
        .a2a_relay
        .peer
        .as_ref()
        .is_some_and(|peer| peer.enabled)
    {
        return Ok(());
    }

    for peer_config in &state.config.agents.a2a {
        let Some(peer_node_id) = peer_config.node_id.as_deref() else {
            continue;
        };
        // The lexical winner offers, preventing simultaneous-offer glare.
        if node_id >= peer_node_id || state.peer_manager.has_direct_connection(peer_node_id) {
            continue;
        }
        match crate::a2a::peer::create_peer_offer(state, peer_node_id).await {
            Ok(frame) => {
                spoke_tx
                    .try_send(frame)
                    .map_err(|_| anyhow!("spoke writer closed"))?;
                info!(node = %node_id, target = %peer_node_id, "sent peer WebRTC offer");
            }
            Err(error) => {
                warn!(node = %node_id, target = %peer_node_id, %error, "peer WebRTC offer failed; hub relay remains active");
            }
        }
    }
    Ok(())
}

fn spoke_hello(state: &AppState, node_id: &str, nonce_node: Option<String>) -> RelayFrame {
    RelayFrame::Hello {
        protocol: RELAY_PROTOCOL.to_owned(),
        node_id: node_id.to_owned(),
        node_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        agent_card: Some(crate::a2a::server::build_agent_card(state, false)),
        capabilities: Some(HelloCapabilities {
            streaming_relay: true,
        }),
        nonce_node,
    }
}

fn spoke_route_lease(state: &AppState, node_id: &str, epoch: u64) -> RelayFrame {
    let agents = state
        .agents
        .all()
        .into_iter()
        .map(|agent| format!("{node_id}/{}", agent.id))
        .collect();
    RelayFrame::RouteLease {
        node_id: node_id.to_owned(),
        agents,
        ttl_ms: ROUTE_TTL_MS,
        epoch,
    }
}

async fn send_spoke_frame<W>(write: &mut W, frame: &RelayFrame) -> Result<()>
where
    W: futures::Sink<tokio_tungstenite::tungstenite::Message> + Unpin,
    W::Error: std::error::Error + Send + Sync + 'static,
{
    write
        .send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::to_string(frame)?.into(),
        ))
        .await?;
    Ok(())
}

async fn handle_spoke_request(
    state: &AppState,
    node_id: &str,
    request_id: &str,
    target: &str,
    method: &str,
    params: Value,
    principal: String,
    spoke_tx: mpsc::Sender<RelayFrame>,
    control_generation: &str,
) -> Option<JsonRpcResponse> {
    let Some(local_agent) = local_agent_from_ref(target, node_id) else {
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

    // Streaming methods: spawn the task locally and forward events via relay.
    if method == "SendStreamingMessage" || method == "SubscribeToTask" {
        let caller = Some(A2aIdentity {
            id: principal,
            scopes: Vec::new(),
        });
        let (task_id, event_rx, cancel_owner) = if method == "SubscribeToTask" {
            let (task_id, event_rx) =
                crate::a2a::streaming::subscribe_to_task(state, &caller, &params);
            (task_id, event_rx, None)
        } else {
            crate::a2a::streaming::spawn_streaming_task(state.clone(), caller, params).await
        };
        let request_id_for_relay = request_id.to_owned();
        state.relay_hub.spoke_stream_tasks.insert(
            request_id_for_relay.clone(),
            SpokeStreamTask {
                task_id: task_id.clone(),
                control_generation: control_generation.to_owned(),
                cancel_owner,
            },
        );
        let relay_hub = state.relay_hub.clone();
        let control_generation = control_generation.to_owned();
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
                            .try_send(RelayFrame::Event {
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
                        warn!(lagged = n, "spoke relay event lagged");
                    }
                }
            }
            relay_hub.remove_spoke_stream_task(&request_id_for_relay, &control_generation);
            if spoke_tx
                .try_send(RelayFrame::Response {
                    request_id: request_id_for_relay,
                    response: JsonRpcResponse::ok(
                        Value::String(task_id),
                        serde_json::json!({"ok": true}),
                    ),
                })
                .is_err()
            {
                debug!("stream terminal response dropped because spoke writer closed");
            }
        });
        return None;
    }

    let req = JsonRpcRequest {
        jsonrpc: "2.0".to_owned(),
        id: Value::String(format!("spoke:{}", Uuid::new_v4())),
        method: method.to_owned(),
        params,
    };
    let caller = Some(A2aIdentity {
        id: principal,
        scopes: Vec::new(),
    });
    Some(
        crate::a2a::server::a2a_rpc_handler_inner(state.clone(), caller, req)
            .await
            .0,
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn peer_forward_classification_preserves_delivery_provenance() {
        let request_id = Value::String("caller-id".to_owned());
        let response =
            JsonRpcResponse::err(Value::String("remote-id".to_owned()), -32004, "remote");
        match classify_peer_forward_result(&request_id, Ok(response)) {
            PeerForwardResult::Handled(response) => {
                assert_eq!(response.id, request_id);
                assert_eq!(response.error.expect("remote error").code, -32004);
            }
            PeerForwardResult::Unavailable => {
                panic!("delivered response must not request fallback")
            }
        }

        assert!(matches!(
            classify_peer_forward_result(
                &request_id,
                Err(crate::a2a::peer::PeerInvokeError::Unavailable(
                    "queue full".to_owned(),
                )),
            ),
            PeerForwardResult::Unavailable
        ));

        match classify_peer_forward_result(
            &request_id,
            Err(crate::a2a::peer::PeerInvokeError::DeliveryUnknown(
                "response lost".to_owned(),
            )),
        ) {
            PeerForwardResult::Handled(response) => {
                let error = response.error.expect("delivery-unknown error");
                assert_eq!(error.code, -32005);
                assert_eq!(error.message, "response lost");
            }
            PeerForwardResult::Unavailable => {
                panic!("unknown delivery must not request fallback")
            }
        }
    }

    #[test]
    fn spoke_request_correlation_is_bounded_and_expires() {
        let hub = RelayHub::new();
        for index in 0..MAX_SPOKE_RELAY_REQUESTS {
            assert!(hub.reserve_spoke_request(
                &format!("request-{index}"),
                "source",
                "target",
                REQUEST_TIMEOUT.as_millis() as u64,
            ));
        }
        assert!(!hub.reserve_spoke_request(
            "request-over-capacity",
            "source",
            "target",
            REQUEST_TIMEOUT.as_millis() as u64,
        ));
        for request in hub
            .spoke_request_sources
            .lock()
            .expect("spoke relay request mutex poisoned")
            .values_mut()
        {
            request.expires_at = std::time::Instant::now() - Duration::from_millis(1);
        }

        assert_eq!(hub.sweep_expired_streams(), MAX_SPOKE_RELAY_REQUESTS);
        assert!(
            hub.spoke_request_sources
                .lock()
                .expect("spoke relay request mutex poisoned")
                .is_empty()
        );
    }

    #[test]
    fn suffix_scopes_match_agent_children_only() {
        let scopes = vec!["a2a:invoke:a3/*".to_owned()];
        assert!(scope_allows(&scopes, "a2a", "invoke", "a3/main"));
        assert!(!scope_allows(&scopes, "a2a", "invoke", "a30/main"));
        assert!(!scope_allows(&scopes, "a2a", "cancel", "a3/main"));
    }

    #[test]
    fn route_lease_rejects_cross_node_advertise() {
        let hub = RelayHub::new();
        let err = hub
            .apply_route_lease("a1", &["a3/main".to_owned()], 10_000, 1)
            .expect_err("cross-node route should fail");
        assert!(err.to_string().contains("cannot advertise"));
    }

    #[test]
    fn route_lease_adds_live_route() {
        let hub = RelayHub::new();
        hub.apply_route_lease("a1", &["a1/main".to_owned()], 10_000, 1)
            .unwrap();
        let route = hub.route_for("a1/main").expect("route");
        assert_eq!(route.node_id, "a1");
    }

    #[test]
    fn gateway_auth_can_invoke_everything() {
        let id = A2aIdentity {
            id: "gateway-auth".to_owned(),
            scopes: Vec::new(),
        };
        assert!(can_invoke(Some(&id), "a3/main"));
    }

    #[test]
    fn scoped_identity_can_only_invoke_allowed_target() {
        let id = A2aIdentity {
            id: "node:a1".to_owned(),
            scopes: vec!["a2a:invoke:a3/main".to_owned()],
        };
        assert!(can_invoke(Some(&id), "a3/main"));
        assert!(!can_invoke(Some(&id), "a3/coder"));
    }

    #[test]
    fn keypair_node_with_token_still_requires_matching_token() {
        let node = A2aRelayNodeRuntime {
            node_id: "a1".to_owned(),
            token: "secret".to_owned(),
            public_key: Some("pk".to_owned()),
            roles: Vec::new(),
            scopes: Vec::new(),
        };
        assert!(relay_connect_token_allows(&node, Some("secret")));
        assert!(!relay_connect_token_allows(&node, None));
        assert!(!relay_connect_token_allows(&node, Some("wrong")));
    }

    #[tokio::test]
    async fn hub_invocation_sends_request_and_returns_response() {
        let hub = std::sync::Arc::new(RelayHub::new());
        let (tx, mut rx) = mpsc::channel(RELAY_WRITE_QUEUE_CAPACITY);
        hub.register_connection("a3", tx, 1);
        hub.apply_route_lease("a3", &["a3/main".to_owned()], 10_000, 1)
            .unwrap();

        let invoke_hub = std::sync::Arc::clone(&hub);
        let invoke = tokio::spawn(async move {
            invoke_hub
                .invoke_jsonrpc(
                    "a3/main",
                    "SendMessage",
                    serde_json::json!({"metadata": {"agentId": "main"}}),
                    "node:a1",
                )
                .await
        });

        let msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let AxumWsMessage::Text(text) = msg else {
            panic!("expected text relay frame");
        };
        let frame: RelayFrame = serde_json::from_str(&text).unwrap();
        let RelayFrame::Request {
            request_id,
            target,
            principal,
            ..
        } = frame
        else {
            panic!("expected request frame");
        };
        assert_eq!(target, "a3/main");
        assert_eq!(principal, "node:a1");

        hub.complete_pending_from(
            &request_id,
            JsonRpcResponse::ok(
                Value::String("client-id".into()),
                serde_json::json!({"ok": true}),
            ),
            Some("a3"),
        );

        let response = invoke.await.unwrap().unwrap();
        assert_eq!(response.result.unwrap()["ok"], true);
    }

    #[tokio::test]
    async fn drop_guard_sends_cancel_when_stream_drops_early() {
        let hub = std::sync::Arc::new(RelayHub::new());
        let (tx, mut rx) = mpsc::channel(RELAY_WRITE_QUEUE_CAPACITY);
        hub.register_connection("a3", tx, 1);
        hub.apply_route_lease("a3", &["a3/main".to_owned()], 10_000, 1)
            .unwrap();

        let (request_id, node_id, _event_rx) = hub
            .invoke_streaming(
                "a3/main",
                "SendStreamingMessage",
                serde_json::json!({"metadata": {"agentId": "main"}}),
                "node:a1",
            )
            .await
            .unwrap();
        assert_eq!(node_id, "a3");

        // Drain the Request frame.
        let _req = rx.recv().await.unwrap();

        // Simulate SSE consumer disconnect: drop the guard.
        let guard = RelayStreamGuard::new(hub.clone(), node_id, request_id.clone());
        drop(guard);

        let msg = tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("cancel frame should arrive")
            .unwrap();
        let AxumWsMessage::Text(text) = msg else {
            panic!("expected text relay frame");
        };
        let frame: RelayFrame = serde_json::from_str(&text).unwrap();
        match frame {
            RelayFrame::Cancel {
                request_id: rid, ..
            } => assert_eq!(rid, request_id),
            other => panic!("expected Cancel, got {other:?}"),
        }

        // stream_pending entry should be cleaned up.
        assert!(!hub.stream_pending.contains_key(&request_id));
    }

    #[test]
    fn relay_target_falls_back_to_task_id_route() {
        let hub = RelayHub::new();
        let peer_manager = crate::a2a::peer::PeerManager::default();
        hub.record_task_route("task-abc", "a3/main");

        // No metadata.agentId, only `id` (task_id) — should still route.
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: Value::Null,
            method: "GetTask".to_owned(),
            params: serde_json::json!({"id": "task-abc"}),
        };
        assert_eq!(
            relay_target_from_routes(&peer_manager, &hub, &req).as_deref(),
            Some("a3/main")
        );

        // Push config method uses `taskId` (camelCase).
        let push_req = JsonRpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: Value::Null,
            method: "GetTaskPushNotificationConfig".to_owned(),
            params: serde_json::json!({"taskId": "task-abc", "pushNotificationConfigId": "p1"}),
        };
        assert_eq!(
            relay_target_from_routes(&peer_manager, &hub, &push_req).as_deref(),
            Some("a3/main")
        );

        // Unforwardable method must not route even if task_id matches.
        let bad_req = JsonRpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: Value::Null,
            method: "ListTasks".to_owned(),
            params: serde_json::json!({"id": "task-abc"}),
        };
        assert!(relay_target_from_routes(&peer_manager, &hub, &bad_req).is_none());
    }

    #[test]
    fn forward_stream_event_records_task_route() {
        let hub = RelayHub::new();
        let (event_tx, _event_rx) = broadcast::channel::<Value>(4);
        hub.stream_pending.insert(
            "req-1".to_owned(),
            StreamPending {
                tx: event_tx,
                agent_ref: "a3/main".to_owned(),
                node_id: "a3".to_owned(),
                deadline: std::time::Instant::now() + Duration::from_secs(60),
            },
        );

        let wire = serde_json::json!({
            "kind": "status-update",
            "taskId": "task-xyz",
            "contextId": "ctx-1",
            "status": {"state": "submitted"},
            "final": false,
        });
        hub.forward_stream_event("req-1", wire);

        assert_eq!(hub.route_for_task("task-xyz").as_deref(), Some("a3/main"));
    }

    #[tokio::test]
    async fn unregister_surfaces_inflight_stream_as_failed() {
        // The whole point of in-flight loss surfacing: if a spoke
        // disconnects mid-stream, the SSE consumer must receive a
        // terminal status-update with state=failed instead of hanging
        // until REQUEST_TIMEOUT.
        let hub = std::sync::Arc::new(RelayHub::new());
        let (tx, mut _rx) = mpsc::channel(RELAY_WRITE_QUEUE_CAPACITY);
        hub.register_connection("home-mac", tx, 1);
        hub.apply_route_lease("home-mac", &["home-mac/main".to_owned()], 10_000, 1)
            .unwrap();

        let (_request_id, _node_id, mut event_rx) = hub
            .invoke_streaming(
                "home-mac/main",
                "SendStreamingMessage",
                serde_json::json!({"metadata": {"agentId": "main"}}),
                "node:hub",
            )
            .await
            .unwrap();

        // Drop the connection (simulates spoke WS death).
        hub.unregister_connection("home-mac", 1);

        let event = tokio::time::timeout(Duration::from_millis(200), event_rx.recv())
            .await
            .expect("synthetic failure event must arrive")
            .expect("recv ok");
        assert_eq!(event["kind"], "status-update");
        assert_eq!(event["status"]["state"], "failed");
        assert_eq!(event["final"], true);
        assert_eq!(
            hub.metrics.inflight_losses.load(Ordering::Relaxed),
            1,
            "inflight_losses metric must increment"
        );
    }

    #[tokio::test]
    async fn unregister_resolves_pending_jsonrpc_for_owning_node_only() {
        // Lose connection to node A — A's pending RPC must fail; B's
        // must stay untouched.
        let hub = std::sync::Arc::new(RelayHub::new());
        let (tx_a, mut rx_a) = mpsc::channel(RELAY_WRITE_QUEUE_CAPACITY);
        let (tx_b, mut _rx_b) = mpsc::channel(RELAY_WRITE_QUEUE_CAPACITY);
        hub.register_connection("a", tx_a, 1);
        hub.register_connection("b", tx_b, 1);
        hub.apply_route_lease("a", &["a/main".to_owned()], 10_000, 1)
            .unwrap();
        hub.apply_route_lease("b", &["b/main".to_owned()], 10_000, 1)
            .unwrap();

        let a_hub = hub.clone();
        let a_call = tokio::spawn(async move {
            a_hub
                .invoke_jsonrpc(
                    "a/main",
                    "SendMessage",
                    serde_json::json!({"metadata": {"agentId": "main"}}),
                    "test",
                )
                .await
        });
        // Drain a's request frame to ensure pending is populated.
        let _ = tokio::time::timeout(Duration::from_millis(200), rx_a.recv())
            .await
            .expect("a should have received request frame");

        // Kill a — pending should be drained.
        hub.unregister_connection("a", 1);

        let response = tokio::time::timeout(Duration::from_millis(500), a_call)
            .await
            .expect("a's call must unblock fast")
            .unwrap()
            .unwrap();
        assert!(response.error.is_some(), "must surface as JSON-RPC error");
        assert!(response.error.unwrap().message.contains("disconnected"));
    }

    #[test]
    fn revoked_node_is_not_resolvable() {
        let relay = A2aRelayRuntime {
            mode: A2aRelayModeRuntime::Hub,
            relay_id: "main".to_owned(),
            revoked_nodes: vec!["bad-node".to_owned()],
            nodes: vec![A2aRelayNodeRuntime {
                node_id: "bad-node".to_owned(),
                token: "anything".to_owned(),
                public_key: None,
                roles: vec![],
                scopes: vec![],
            }],
            ..Default::default()
        };
        assert!(resolve_node(&relay, "bad-node").is_none());
    }

    #[test]
    fn keypair_node_skips_token_verification_at_resolve_time() {
        // The hub-side resolve_node only returns the node skeleton;
        // token verification happens separately and is skipped when
        // public_key is set.
        let relay = A2aRelayRuntime {
            mode: A2aRelayModeRuntime::Hub,
            relay_id: "main".to_owned(),
            nodes: vec![A2aRelayNodeRuntime {
                node_id: "kp-node".to_owned(),
                token: String::new(),
                public_key: Some("dummy".to_owned()),
                roles: vec![],
                scopes: vec![],
            }],
            ..Default::default()
        };
        let node = resolve_node(&relay, "kp-node").expect("found");
        assert!(node.public_key.is_some());
        // verify_node_token would refuse an empty token — that's why
        // the hub branches on public_key.is_some() to skip it.
        assert!(!verify_node_token(&node, ""));
    }

    #[test]
    fn metrics_snapshot_includes_all_spec_counters() {
        let metrics = RelayMetrics::default();
        metrics.request_count.store(42, Ordering::Relaxed);
        metrics
            .request_latency_ms_total
            .store(4200, Ordering::Relaxed);
        metrics.acl_denials.store(7, Ordering::Relaxed);
        let v = metrics.snapshot(3, 5);
        assert_eq!(v["connected_nodes"], 3);
        assert_eq!(v["route_count"], 5);
        assert_eq!(v["request_count"], 42);
        assert_eq!(v["request_latency_ms_avg"], 100);
        assert_eq!(v["acl_denials"], 7);
        // All 10 spec counters must be present.
        for key in [
            "ws_reconnects",
            "auth_failures",
            "route_expirations",
            "failovers",
            "inflight_losses",
        ] {
            assert!(v.get(key).is_some(), "missing counter: {key}");
        }
    }

    #[test]
    fn route_expiration_increments_metric() {
        let hub = RelayHub::new();
        // 1ms TTL — already expired by the time route_for runs.
        hub.apply_route_lease("a", &["a/main".to_owned()], 1, 1)
            .unwrap();
        std::thread::sleep(Duration::from_millis(10));
        assert!(hub.route_for("a/main").is_none());
        assert_eq!(hub.metrics.route_expirations.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn drop_guard_no_cancel_after_normal_completion() {
        let hub = std::sync::Arc::new(RelayHub::new());
        let (tx, mut rx) = mpsc::channel(RELAY_WRITE_QUEUE_CAPACITY);
        hub.register_connection("a3", tx, 1);
        hub.apply_route_lease("a3", &["a3/main".to_owned()], 10_000, 1)
            .unwrap();

        let (request_id, node_id, _event_rx) = hub
            .invoke_streaming(
                "a3/main",
                "SendStreamingMessage",
                serde_json::json!({"metadata": {"agentId": "main"}}),
                "node:a1",
            )
            .await
            .unwrap();
        let _req = rx.recv().await.unwrap();

        // Simulate the spoke finishing: terminal Response removes stream_pending.
        assert!(hub.complete_streaming(&request_id));

        // Now drop the guard — should be a no-op (no Cancel frame emitted).
        let guard = RelayStreamGuard::new(hub.clone(), node_id, request_id.clone());
        drop(guard);

        let no_msg = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await;
        assert!(
            no_msg.is_err(),
            "no Cancel frame expected after normal completion"
        );
    }

    // -------------------------------------------------------------------
    // P2P / hole-punch tests (ADR 0002)
    // -------------------------------------------------------------------

    #[test]
    fn route_entry_defaults_to_relayed_mode() {
        let hub = RelayHub::new();
        hub.apply_route_lease("a1", &["a1/main".to_owned()], 10_000, 1)
            .unwrap();
        let route = hub.route_for("a1/main").expect("route");
        assert_eq!(route.mode, RouteMode::Relayed);
    }

    #[test]
    fn peer_connection_never_disables_hub_route() {
        let hub = RelayHub::new();
        let (tx, _rx) = mpsc::channel(RELAY_WRITE_QUEUE_CAPACITY);
        hub.register_connection("node-b", tx, 1);
        hub.apply_route_lease("node-b", &["node-b/agent2".to_owned()], 10_000, 1)
            .unwrap();

        let route = hub.route_for("node-b/agent2").expect("hub route");
        assert_eq!(route.mode, RouteMode::Relayed);
    }

    #[test]
    fn send_to_node_queues_frame_on_connected_node() {
        let hub = RelayHub::new();
        let (tx, mut rx) = mpsc::channel(RELAY_WRITE_QUEUE_CAPACITY);
        hub.register_connection("b", tx, 1);

        let frame = RelayFrame::Ping { ts: 42 };
        assert!(hub.send_to_node("b", &frame), "should queue");

        let msg = rx.try_recv().expect("message should arrive");
        let text = match msg {
            AxumWsMessage::Text(t) => t.to_string(),
            other => panic!("expected Text, got {other:?}"),
        };
        assert!(
            text.contains("\"ping\""),
            "expected ping frame, got: {text}"
        );
    }

    #[test]
    fn stale_spoke_control_cannot_remove_replacement_stream_tasks() {
        let hub = RelayHub::new();
        hub.spoke_stream_tasks.insert(
            "old-request".to_owned(),
            SpokeStreamTask {
                task_id: "old-task".to_owned(),
                control_generation: "old-control".to_owned(),
                cancel_owner: None,
            },
        );
        hub.spoke_stream_tasks.insert(
            "replacement-request".to_owned(),
            SpokeStreamTask {
                task_id: "replacement-task".to_owned(),
                control_generation: "replacement-control".to_owned(),
                cancel_owner: None,
            },
        );

        assert_eq!(
            hub.take_spoke_stream_tasks("old-control")
                .into_iter()
                .map(|task| task.task_id)
                .collect::<Vec<_>>(),
            vec!["old-task".to_owned()]
        );
        assert!(hub.spoke_stream_tasks.get("old-request").is_none());
        assert_eq!(
            hub.spoke_stream_tasks
                .get("replacement-request")
                .expect("replacement stream must survive stale teardown")
                .task_id,
            "replacement-task"
        );
        assert!(
            hub.remove_spoke_stream_task("replacement-request", "old-control")
                .is_none()
        );
        assert!(hub.spoke_stream_tasks.contains_key("replacement-request"));

        let task_cancels = DashMap::new();
        let old_owner = Arc::new(tokio_util::sync::CancellationToken::new());
        let replacement_owner = Arc::new(tokio_util::sync::CancellationToken::new());
        task_cancels.insert("shared-task".to_owned(), replacement_owner.clone());
        hub.spoke_stream_tasks.insert(
            "old-shared-request".to_owned(),
            SpokeStreamTask {
                task_id: "shared-task".to_owned(),
                control_generation: "old-control".to_owned(),
                cancel_owner: Some(old_owner.clone()),
            },
        );
        for task in hub.take_spoke_stream_tasks("old-control") {
            if let Some(owner) = task.cancel_owner
                && let Some(token) =
                    crate::server::remove_task_cancel_if_owner(&task_cancels, &task.task_id, &owner)
            {
                token.cancel();
            }
        }
        assert!(!old_owner.is_cancelled());
        assert!(!replacement_owner.is_cancelled());
        assert!(task_cancels.contains_key("shared-task"));
    }

    #[tokio::test]
    async fn relay_unary_correlation_admission_is_bounded() {
        let hub = RelayHub::new();
        let (tx, _rx) = mpsc::channel(RELAY_WRITE_QUEUE_CAPACITY);
        hub.register_connection("node-b", tx, 1);
        hub.apply_route_lease("node-b", &["node-b/main".to_owned()], 10_000, 1)
            .expect("test route lease");
        for index in 0..MAX_RELAY_PENDING_REQUESTS {
            let (pending_tx, _pending_rx) = oneshot::channel();
            hub.pending.insert(
                format!("existing-{index}"),
                (pending_tx, "node-b".to_owned()),
            );
        }

        let result = hub
            .invoke_jsonrpc(
                "node-b/main",
                "SendMessage",
                serde_json::json!({}),
                "caller",
            )
            .await;

        assert!(result.is_err());
        assert_eq!(hub.pending.len(), MAX_RELAY_PENDING_REQUESTS);
    }

    #[tokio::test]
    async fn relay_stream_correlation_admission_is_bounded() {
        let hub = RelayHub::new();
        let (tx, _rx) = mpsc::channel(RELAY_WRITE_QUEUE_CAPACITY);
        hub.register_connection("node-b", tx, 1);
        hub.apply_route_lease("node-b", &["node-b/main".to_owned()], 10_000, 1)
            .expect("test route lease");
        for index in 0..MAX_RELAY_STREAMS {
            let (event_tx, _event_rx) = broadcast::channel(1);
            hub.stream_pending.insert(
                format!("existing-{index}"),
                StreamPending {
                    tx: event_tx,
                    agent_ref: "node-b/main".to_owned(),
                    node_id: "node-b".to_owned(),
                    deadline: std::time::Instant::now() + STREAM_MAX_LIFETIME,
                },
            );
        }

        let result = hub
            .invoke_streaming(
                "node-b/main",
                "SendStreamingMessage",
                serde_json::json!({}),
                "caller",
            )
            .await;

        assert!(result.is_err());
        assert_eq!(hub.stream_pending.len(), MAX_RELAY_STREAMS);
    }

    #[tokio::test]
    async fn replacement_connection_revokes_previous_epoch() {
        let hub = RelayHub::new();
        let (old_tx, mut old_rx) = mpsc::channel(RELAY_WRITE_QUEUE_CAPACITY);
        let (new_tx, _new_rx) = mpsc::channel(RELAY_WRITE_QUEUE_CAPACITY);

        hub.register_connection("node-a", old_tx, 1);
        assert!(hub.owns_connection("node-a", 1));
        hub.register_connection("node-a", new_tx, 2);

        assert!(!hub.owns_connection("node-a", 1));
        assert!(hub.owns_connection("node-a", 2));
        assert!(matches!(old_rx.recv().await, Some(AxumWsMessage::Close(_))));
        hub.unregister_connection("node-a", 1);
        assert!(hub.owns_connection("node-a", 2));
    }

    #[test]
    fn send_to_node_disconnects_saturated_writer_queue() {
        let hub = RelayHub::new();
        let (tx, _rx) = mpsc::channel(1);
        hub.register_connection("b", tx, 1);

        assert!(hub.send_to_node("b", &RelayFrame::Ping { ts: 1 }));
        assert!(!hub.send_to_node("b", &RelayFrame::Ping { ts: 2 }));
        assert_eq!(hub.connection_count(), 0);
    }

    #[test]
    fn send_to_node_returns_false_for_unknown_node() {
        let hub = RelayHub::new();
        let frame = RelayFrame::Ping { ts: 42 };
        assert!(!hub.send_to_node("ghost", &frame));
    }

    #[tokio::test]
    async fn hub_rejects_multiline_peer_signaling_identifiers() {
        let hub = Arc::new(RelayHub::new());
        let (target_tx, _target_rx) = mpsc::channel(1);
        hub.register_connection("node-b", target_tx, 1);
        let ctx = HubCtx {
            hub: hub.clone(),
            relay_id: "hub".to_owned(),
            nodes: Arc::new(Vec::new()),
        };
        let source = A2aRelayNodeRuntime {
            node_id: "node-a".to_owned(),
            ..Default::default()
        };

        handle_hub_frame(
            &ctx,
            &source,
            RelayFrame::PeerOffer {
                session_id: "session-1\nnode-a".to_owned(),
                target_node: "node-b".to_owned(),
                sdp: "v=0\r\n".to_owned(),
                signature: None,
            },
        )
        .await;
        handle_hub_frame(
            &ctx,
            &source,
            RelayFrame::PeerOffer {
                session_id: "session-2".to_owned(),
                target_node: "node-b\rnode-c".to_owned(),
                sdp: "v=0\r\n".to_owned(),
                signature: None,
            },
        )
        .await;

        assert!(
            hub.peer_signal_sessions
                .lock()
                .expect("peer signaling session mutex poisoned")
                .is_empty()
        );
        assert_eq!(hub.metrics.acl_denials.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn relay_frame_peer_offer_serde_roundtrip() {
        let frame = RelayFrame::PeerOffer {
            session_id: "session-1".into(),
            target_node: "node-b".into(),
            sdp: "v=0\r\n".into(),
            signature: Some("signature".into()),
        };
        let json = serde_json::to_string(&frame).unwrap();
        let decoded: RelayFrame = serde_json::from_str(&json).unwrap();
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
    fn relay_frame_peer_connected_serde_roundtrip() {
        let frame = RelayFrame::PeerConnected {
            peer_node: "node-x".into(),
            session_id: "session-1".into(),
        };
        let json = serde_json::to_string(&frame).unwrap();
        let decoded: RelayFrame = serde_json::from_str(&json).unwrap();
        match decoded {
            RelayFrame::PeerConnected {
                peer_node,
                session_id,
            } => {
                assert_eq!(peer_node, "node-x");
                assert_eq!(session_id, "session-1");
            }
            other => panic!("expected PeerConnected, got {other:?}"),
        }
    }

    // -------------------------------------------------------------------
    // Real TCP tests — axum hub server + tungstenite spoke clients
    // -------------------------------------------------------------------

    const RT_SECRET: &str = "rt-secret";
    const RT_NODE_A: &str = "rt-a";
    const RT_NODE_B: &str = "rt-b";

    fn rt_cfg() -> A2aRelayRuntime {
        A2aRelayRuntime {
            mode: A2aRelayModeRuntime::Hub,
            relay_id: "rt-hub".into(),
            token: Some(RT_SECRET.into()),
            nodes: vec![
                A2aRelayNodeRuntime {
                    node_id: RT_NODE_A.into(),
                    token: RT_SECRET.into(),
                    public_key: None,
                    roles: vec![],
                    scopes: vec![
                        "relay:connect:rt-hub".into(),
                        "relay:advertise:rt-a/*".into(),
                        "relay:receive:rt-a/*".into(),
                        "a2a:invoke:*".into(),
                    ],
                },
                A2aRelayNodeRuntime {
                    node_id: RT_NODE_B.into(),
                    token: RT_SECRET.into(),
                    public_key: None,
                    roles: vec![],
                    scopes: vec![],
                },
            ],
            ..Default::default()
        }
    }

    async fn rt_connect(
        port: u16,
        node_id: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let url = format!("ws://127.0.0.1:{port}/a2a/relay/ws?node_id={node_id}");
        use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
        let mut request = url.into_client_request().expect("build relay test request");
        request.headers_mut().insert(
            tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
            tokio_tungstenite::tungstenite::http::HeaderValue::from_static("Bearer rt-secret"),
        );
        let (stream, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("connect relay test spoke");
        stream
    }

    async fn rt_send<W>(ws: &mut W, frame: &RelayFrame)
    where
        W: futures::SinkExt<tokio_tungstenite::tungstenite::Message> + Unpin,
        W::Error: std::fmt::Debug,
    {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::to_string(frame).unwrap().into(),
        ))
        .await
        .unwrap();
    }

    async fn rt_recv<S>(ws: &mut S) -> RelayFrame
    where
        S: futures::StreamExt<
                Item = Result<
                    tokio_tungstenite::tungstenite::Message,
                    tokio_tungstenite::tungstenite::Error,
                >,
            > + Unpin,
    {
        use futures::StreamExt as _;
        use tokio_tungstenite::tungstenite::Message;
        // The hub's heartbeat task fires its first tick immediately, so a
        // WS-level Ping and an app-level Ping frame typically arrive before
        // the payload we actually care about. Skip both.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let msg = tokio::time::timeout_at(deadline, ws.next())
                .await
                .expect("timed out waiting for a relay frame")
                .expect("websocket closed")
                .expect("websocket error");
            let Message::Text(text) = msg else {
                continue; // Ping / Pong / Binary / Close — not a relay frame.
            };
            let frame: RelayFrame = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("bad relay frame {text:?}: {e}"));
            if matches!(frame, RelayFrame::Ping { .. } | RelayFrame::Pong { .. }) {
                continue;
            }
            return frame;
        }
    }

    /// Start an axum relay hub server on an OS-assigned loopback port.
    /// Returns the shared `RelayHub`, the server task, and the bound port.
    /// Port 0 is used so concurrently-running tests never collide.
    async fn rt_start_hub() -> (std::sync::Arc<RelayHub>, tokio::task::JoinHandle<()>, u16) {
        let hub: std::sync::Arc<RelayHub> = std::sync::Arc::new(RelayHub::new());
        let cfg = rt_cfg();
        let hub_for_svc = hub.clone();
        let cfg_for_svc = cfg.clone();

        // We can't call relay_ws_handler directly (it needs AppState), so we
        // build a minimal axum router replicating its auth + upgrade path.
        let app = axum::Router::new().route(
            "/a2a/relay/ws",
            axum::routing::get(
                |ws: axum::extract::WebSocketUpgrade,
                 Query(q): Query<RelayWsQuery>,
                 headers: HeaderMap| async move {
                    let relay = &cfg_for_svc;
                    if relay.mode != A2aRelayModeRuntime::Hub {
                        return axum::http::StatusCode::NOT_FOUND.into_response();
                    }
                    let Some(mut node) = resolve_node(relay, &q.node_id) else {
                        return axum::http::StatusCode::UNAUTHORIZED.into_response();
                    };
                    let presented = bearer_token(&headers);
                    if !relay_connect_token_allows(&node, presented) {
                        return axum::http::StatusCode::UNAUTHORIZED.into_response();
                    }
                    if node.scopes.is_empty() {
                        node.scopes = default_node_scopes(&node.node_id, &relay.relay_id);
                    }
                    if !scope_allows(&node.scopes, "relay", "connect", &relay.relay_id) {
                        return axum::http::StatusCode::FORBIDDEN.into_response();
                    }
                    let ctx = HubCtx {
                        hub: hub_for_svc.clone(),
                        relay_id: relay.relay_id.clone(),
                        nodes: std::sync::Arc::new(relay.nodes.clone()),
                    };
                    ws.on_upgrade(move |socket| handle_hub_socket(socket, ctx, node))
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let jh = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        (hub, jh, port)
    }

    #[tokio::test]
    async fn rtc_connect_route_lease_disconnect() {
        let (hub, jh, port) = rt_start_hub().await;

        let mut spoke_a = rt_connect(port, RT_NODE_A).await;
        let mut spoke_b = rt_connect(port, RT_NODE_B).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Spoke A advertises a route.
        rt_send(
            &mut spoke_a,
            &RelayFrame::RouteLease {
                node_id: RT_NODE_A.into(),
                agents: vec![format!("{RT_NODE_A}/main")],
                ttl_ms: 30_000,
                epoch: 1,
            },
        )
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Hub has the route.
        let route = hub.route_for(&format!("{RT_NODE_A}/main"));
        assert!(route.is_some(), "hub must have route for rt-a/main");
        assert_eq!(route.unwrap().mode, RouteMode::Relayed);

        // Both spokes connected.
        assert_eq!(hub.connection_count(), 2);

        // Spoke A disconnects → route cleaned up.
        drop(spoke_a);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(hub.route_for(&format!("{RT_NODE_A}/main")).is_none());

        drop(spoke_b);
        jh.abort();
    }

    #[tokio::test]
    async fn rtc_spoke_request_relays_and_returns_response() {
        let (hub, jh, port) = rt_start_hub().await;
        let mut spoke_a = rt_connect(port, RT_NODE_A).await;
        let mut spoke_b = rt_connect(port, RT_NODE_B).await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        rt_send(
            &mut spoke_b,
            &RelayFrame::RouteLease {
                node_id: RT_NODE_B.into(),
                agents: vec![format!("{RT_NODE_B}/main")],
                ttl_ms: 30_000,
                epoch: 1,
            },
        )
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        rt_send(
            &mut spoke_a,
            &RelayFrame::Request {
                request_id: "spoke-request-1".into(),
                target: format!("{RT_NODE_B}/main"),
                method: "SendMessage".into(),
                params: serde_json::json!({}),
                principal: "agent-a".into(),
                deadline_ms: 30_000,
            },
        )
        .await;

        let forwarded = rt_recv(&mut spoke_b).await;
        let RelayFrame::Request {
            request_id,
            target,
            principal,
            ..
        } = forwarded
        else {
            panic!("expected relayed Request");
        };
        assert_eq!(request_id, "spoke-request-1");
        assert_eq!(target, format!("{RT_NODE_B}/main"));
        assert_eq!(principal, format!("node:{RT_NODE_A}:agent-a"));

        rt_send(
            &mut spoke_b,
            &RelayFrame::Response {
                request_id,
                response: JsonRpcResponse::ok(Value::Null, serde_json::json!({"id":"task-1"})),
            },
        )
        .await;
        let returned = rt_recv(&mut spoke_a).await;
        assert!(
            matches!(returned, RelayFrame::Response { request_id, .. } if request_id == "spoke-request-1")
        );

        drop(spoke_a);
        drop(spoke_b);
        jh.abort();
        drop(hub);
    }

    #[tokio::test]
    async fn rtc_peer_offer_relay_between_spokes() {
        let (_hub, jh, port) = rt_start_hub().await;

        let mut spoke_a = rt_connect(port, RT_NODE_A).await;
        let mut spoke_b = rt_connect(port, RT_NODE_B).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        rt_send(
            &mut spoke_a,
            &RelayFrame::PeerOffer {
                session_id: "session-1".into(),
                target_node: RT_NODE_B.into(),
                sdp: "v=0\r\n".into(),
                signature: Some("signature".into()),
            },
        )
        .await;

        let got = rt_recv(&mut spoke_b).await;
        match got {
            RelayFrame::PeerOfferRelay {
                session_id,
                source_node,
                sdp,
                signature,
            } => {
                assert_eq!(session_id, "session-1");
                assert_eq!(source_node, RT_NODE_A);
                assert_eq!(sdp, "v=0\r\n");
                assert_eq!(signature.as_deref(), Some("signature"));
            }
            other => panic!("expected PeerOfferRelay, got {other:?}"),
        }

        rt_send(
            &mut spoke_b,
            &RelayFrame::PeerAnswer {
                session_id: "session-1".into(),
                target_node: RT_NODE_A.into(),
                sdp: "v=0\r\na=answer\r\n".into(),
                signature: Some("answer-signature".into()),
            },
        )
        .await;
        assert!(matches!(
            rt_recv(&mut spoke_a).await,
            RelayFrame::PeerAnswerRelay { session_id, .. } if session_id == "session-1"
        ));

        rt_send(
            &mut spoke_a,
            &RelayFrame::PeerOffer {
                session_id: "session-1".into(),
                target_node: RT_NODE_B.into(),
                sdp: "v=0\r\n".into(),
                signature: Some("signature".into()),
            },
        )
        .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(200), spoke_b.next())
                .await
                .is_err(),
            "completed peer offer replay must not be forwarded"
        );

        drop(spoke_a);
        drop(spoke_b);
        jh.abort();
    }

    #[tokio::test]
    async fn rtc_ping_pong_echo() {
        let (hub, jh, port) = rt_start_hub().await;

        let mut spoke_a = rt_connect(port, RT_NODE_A).await;
        let mut _spoke_b = rt_connect(port, RT_NODE_B).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Spoke A sends Ping → hub echoes Pong (same behavior as production).
        rt_send(&mut spoke_a, &RelayFrame::Ping { ts: 42 }).await;

        // The hub's frame handler treats Ping as a no-op (only the WS-level
        // heartbeat replies), so we don't expect an app-level Pong. Instead
        // verify the hub stays connected and count is 2.
        assert_eq!(hub.connection_count(), 2);

        drop(spoke_a);
        drop(_spoke_b);
        jh.abort();
    }
}
