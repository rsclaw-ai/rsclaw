//! rsclaw private A2A relay transport.
//!
//! Public A2A remains JSON-RPC over HTTP/SSE. This module is the private
//! outbound-WS transport that lets NAT/private nodes attach to a hub.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
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
use uuid::Uuid;

use crate::{
    a2a::{
        auth::A2aIdentity,
        types::{AgentCard, JsonRpcRequest, JsonRpcResponse},
    },
    config::runtime::{A2aRelayModeRuntime, A2aRelayNodeRuntime, A2aRelayRuntime},
    server::{AppState, constant_time_eq},
};

const RELAY_PROTOCOL: &str = "rsclaw.a2a.relay.v1";
const ROUTE_TTL_MS: u64 = 30_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

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

#[derive(Default)]
pub struct RelayHub {
    connections: DashMap<String, Connection>,
    routes: DashMap<String, RouteEntry>,
    pending: DashMap<String, oneshot::Sender<JsonRpcResponse>>,
    /// Streaming relay entries: request_id → broadcast sender for wire events.
    /// Inserted by `invoke_streaming`; removed when the spoke sends its
    /// terminal `RelayFrame::Response` for the streaming request.
    stream_pending: DashMap<String, broadcast::Sender<Value>>,
    /// Spoke-side map of relay request_id → local task_id, so a `Cancel`
    /// frame can find the local `CancellationToken` in `task_cancels`.
    spoke_stream_tasks: DashMap<String, String>,
}

