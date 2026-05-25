//! rsclaw private A2A relay transport.
//!
//! Public A2A remains JSON-RPC over HTTP/SSE. This module is the private
//! outbound-WS transport that lets NAT/private nodes attach to a hub.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

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
    config::runtime::{
        A2aRelayModeRuntime, A2aRelayNodeRuntime, A2aRelayRuntime, A2aRelayStrategyRuntime,
    },
    server::{AppState, constant_time_eq},
};

const RELAY_PROTOCOL: &str = "rsclaw.a2a.relay.v1";
const ROUTE_TTL_MS: u64 = 30_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Max time we wait for the spoke to complete keypair auth after WS upgrade
/// before dropping the connection. Long enough to absorb cross-continent
/// RTT, short enough to free slots fast under attack.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
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
}

#[derive(Debug, Clone)]
struct Connection {
    tx: mpsc::UnboundedSender<AxumWsMessage>,
    epoch: u64,
}

#[derive(Debug, Clone)]
pub struct RouteEntry {
    pub agent_ref: String,
    pub node_id: String,
    pub epoch: u64,
    pub expires_at: std::time::Instant,
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
    routes: DashMap<String, RouteEntry>,
    /// request_id → (waiter, target_node_id). We carry node_id so that
    /// when a connection drops we can resolve only its pending waiters
    /// instead of nuking every in-flight JSON-RPC call.
    pending: DashMap<String, (oneshot::Sender<JsonRpcResponse>, String)>,
    /// Streaming relay entries: request_id → (broadcast sender, agent_ref).
    /// The agent_ref is kept so we can record `task_id → agent_ref` once
    /// the first event arrives carrying a taskId. Inserted by
    /// `invoke_streaming`; removed when the spoke sends its terminal
    /// `RelayFrame::Response` for the streaming request.
    stream_pending: DashMap<String, (broadcast::Sender<Value>, String)>,
    /// Spoke-side map of relay request_id → local task_id, so a `Cancel`
    /// frame can find the local `CancellationToken` in `task_cancels`.
    spoke_stream_tasks: DashMap<String, String>,
    /// Hub-side cache of task_id → agent_ref ("node/agent"), populated by
    /// sniffing responses and streaming events as they pass through the
    /// hub. Lets follow-up task-bound RPCs (GetTask, CancelTask, push
    /// config ops, SubscribeToTask) route to the right spoke even when
    /// the client only knows the task_id.
    task_routes: DashMap<String, String>,
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
    pub fn new() -> Self {
        Self::default()
    }

    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    pub fn connected_nodes(&self) -> Vec<String> {
        let mut nodes: Vec<String> = self
            .connections
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        nodes.sort();
        nodes
    }

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