impl RelayHub {
    pub fn new() -> Self {
        Self::default()
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
            return None;
        }
        Some(entry.clone())
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
        self.pending.insert(request_id.clone(), tx);
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
            anyhow::bail!("relay send to node '{}' failed: {e}", route.node_id);
        }
        drop(conn);
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err(anyhow!("relay response channel closed")),
            Err(_) => {
                self.pending.remove(&request_id);
                Err(anyhow!("relay request timed out"))
            }
        }
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
    }

    fn complete_pending(&self, request_id: &str, response: JsonRpcResponse) {
        if let Some((_, tx)) = self.pending.remove(request_id) {
            let _ = tx.send(response);
        }
    }

    /// Send a streaming Request to the spoke that owns `target` and return a
    /// broadcast receiver that will receive wire-event `Value`s as
    /// `RelayFrame::Event` frames arrive from the spoke.
    pub async fn invoke_streaming(
        &self,
        target: &str,
        method: &str,
        params: Value,
        principal: &str,
    ) -> Result<broadcast::Receiver<Value>> {
        let route = self
            .route_for(target)
            .ok_or_else(|| anyhow!("no live relay route for {target}"))?;
        let conn = self
            .connections
            .get(&route.node_id)
            .ok_or_else(|| anyhow!("node '{}' is not connected", route.node_id))?;
        let request_id = format!("relay:stream:{}", Uuid::new_v4());
        let (event_tx, event_rx) = broadcast::channel(128);
        self.stream_pending.insert(request_id.clone(), event_tx);
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
        Ok(event_rx)
    }

    /// Remove and drop the stream entry for `request_id`. Returns `true` if
    /// a streaming entry existed (and was cleaned up). Dropping the
    /// broadcast sender signals Closed to all receivers, which terminates
    /// the SSE stream.
    fn complete_streaming(&self, request_id: &str) -> bool {
        self.stream_pending.remove(request_id).is_some()
    }

    /// Route a wire-event `Value` to the stream subscriber for `request_id`,
    /// if one exists. Returns the number of active receivers.
    fn forward_stream_event(&self, request_id: &str, value: Value) -> usize {
        self.stream_pending
            .get(request_id)
            .map(|tx| tx.send(value).unwrap_or(0))
            .unwrap_or(0)
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

fn resolve_node(
    relay: &A2aRelayRuntime,
    node_id: &str,
    token: &str,
) -> Option<A2aRelayNodeRuntime> {
    relay.nodes.iter().find_map(|node| {
        (node.node_id == node_id && constant_time_eq(&node.token, token)).then(|| node.clone())
    })
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
    let Some(token) = query.token.as_deref().or_else(|| bearer_token(&headers)) else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    let Some(mut node) = resolve_node(relay, &query.node_id, token) else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    if node.scopes.is_empty() {
        node.scopes = default_node_scopes(&node.node_id, &relay.relay_id);
    }
    if !scope_allows(&node.scopes, "relay", "connect", &relay.relay_id) {
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

async fn handle_hub_socket(socket: WebSocket, state: AppState, node: A2aRelayNodeRuntime) {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0);
    let (mut sink, mut stream) = socket.split();
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
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        loop {
            interval.tick().await;
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
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
            if node_id != node.node_id {
                warn!(node = %node.node_id, claimed = %node_id, "relay route lease node mismatch");
                return;
            }
            for agent in &agents {
                if !scope_allows(&node.scopes, "relay", "advertise", agent) {
                    warn!(node = %node.node_id, agent, "relay advertise denied");
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
        RelayFrame::Cancel { request_id, .. } => {
            // Forward the cancel to the spoke that owns this streaming request.
            let frame = RelayFrame::Cancel {
                request_id: request_id.clone(),
                task_id: None,
            };
            let msg = match serde_json::to_string(&frame) {
                Ok(s) => AxumWsMessage::Text(s.into()),
                Err(e) => {
                    warn!(error = %e, "failed to serialize cancel frame");
                    return;
                }
            };
            // Send to every connected node — the spoke that owns the stream
            // will recognise the request_id and cancel its local task.
            for conn in state.relay_hub.connections.iter() {
                let _ = conn.tx.send(msg.clone());
            }
            // Drop the stream entry if present.
            state.relay_hub.complete_streaming(&request_id);
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

pub fn relay_target_from_request(req: &JsonRpcRequest) -> Option<String> {
    if !matches!(
        req.method.as_str(),
        "SendMessage" | "GetTask" | "CancelTask"
    ) {
        return None;
    }
    relay_target_from_params(&req.params)
}

pub async fn try_forward_jsonrpc(
    state: &AppState,
    caller: Option<&A2aIdentity>,
    req: &JsonRpcRequest,
) -> Option<JsonRpcResponse> {
    let target = relay_target_from_request(req)?;
    if state.relay_hub.route_for(&target).is_none() {
        return None;
    }
    if !can_invoke(caller, &target) {
        return Some(JsonRpcResponse::err(
            req.id.clone(),
            -32003,
            format!("not authorized to invoke {target}"),
        ));
    }
    let principal = caller.map(|id| id.id.as_str()).unwrap_or("anonymous-dev");
    let mut params = req.params.clone();
    rewrite_target_agent_for_spoke(&mut params, &target);
    match state
        .relay_hub
        .invoke_jsonrpc(&target, &req.method, params, principal)
        .await
    {
        Ok(mut response) => {
            response.id = req.id.clone();
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

pub fn start_spoke_if_configured(state: AppState) {
    if state.config.gateway.a2a_relay.mode != A2aRelayModeRuntime::Spoke {
        return;
    }
    let relay = state.config.gateway.a2a_relay.clone();
    tokio::spawn(async move {
        let mut delay = Duration::from_secs(1);
        const MAX_DELAY: Duration = Duration::from_secs(120);
        loop {
            if let Err(e) = run_spoke_once(state.clone(), relay.clone()).await {
                warn!(error = %e, delay_secs = delay.as_secs(), "a2a relay spoke disconnected, retrying");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(MAX_DELAY);
            } else {
                delay = Duration::from_secs(1);
            }
        }
    });
}

async fn run_spoke_once(state: AppState, relay: A2aRelayRuntime) -> Result<()> {
    let node_id = relay
        .node_id
        .as_deref()
        .ok_or_else(|| anyhow!("a2a relay spoke node_id is required"))?;
    let token = relay
        .token
        .as_deref()
        .ok_or_else(|| anyhow!("a2a relay spoke token is required"))?;
    let hub_url = relay
        .hub_urls
        .first()
        .ok_or_else(|| anyhow!("a2a relay spoke hub_url is required"))?;
    let sep = if hub_url.contains('?') { '&' } else { '?' };
    let url = format!(
        "{hub_url}{sep}node_id={}&token={}",
        urlencoding::encode(node_id),
        urlencoding::encode(token)
    );
    let (stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .with_context(|| format!("connect relay hub {hub_url}"))?;
    info!(node = %node_id, hub = %hub_url, "a2a relay spoke connected");

    let (mut write, mut read) = stream.split();
    let (spoke_tx, mut spoke_rx) = mpsc::unbounded_channel::<RelayFrame>();
    let writer = tokio::spawn(async move {
        while let Some(frame) = spoke_rx.recv().await {
            if let Err(e) = send_spoke_frame(&mut write, &frame).await {
                warn!(error = %e, "spoke write error");
                break;
            }
        }
    });

    spoke_tx
        .send(spoke_hello(&state, node_id))
        .map_err(|_| anyhow!("spoke writer closed"))?;
    spoke_tx
        .send(spoke_route_lease(&state, node_id, 1))
        .map_err(|_| anyhow!("spoke writer closed"))?;

    while let Some(msg) = read.next().await {
        let msg = msg?;
        let tokio_tungstenite::tungstenite::Message::Text(text) = msg else {
            continue;
        };
        let frame: RelayFrame = serde_json::from_str(&text)?;
        match frame {
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

    writer.abort();
    Ok(())
}

fn spoke_hello(state: &AppState, node_id: &str) -> RelayFrame {
    RelayFrame::Hello {
        protocol: RELAY_PROTOCOL.to_owned(),
        node_id: node_id.to_owned(),
        node_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        agent_card: Some(crate::a2a::server::build_agent_card(state, false)),
        capabilities: Some(HelloCapabilities {
            streaming_relay: true,
        }),
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
}