    pub fn route_count(&self) -> usize {
        self.routes.len()
    }

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
                },
            );
        }
        Ok(())
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
        self.pending
            .insert(request_id.clone(), (tx, node_id.clone()));
        let frame = RelayFrame::Request {
            request_id: request_id.clone(),
            target: target.to_owned(),
            method: method.to_owned(),
            params,
            principal: principal.to_owned(),
            deadline_ms: REQUEST_TIMEOUT.as_millis() as u64,
        };
        let msg = AxumWsMessage::Text(serde_json::to_string(&frame)?.into());
        if let Err(e) = conn.tx.send(msg) {
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

    fn register_connection(
        &self,
        node_id: &str,
        tx: mpsc::UnboundedSender<AxumWsMessage>,
        epoch: u64,
    ) {
        self.connections
            .insert(node_id.to_owned(), Connection { tx, epoch });
    }

    fn unregister_connection(&self, node_id: &str, epoch: u64) {
        if let Some(conn) = self.connections.get(node_id)
            && conn.epoch != epoch
        {
            return;
        }
        self.connections.remove(node_id);
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
            .filter(|entry| entry.value().1.starts_with(&prefix))
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
            if let Some((_, (tx, _))) = self.pending.remove(&k) {
                let _ = tx.send(JsonRpcResponse::err(
                    Value::Null,
                    -32004,
                    format!("relay node '{node_id}' disconnected"),
                ));
            }
        }
    }

    fn complete_pending(&self, request_id: &str, response: JsonRpcResponse) {
        if let Some((_, (tx, _node))) = self.pending.remove(request_id) {
            let _ = tx.send(response);
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
        self.stream_pending
            .insert(request_id.clone(), (event_tx, target.to_owned()));
        let frame = RelayFrame::Request {
            request_id: request_id.clone(),
            target: target.to_owned(),
            method: method.to_owned(),
            params,
            principal: principal.to_owned(),
            deadline_ms: REQUEST_TIMEOUT.as_millis() as u64,
        };
        let msg = AxumWsMessage::Text(serde_json::to_string(&frame)?.into());
        if let Err(e) = conn.tx.send(msg) {
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
        if let Ok(s) = serde_json::to_string(&frame) {
            let _ = conn.tx.send(AxumWsMessage::Text(s.into()));
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
                .insert(task_id.to_owned(), entry.1.clone());
        }
        entry.0.send(value).unwrap_or(0)
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
    pub fn new(
        relay_hub: std::sync::Arc<RelayHub>,
        node_id: String,
        request_id: String,
    ) -> Self {
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

pub fn validate_agent_ref(agent_ref: &str) -> Result<()> {
    let Some((node, agent)) = agent_ref.split_once('/') else {
        anyhow::bail!("agent_ref must be '<node>/<agent>'");
    };
    if node.is_empty() || agent.is_empty() || agent.contains('/') {
        anyhow::bail!("invalid agent_ref '{agent_ref}'");
    }
    Ok(())
}

pub fn local_agent_from_ref(agent_ref: &str, node_id: &str) -> Option<String> {
    let (node, agent) = agent_ref.split_once('/')?;
    (node == node_id && !agent.is_empty() && !agent.contains('/')).then(|| agent.to_owned())
}

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

/// Resolve a relay node by `node_id`. Token verification is the caller's
/// responsibility — callers must check whether the node requires token
/// auth (no public_key) or keypair auth (public_key set), and validate
/// the token only in the former case. Returns None if `node_id` is
/// unknown OR present on the revocation list.
fn resolve_node(relay: &A2aRelayRuntime, node_id: &str) -> Option<A2aRelayNodeRuntime> {
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
fn verify_node_token(node: &A2aRelayNodeRuntime, token: &str) -> bool {
    !node.token.is_empty() && constant_time_eq(&node.token, token)
}

#[derive(Debug, Deserialize)]
pub struct RelayWsQuery {
    node_id: String,
    #[serde(default)]
    token: Option<String>,
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
    // Token verification: only required when node is not keypair-only.
    // Keypair nodes must still pass challenge-response in handle_hub_socket
    // before they are registered.
    if node.public_key.is_none() {
        let presented = query.token.as_deref().or_else(|| bearer_token(&headers));
        let Some(token) = presented else {
            state
                .relay_hub
                .metrics
                .auth_failures
                .fetch_add(1, Ordering::Relaxed);
            audit_relay(
                "deny",
                &format!("node:{}", node.node_id),
                "connect",
                &format!("relay:{}", relay.relay_id),
                &relay.relay_id,
                &node.node_id,
                None,
                Some("no token presented and no public_key configured"),
            );
            return axum::http::StatusCode::UNAUTHORIZED.into_response();
        };
        if !verify_node_token(&node, token) {
            state
                .relay_hub
                .metrics
                .auth_failures
                .fetch_add(1, Ordering::Relaxed);
            audit_relay(
                "deny",
                &format!("node:{}", node.node_id),
                "connect",
                &format!("relay:{}", relay.relay_id),
                &relay.relay_id,
                &node.node_id,
                None,
                Some("token mismatch"),
            );
            return axum::http::StatusCode::UNAUTHORIZED.into_response();
        }
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
    ws.on_upgrade(move |socket| handle_hub_socket(socket, state, node))
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
        Ok(RelayFrame::Hello { nonce_node: None, .. }) => {
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
    let payload = serde_json::to_string(&challenge)
        .map_err(|e| format!("serialize Challenge: {e}"))?;
    if sink.send(AxumWsMessage::Text(payload.into())).await.is_err() {
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

async fn handle_hub_socket(socket: WebSocket, state: AppState, node: A2aRelayNodeRuntime) {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0);
    let (mut sink, mut stream) = socket.split();

    // Keypair handshake. Performed BEFORE register_connection so a node
    // that fails challenge-response never appears in `connections`
    // (cannot receive requests, cannot evict an existing well-behaved
    // session by same node_id).
    if let Some(public_key_b64) = node.public_key.as_deref() {
        let relay_id = state.config.gateway.a2a_relay.relay_id.clone();
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
                state
                    .relay_hub
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
                let _ = sink.send(AxumWsMessage::Close(None)).await;
                return;
            }
        }
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<AxumWsMessage>();
    let ping_tx = tx.clone();
    state
        .relay_hub
        .register_connection(&node.node_id, tx, epoch);
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
            if ping_tx.send(AxumWsMessage::Ping(Vec::new().into())).is_err() {
                break;
            }
            // 2. App-level JSON Ping — kept for backwards compatibility
            // with spoke side that may match on RelayFrame::Ping for RTT
            // bookkeeping.
            let frame = RelayFrame::Ping { ts };
            if let Ok(msg) = serde_json::to_string(&frame) {
                if ping_tx.send(AxumWsMessage::Text(msg.into())).is_err() {
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
        match serde_json::from_str::<RelayFrame>(&text) {
            Ok(frame) => handle_hub_frame(&state, &node, frame).await,
            Err(e) => warn!(node = %node.node_id, error = %e, "invalid a2a relay frame"),
        }
    }

    state.relay_hub.unregister_connection(&node.node_id, epoch);
    writer.abort();
    ping.abort();
    info!(node = %node.node_id, "a2a relay node disconnected");
}

async fn handle_hub_frame(state: &AppState, node: &A2aRelayNodeRuntime, frame: RelayFrame) {
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
            let relay_id = state.config.gateway.a2a_relay.relay_id.as_str();
            if node_id != node.node_id {
                warn!(node = %node.node_id, claimed = %node_id, "relay route lease node mismatch");
                state
                    .relay_hub
                    .metrics
                    .acl_denials
                    .fetch_add(1, Ordering::Relaxed);
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
                    state
                        .relay_hub
                        .metrics
                        .acl_denials
                        .fetch_add(1, Ordering::Relaxed);
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
            if let Err(e) = state
                .relay_hub
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
            // Streaming responses are signalled via Event frames; when the
            // spoke finishes it sends a Response to clean up the stream entry.
            if !state.relay_hub.complete_streaming(&request_id) {
                state.relay_hub.complete_pending(&request_id, response);
            }
        }
        RelayFrame::Event {
            request_id,
            result,
            ..
        } => {
            if state.relay_hub.forward_stream_event(&request_id, result) == 0 {
                debug!(request_id, "relay event for unknown stream");
            }
        }
        RelayFrame::Pong { .. } => {}
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
        .filter(|target| target.contains('/'))
        .map(str::to_owned)
}

/// Methods whose JSON-RPC params identify a target spoke either explicitly
/// via `metadata.agentId` or implicitly via a `task_id` (resolved through
/// the hub's task_routes cache, which is populated by sniffing responses
/// and streaming events).
const FORWARDABLE_METHODS: &[&str] = &[
    "SendMessage",
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

pub fn relay_target_from_request(
    hub: &RelayHub,
    req: &JsonRpcRequest,
) -> Option<String> {
    if !FORWARDABLE_METHODS.contains(&req.method.as_str()) {
        return None;
    }
    if let Some(target) = relay_target_from_params(&req.params) {
        return Some(target);
    }
    // Fall back to implicit routing by task_id.
    task_id_from_params(&req.params).and_then(|tid| hub.route_for_task(tid))
}

pub async fn try_forward_jsonrpc(
    state: &AppState,
    caller: Option<&A2aIdentity>,
    req: &JsonRpcRequest,
) -> Option<JsonRpcResponse> {
    let target = relay_target_from_request(&state.relay_hub, req)?;
    if state.relay_hub.route_for(&target).is_none() {
        return None;
    }
    let relay_id = state.config.gateway.a2a_relay.relay_id.as_str();
    let principal_id = caller.map(|id| id.id.as_str()).unwrap_or("anonymous-dev");
    if !can_invoke(caller, &target) {
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
    // Cross-node allow — log so operators can answer "why could caller X
    // invoke agent Y" after the fact (spec audit requirement).
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
    rewrite_target_agent_for_spoke(&mut params, &target);
    match state
        .relay_hub
        .invoke_jsonrpc(&target, &req.method, params, principal)
        .await
    {
        Ok(mut response) => {
            response.id = req.id.clone();
            // Sniff task_id from the response so follow-up RPCs that only
            // carry a task_id can be routed back to this spoke.
            if let Some(task_id) = response
                .result
                .as_ref()
                .and_then(|r| r.get("id"))
                .and_then(|v| v.as_str())
            {
                state.relay_hub.record_task_route(task_id, &target);
            }
            Some(response)
        }
        Err(e) => Some(JsonRpcResponse::err(req.id.clone(), -32004, e.to_string())),
    }
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
            relay_identity::signing_key_from_b64(pk)
                .context("parse spoke private_key (base64)")?,
        ),
        None => None,
    };
    if token.is_none() && signing_key.is_none() {
        anyhow::bail!("a2a relay spoke requires either token or private_key");
    }
    let sep = if hub_url.contains('?') { '&' } else { '?' };
    let mut url = format!("{hub_url}{sep}node_id={}", urlencoding::encode(node_id));
    if let Some(t) = token {
        url.push_str(&format!("&token={}", urlencoding::encode(t)));
    }
    let (stream, _) = tokio_tungstenite::connect_async(&url)
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
    let (write_tx, mut write_rx) = mpsc::unbounded_channel::<SpokeWriteItem>();
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
    let (spoke_tx, mut frame_rx) = mpsc::unbounded_channel::<RelayFrame>();
    let frame_adapter_tx = write_tx.clone();
    let frame_adapter = tokio::spawn(async move {
        while let Some(frame) = frame_rx.recv().await {
            if frame_adapter_tx.send(SpokeWriteItem::Frame(frame)).is_err() {
                break;
            }
        }
    });

    let nonce_node = signing_key.as_ref().map(|_| relay_identity::fresh_nonce_b64());
    spoke_tx
        .send(spoke_hello(&state, node_id, nonce_node.clone()))
        .map_err(|_| anyhow!("spoke writer closed"))?;
    // RouteLease is sent AFTER the Auth round-trip when in keypair mode
    // so the hub doesn't drop us mid-handshake. Token-mode spokes send
    // it immediately because there's no handshake.
    if signing_key.is_none() {
        spoke_tx
            .send(spoke_route_lease(&state, node_id, 1))
            .map_err(|_| anyhow!("spoke writer closed"))?;
    }

    // WS-level Ping every 15s so NAT/firewall idle counters reset.
    let ping_tx = write_tx.clone();
    let pinger = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        interval.tick().await; // skip immediate tick
        loop {
            interval.tick().await;
            if ping_tx.send(SpokeWriteItem::WsPing).is_err() {
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
            if renew_tx.send(frame).is_err() {
                break;
            }
            epoch = epoch.saturating_add(1);
        }
    });

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
                    .send(RelayFrame::Auth { signature: sig })
                    .map_err(|_| anyhow!("spoke writer closed"))?;
                // Now safe to publish routes — handshake done.
                spoke_tx
                    .send(spoke_route_lease(&state, node_id, 1))
                    .map_err(|_| anyhow!("spoke writer closed"))?;
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
                )
                .await;
                if let Some(response) = response {
                    let _ = spoke_tx.send(RelayFrame::Response {
                        request_id,
                        response,
                    });
                }
                // If response is None, the streaming handler sent events via
                // spoke_tx and will send the terminal Response itself.
            }
            RelayFrame::Ping { ts } => {
                let _ = spoke_tx.send(RelayFrame::Pong { ts });
            }
            RelayFrame::Cancel { request_id, .. } => {
                // Cancel the streaming task if it exists locally. `task_cancels`
                // is keyed by local task_id, so resolve via the spoke-side map.
                if let Some((_, task_id)) =
                    state.relay_hub.spoke_stream_tasks.remove(&request_id)
                    && let Some((_, token)) = state.task_cancels.remove(&task_id)
                {
                    token.cancel();
                }
            }
            _ => {}
        }
    }

    // WS drop reached: cancel every local streaming task we were
    // proxying so workers stop burning tokens for a stream that will
    // never reach the client. After reconnect, the hub will route new
    // requests through fresh task_ids; old request_ids are irrelevant.
    let request_ids: Vec<String> = state
        .relay_hub
        .spoke_stream_tasks
        .iter()
        .map(|e| e.key().clone())
        .collect();
    for rid in request_ids {
        if let Some((_, task_id)) = state.relay_hub.spoke_stream_tasks.remove(&rid)
            && let Some((_, token)) = state.task_cancels.remove(&task_id)
        {
            token.cancel();
        }
    }
    writer.abort();
    renewer.abort();
    pinger.abort();
    frame_adapter.abort();
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
    spoke_tx: mpsc::UnboundedSender<RelayFrame>,
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
        let (task_id, event_rx) =
            crate::a2a::streaming::spawn_streaming_task(state.clone(), caller, params).await;
        let request_id_for_relay = request_id.to_owned();
        state
            .relay_hub
            .spoke_stream_tasks
            .insert(request_id_for_relay.clone(), task_id.clone());
        let relay_hub = state.relay_hub.clone();
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
                        warn!(lagged = n, "spoke relay event lagged");
                    }
                }
            }
            relay_hub.spoke_stream_tasks.remove(&request_id_for_relay);
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
    use super::*;

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

    #[tokio::test]
    async fn hub_invocation_sends_request_and_returns_response() {
        let hub = std::sync::Arc::new(RelayHub::new());
        let (tx, mut rx) = mpsc::unbounded_channel();
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

        hub.complete_pending(
            &request_id,
            JsonRpcResponse::ok(
                Value::String("client-id".into()),
                serde_json::json!({"ok": true}),
            ),
        );

        let response = invoke.await.unwrap().unwrap();
        assert_eq!(response.result.unwrap()["ok"], true);
    }

    #[tokio::test]
    async fn drop_guard_sends_cancel_when_stream_drops_early() {
        let hub = std::sync::Arc::new(RelayHub::new());
        let (tx, mut rx) = mpsc::unbounded_channel();
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
            RelayFrame::Cancel { request_id: rid, .. } => assert_eq!(rid, request_id),
            other => panic!("expected Cancel, got {other:?}"),
        }

        // stream_pending entry should be cleaned up.
        assert!(!hub.stream_pending.contains_key(&request_id));
    }

    #[test]
    fn relay_target_falls_back_to_task_id_route() {
        let hub = RelayHub::new();
        hub.record_task_route("task-abc", "a3/main");

        // No metadata.agentId, only `id` (task_id) — should still route.
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: Value::Null,
            method: "GetTask".to_owned(),
            params: serde_json::json!({"id": "task-abc"}),
        };
        assert_eq!(relay_target_from_request(&hub, &req).as_deref(), Some("a3/main"));

        // Push config method uses `taskId` (camelCase).
        let push_req = JsonRpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: Value::Null,
            method: "GetTaskPushNotificationConfig".to_owned(),
            params: serde_json::json!({"taskId": "task-abc", "pushNotificationConfigId": "p1"}),
        };
        assert_eq!(
            relay_target_from_request(&hub, &push_req).as_deref(),
            Some("a3/main")
        );

        // Unforwardable method must not route even if task_id matches.
        let bad_req = JsonRpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: Value::Null,
            method: "ListTasks".to_owned(),
            params: serde_json::json!({"id": "task-abc"}),
        };
        assert!(relay_target_from_request(&hub, &bad_req).is_none());
    }

    #[test]
    fn forward_stream_event_records_task_route() {
        let hub = RelayHub::new();
        let (event_tx, _event_rx) = broadcast::channel::<Value>(4);
        hub.stream_pending
            .insert("req-1".to_owned(), (event_tx, "a3/main".to_owned()));

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
        let (tx, mut _rx) = mpsc::unbounded_channel();
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
        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut _rx_b) = mpsc::unbounded_channel();
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
        hub.apply_route_lease("a", &["a/main".to_owned()], 1, 1).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        assert!(hub.route_for("a/main").is_none());
        assert_eq!(hub.metrics.route_expirations.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn drop_guard_no_cancel_after_normal_completion() {
        let hub = std::sync::Arc::new(RelayHub::new());
        let (tx, mut rx) = mpsc::unbounded_channel();
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
        assert!(no_msg.is_err(), "no Cancel frame expected after normal completion");
    }
}
