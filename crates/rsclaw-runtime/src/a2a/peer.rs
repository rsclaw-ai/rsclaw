//! WebRTC ICE/DTLS/SCTP peer transport for the A2A relay overlay.
//!
//! The relay hub carries authenticated SDP signaling only. A2A relay frames
//! travel on a reliable, ordered WebRTC data channel after ICE selects a host,
//! server-reflexive (STUN), or relay (TURN) candidate pair.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow};
use bytes::BytesMut;
use dashmap::DashMap;
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tracing::{debug, info, warn};
use webrtc::{
    data_channel::{DataChannel, DataChannelEvent},
    peer_connection::{
        PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
        RTCIceGatheringState, RTCIceServer, RTCPeerConnectionState, RTCSessionDescription,
    },
};

use crate::{
    a2a::{
        relay::{RelayFrame, RouteMode, StreamPending},
        relay_identity::{sign_payload, signing_key_from_b64, verify_payload},
        types::{JsonRpcRequest, JsonRpcResponse},
    },
    server::AppState,
};

const DATA_CHANNEL_LABEL: &str = "rsclaw.a2a.peer.v1";
const SIGNALING_PROTOCOL: &str = "rsclaw.a2a.webrtc.v1";
const MAX_SIGNAL_SDP_BYTES: usize = 64 * 1024;
const MAX_DATA_MESSAGE_BYTES: usize = 16 * 1024;
const DATA_CHUNK_HEADER_BYTES: usize = 17;
const DATA_CHUNK_BYTES: usize = MAX_DATA_MESSAGE_BYTES - DATA_CHUNK_HEADER_BYTES;
const MAX_RELAY_FRAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_CHUNKS: usize = MAX_RELAY_FRAME_BYTES.div_ceil(DATA_CHUNK_BYTES);
const DATA_QUEUE_CAPACITY: usize = 128;
const PEER_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const PEER_STREAM_MAX_LIFETIME: Duration = Duration::from_secs(1800);
const ICE_GATHER_TIMEOUT: Duration = Duration::from_secs(15);
const REASSEMBLY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_INFLIGHT_REASSEMBLIES: usize = 64;
const MAX_REASSEMBLY_BYTES: usize = 16 * 1024 * 1024;
const PEER_ROUTE_TTL: Duration = Duration::from_secs(300);

/// Failure classification for a direct unary invocation.
#[derive(Debug, thiserror::Error)]
pub enum PeerInvokeError {
    /// The frame was not queued; hub fallback is safe.
    #[error("peer direct transport unavailable: {0}")]
    Unavailable(String),
    /// The frame was queued and may have executed; automatic retry is unsafe.
    #[error("peer direct delivery outcome unknown: {0}")]
    DeliveryUnknown(String),
}

#[derive(Clone)]
struct PeerConnectionEntry {
    tx: mpsc::Sender<RelayFrame>,
    generation: u64,
    session_id: String,
}

struct PeerSession {
    peer_node_id: String,
    connection: Arc<dyn PeerConnection>,
}

struct Reassembly {
    chunks: Vec<Option<Vec<u8>>>,
    received: usize,
    bytes: usize,
    started_at: Instant,
}

impl Reassembly {
    fn new(chunk_count: usize) -> Self {
        Self {
            chunks: vec![None; chunk_count],
            received: 0,
            bytes: 0,
            started_at: Instant::now(),
        }
    }

    fn insert(&mut self, index: usize, bytes: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
        if self.chunks[index].is_some() {
            return Ok(None);
        }
        self.bytes = self.bytes.saturating_add(bytes.len());
        if self.bytes > MAX_RELAY_FRAME_BYTES {
            anyhow::bail!("reassembled peer frame exceeds {MAX_RELAY_FRAME_BYTES} bytes");
        }
        self.chunks[index] = Some(bytes.to_vec());
        self.received += 1;
        if self.received != self.chunks.len() {
            return Ok(None);
        }

        let mut frame = Vec::with_capacity(self.bytes);
        for chunk in &mut self.chunks {
            let bytes = chunk
                .take()
                .ok_or_else(|| anyhow!("peer frame reassembly missing chunk"))?;
            frame.extend_from_slice(&bytes);
        }
        Ok(Some(frame))
    }
}

/// Tracks direct WebRTC connections and routes for one gateway.
pub struct PeerManager {
    direct_connections: DashMap<String, PeerConnectionEntry>,
    sessions: DashMap<String, PeerSession>,
    routes: DashMap<String, super::relay::RouteEntry>,
    pending: DashMap<String, (oneshot::Sender<JsonRpcResponse>, String)>,
    stream_pending: DashMap<String, StreamPending>,
    task_routes: DashMap<String, String>,
    pub(crate) peer_stream_tasks: DashMap<String, String>,
    request_counter: AtomicU64,
    generation_counter: AtomicU64,
    pub metrics: super::relay::RelayMetrics,
}

impl Default for PeerManager {
    fn default() -> Self {
        Self {
            direct_connections: DashMap::new(),
            sessions: DashMap::new(),
            routes: DashMap::new(),
            pending: DashMap::new(),
            stream_pending: DashMap::new(),
            task_routes: DashMap::new(),
            peer_stream_tasks: DashMap::new(),
            request_counter: AtomicU64::new(0),
            generation_counter: AtomicU64::new(1),
            metrics: super::relay::RelayMetrics::default(),
        }
    }
}

impl PeerManager {
    fn store_session(
        &self,
        session_id: String,
        peer_node_id: String,
        connection: Arc<dyn PeerConnection>,
    ) -> anyhow::Result<()> {
        if self.sessions.contains_key(&session_id) {
            anyhow::bail!("duplicate peer signaling session '{session_id}'");
        }
        self.sessions.insert(
            session_id,
            PeerSession {
                peer_node_id,
                connection,
            },
        );
        Ok(())
    }

    fn remove_session_if_owned(&self, session_id: &str, peer_node_id: &str) {
        let owned = self
            .sessions
            .get(session_id)
            .is_some_and(|session| session.peer_node_id == peer_node_id);
        if owned {
            self.sessions.remove(session_id);
        }
    }

    /// Register a bounded direct-frame queue and return its ownership
    /// generation.
    pub fn register_connection(
        &self,
        peer_node_id: &str,
        session_id: &str,
        tx: mpsc::Sender<RelayFrame>,
    ) -> u64 {
        let generation = self.generation_counter.fetch_add(1, Ordering::Relaxed);
        self.direct_connections.insert(
            peer_node_id.to_owned(),
            PeerConnectionEntry {
                tx,
                generation,
                session_id: session_id.to_owned(),
            },
        );
        info!(peer = %peer_node_id, session = %session_id, generation, "peer WebRTC data channel registered");
        generation
    }

    /// Remove a direct connection only if the caller still owns its generation.
    pub fn unregister_connection(&self, peer_node_id: &str, generation: u64) {
        let Some(current) = self.direct_connections.get(peer_node_id) else {
            return;
        };
        if current.generation != generation {
            return;
        }
        let session_id = current.session_id.clone();
        drop(current);
        self.direct_connections.remove(peer_node_id);
        self.remove_session_if_owned(&session_id, peer_node_id);

        let stale_routes: Vec<String> = self
            .routes
            .iter()
            .filter(|entry| entry.value().node_id == peer_node_id)
            .map(|entry| entry.key().clone())
            .collect();
        for route in stale_routes {
            self.routes.remove(&route);
        }

        let pending_ids: Vec<String> = self
            .pending
            .iter()
            .filter(|entry| entry.value().1 == peer_node_id)
            .map(|entry| entry.key().clone())
            .collect();
        for request_id in pending_ids {
            if let Some((_, (tx, _))) = self.pending.remove(&request_id)
                && tx
                    .send(JsonRpcResponse::err(
                        Value::Null,
                        -32004,
                        format!("peer '{peer_node_id}' disconnected"),
                    ))
                    .is_err()
            {
                debug!(%request_id, "peer response waiter already dropped");
            }
        }

        let stream_ids: Vec<String> = self
            .stream_pending
            .iter()
            .filter(|entry| entry.value().node_id == peer_node_id)
            .map(|entry| entry.key().clone())
            .collect();
        for request_id in stream_ids {
            let synthetic = serde_json::json!({
                "kind": "status-update",
                "taskId": "",
                "contextId": "",
                "status": {
                    "state": "TASK_STATE_FAILED",
                    "message": {
                        "role": "ROLE_AGENT",
                        "messageId": format!("peer-loss-{}", uuid::Uuid::new_v4()),
                        "parts": [{
                            "type": "text",
                            "text": format!("peer direct connection lost: '{peer_node_id}' disconnected")
                        }]
                    }
                },
                "final": true
            });
            self.forward_stream_event(&request_id, synthetic);
            self.stream_pending.remove(&request_id);
            self.peer_stream_tasks.remove(&request_id);
            self.metrics.inflight_losses.fetch_add(1, Ordering::Relaxed);
        }

        info!(peer = %peer_node_id, %session_id, generation, "peer WebRTC data channel unregistered");
    }

    /// Return whether a usable direct data channel is registered for a peer.
    pub fn has_direct_connection(&self, peer_node_id: &str) -> bool {
        self.direct_connections.contains_key(peer_node_id)
    }

    /// Look up an unexpired direct route.
    pub fn route_for(&self, agent_ref: &str) -> Option<super::relay::RouteEntry> {
        let entry = self.routes.get(agent_ref)?;
        if entry.expires_at <= Instant::now() {
            drop(entry);
            self.routes.remove(agent_ref);
            return None;
        }
        Some(entry.clone())
    }

    /// Add a direct route advertised by its authenticated owning peer.
    pub fn add_route(&self, agent_ref: &str, node_id: &str) {
        self.routes.insert(
            agent_ref.to_owned(),
            super::relay::RouteEntry {
                agent_ref: agent_ref.to_owned(),
                node_id: node_id.to_owned(),
                epoch: 1,
                expires_at: Instant::now() + PEER_ROUTE_TTL,
                mode: RouteMode::Direct,
            },
        );
    }

    /// Record a task route learned from a response or stream event.
    pub fn record_task_route(&self, task_id: &str, agent_ref: &str) {
        self.task_routes
            .insert(task_id.to_owned(), agent_ref.to_owned());
    }

    /// Look up an agent route by task ID.
    pub fn route_for_task(&self, task_id: &str) -> Option<String> {
        self.task_routes.get(task_id).map(|entry| entry.clone())
    }

    async fn send(&self, peer_node_id: &str, frame: RelayFrame) -> anyhow::Result<()> {
        let tx = self
            .direct_connections
            .get(peer_node_id)
            .ok_or_else(|| anyhow!("no direct connection to '{peer_node_id}'"))?
            .tx
            .clone();
        tx.send(frame)
            .await
            .map_err(|_| anyhow!("peer send to '{peer_node_id}' failed"))
    }

    /// Invoke a synchronous JSON-RPC request over the direct data channel.
    pub async fn invoke_jsonrpc(
        &self,
        target: &str,
        method: &str,
        params: Value,
        principal: &str,
        peer_node_id: &str,
    ) -> Result<JsonRpcResponse, PeerInvokeError> {
        let request_id = format!(
            "peer:{}",
            self.request_counter.fetch_add(1, Ordering::Relaxed)
        );
        let (tx, rx) = oneshot::channel();
        self.pending
            .insert(request_id.clone(), (tx, peer_node_id.to_owned()));
        if let Err(error) = self
            .send(
                peer_node_id,
                RelayFrame::Request {
                    request_id: request_id.clone(),
                    target: target.to_owned(),
                    method: method.to_owned(),
                    params,
                    principal: principal.to_owned(),
                    deadline_ms: PEER_REQUEST_TIMEOUT.as_millis() as u64,
                },
            )
            .await
        {
            self.pending.remove(&request_id);
            return Err(PeerInvokeError::Unavailable(error.to_string()));
        }

        self.metrics.request_count.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let result = match tokio::time::timeout(PEER_REQUEST_TIMEOUT, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err(PeerInvokeError::DeliveryUnknown(
                "response channel closed".to_owned(),
            )),
            Err(_) => {
                self.pending.remove(&request_id);
                Err(PeerInvokeError::DeliveryUnknown(
                    "request timed out".to_owned(),
                ))
            }
        };
        self.metrics.request_latency_ms_total.fetch_add(
            started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        result
    }

    /// Complete a pending synchronous request.
    pub fn complete_pending(&self, request_id: &str, response: JsonRpcResponse) {
        if let Some((_, (tx, _))) = self.pending.remove(request_id)
            && tx.send(response).is_err()
        {
            debug!(%request_id, "peer response waiter already dropped");
        }
    }

    /// Invoke a streaming request over the direct data channel.
    pub async fn invoke_streaming(
        &self,
        target: &str,
        method: &str,
        params: Value,
        principal: &str,
        peer_node_id: &str,
    ) -> anyhow::Result<(String, String, broadcast::Receiver<Value>)> {
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
                deadline: Instant::now() + PEER_STREAM_MAX_LIFETIME,
            },
        );
        if let Err(error) = self
            .send(
                peer_node_id,
                RelayFrame::Request {
                    request_id: request_id.clone(),
                    target: target.to_owned(),
                    method: method.to_owned(),
                    params,
                    principal: principal.to_owned(),
                    deadline_ms: PEER_REQUEST_TIMEOUT.as_millis() as u64,
                },
            )
            .await
        {
            self.stream_pending.remove(&request_id);
            return Err(error);
        }
        Ok((request_id, peer_node_id.to_owned(), event_rx))
    }

    /// Forward one event to its streaming subscriber.
    pub fn forward_stream_event(&self, request_id: &str, value: Value) -> usize {
        let Some(entry) = self.stream_pending.get(request_id) else {
            return 0;
        };
        if let Some(task_id) = value.get("taskId").and_then(Value::as_str) {
            self.record_task_route(task_id, &entry.agent_ref);
        }
        entry.tx.send(value).unwrap_or(0)
    }

    /// Remove a streaming entry.
    pub fn complete_streaming(&self, request_id: &str) -> bool {
        self.stream_pending.remove(request_id).is_some()
    }

    /// Send cancellation for a direct streaming request.
    pub fn send_cancel_to(&self, peer_node_id: &str, request_id: &str) {
        if let Some(connection) = self.direct_connections.get(peer_node_id)
            && let Err(error) = connection.tx.try_send(RelayFrame::Cancel {
                request_id: request_id.to_owned(),
                task_id: None,
            })
        {
            warn!(peer = %peer_node_id, %error, "unable to queue peer cancellation");
        }
    }

    /// Expire direct streaming requests that exceeded their lifetime cap.
    pub fn sweep_expired_streams(&self) -> usize {
        let expired: Vec<(String, String)> = self
            .stream_pending
            .iter()
            .filter(|entry| entry.value().deadline <= Instant::now())
            .map(|entry| (entry.key().clone(), entry.value().node_id.clone()))
            .collect();
        for (request_id, node_id) in &expired {
            let synthetic = serde_json::json!({
                "kind": "status-update",
                "taskId": "",
                "contextId": "",
                "status": {
                    "state": "TASK_STATE_FAILED",
                    "message": {
                        "role": "ROLE_AGENT",
                        "messageId": format!("peer-deadline-{}", uuid::Uuid::new_v4()),
                        "parts": [{
                            "type": "text",
                            "text": "peer stream exceeded lifetime cap"
                        }]
                    }
                },
                "final": true
            });
            self.forward_stream_event(request_id, synthetic);
            self.stream_pending.remove(request_id);
            self.send_cancel_to(node_id, request_id);
            self.metrics.inflight_losses.fetch_add(1, Ordering::Relaxed);
        }
        expired.len()
    }
}

/// Cancels a direct peer stream when its SSE consumer is dropped.
pub struct PeerStreamGuard {
    peer_manager: Arc<PeerManager>,
    peer_node_id: String,
    request_id: String,
}

impl PeerStreamGuard {
    /// Create a direct stream lifecycle guard.
    pub fn new(peer_manager: Arc<PeerManager>, peer_node_id: String, request_id: String) -> Self {
        Self {
            peer_manager,
            peer_node_id,
            request_id,
        }
    }
}

impl Drop for PeerStreamGuard {
    fn drop(&mut self) {
        if self.peer_manager.complete_streaming(&self.request_id) {
            self.peer_manager
                .send_cancel_to(&self.peer_node_id, &self.request_id);
        }
    }
}

fn local_node_id(state: &AppState) -> anyhow::Result<String> {
    state
        .config
        .gateway
        .a2a_relay
        .node_id
        .clone()
        .filter(|node_id| !node_id.is_empty())
        .ok_or_else(|| anyhow!("peer WebRTC requires gateway.a2a.relay.nodeId"))
}

fn configured_peer<'a>(
    state: &'a AppState,
    peer_node_id: &str,
) -> anyhow::Result<&'a rsclaw_config::schema::A2aPeerConfig> {
    let local_node_id = local_node_id(state)?;
    if peer_node_id == local_node_id {
        anyhow::bail!("self peer is not permitted");
    }
    if state
        .config
        .gateway
        .a2a_relay
        .revoked_nodes
        .iter()
        .any(|node_id| node_id == peer_node_id)
    {
        anyhow::bail!("peer '{peer_node_id}' is revoked");
    }
    state
        .config
        .agents
        .a2a
        .iter()
        .find(|peer| {
            peer.node_id.as_deref() == Some(peer_node_id)
                && peer.mode.as_deref().is_none_or(|mode| mode == "peer")
        })
        .ok_or_else(|| anyhow!("unknown peer '{peer_node_id}'"))
}

fn signaling_payload(
    session_id: &str,
    source_node: &str,
    target_node: &str,
    kind: &str,
    sdp: &str,
) -> Vec<u8> {
    format!("{SIGNALING_PROTOCOL}\n{session_id}\n{source_node}\n{target_node}\n{kind}\n{sdp}")
        .into_bytes()
}

fn sign_sdp(
    state: &AppState,
    session_id: &str,
    target_node: &str,
    kind: &str,
    sdp: &str,
) -> anyhow::Result<Option<String>> {
    let source_node = local_node_id(state)?;
    let Some(private_key) = state.config.gateway.a2a_relay.private_key.as_deref() else {
        return Ok(None);
    };
    let signing_key =
        signing_key_from_b64(private_key).context("invalid peer signaling private key")?;
    Ok(Some(sign_payload(
        &signing_key,
        &signaling_payload(session_id, &source_node, target_node, kind, sdp),
    )))
}

fn verify_sdp(
    state: &AppState,
    session_id: &str,
    source_node: &str,
    kind: &str,
    sdp: &str,
    signature: Option<&str>,
) -> anyhow::Result<()> {
    let target_node = local_node_id(state)?;
    let peer = configured_peer(state, source_node)?;
    if let Some(public_key) = peer.public_key.as_deref() {
        let signature = signature.ok_or_else(|| anyhow!("missing signed peer {kind}"))?;
        verify_payload(
            public_key,
            &signaling_payload(session_id, source_node, &target_node, kind, sdp),
            signature,
        )
        .context("peer signaling signature rejected")?;
    }
    Ok(())
}

struct SessionHandler {
    state: AppState,
    peer_node_id: String,
    session_id: String,
    gathered_tx: watch::Sender<bool>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for SessionHandler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete && self.gathered_tx.send(true).is_err() {
            debug!(peer = %self.peer_node_id, session = %self.session_id, "ICE gather waiter dropped");
        }
    }

    async fn on_connection_state_change(&self, connection_state: RTCPeerConnectionState) {
        debug!(peer = %self.peer_node_id, session = %self.session_id, ?connection_state, "peer WebRTC state changed");
        if matches!(
            connection_state,
            RTCPeerConnectionState::Failed
                | RTCPeerConnectionState::Disconnected
                | RTCPeerConnectionState::Closed
        ) {
            self.state
                .peer_manager
                .remove_session_if_owned(&self.session_id, &self.peer_node_id);
            if let Some(connection) = self
                .state
                .peer_manager
                .direct_connections
                .get(&self.peer_node_id)
                .filter(|entry| entry.session_id == self.session_id)
            {
                let generation = connection.generation;
                drop(connection);
                self.state
                    .peer_manager
                    .unregister_connection(&self.peer_node_id, generation);
            }
        }
    }

    async fn on_data_channel(&self, data_channel: Arc<dyn DataChannel>) {
        let state = self.state.clone();
        let peer_node_id = self.peer_node_id.clone();
        let session_id = self.session_id.clone();
        tokio::spawn(async move {
            run_data_channel(state, peer_node_id, session_id, data_channel).await;
        });
    }
}

async fn build_connection(
    state: &AppState,
    peer_node_id: &str,
    session_id: &str,
) -> anyhow::Result<(Arc<dyn PeerConnection>, watch::Receiver<bool>)> {
    configured_peer(state, peer_node_id)?;
    let peer_config = state
        .config
        .gateway
        .a2a_relay
        .peer
        .as_ref()
        .filter(|peer| peer.enabled)
        .ok_or_else(|| anyhow!("peer WebRTC is disabled"))?;

    let mut ice_servers: Vec<RTCIceServer> = peer_config
        .stun_urls
        .iter()
        .map(|url| RTCIceServer {
            urls: vec![url.clone()],
            ..Default::default()
        })
        .collect();
    if !peer_config.turn_urls.is_empty() {
        ice_servers.push(RTCIceServer {
            urls: peer_config.turn_urls.clone(),
            username: peer_config.turn_username.clone().unwrap_or_default(),
            credential: peer_config.turn_credential.clone().unwrap_or_default(),
        });
    }

    let (gathered_tx, gathered_rx) = watch::channel(false);
    let handler = SessionHandler {
        state: state.clone(),
        peer_node_id: peer_node_id.to_owned(),
        session_id: session_id.to_owned(),
        gathered_tx,
    };
    let configuration = RTCConfigurationBuilder::default()
        .with_ice_servers(ice_servers)
        .build();
    let bind_address = format!("0.0.0.0:{}", peer_config.listen_port);
    let connection = PeerConnectionBuilder::new()
        .with_configuration(configuration)
        .with_handler(Arc::new(handler))
        .with_udp_addrs(vec![bind_address])
        .with_data_channel_send_buffer_limit(MAX_RELAY_FRAME_BYTES)
        .build()
        .await
        .context("create WebRTC peer connection")?;
    Ok((Arc::new(connection), gathered_rx))
}

async fn gathered_sdp(
    connection: &Arc<dyn PeerConnection>,
    mut gathered_rx: watch::Receiver<bool>,
) -> anyhow::Result<String> {
    if !*gathered_rx.borrow() {
        tokio::time::timeout(ICE_GATHER_TIMEOUT, async {
            while !*gathered_rx.borrow() {
                gathered_rx
                    .changed()
                    .await
                    .map_err(|_| anyhow!("ICE gatherer stopped"))?;
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .map_err(|_| anyhow!("ICE candidate gathering timed out"))??;
    }
    let local = connection
        .local_description()
        .await
        .ok_or_else(|| anyhow!("missing local SDP"))?;
    if local.sdp.len() > MAX_SIGNAL_SDP_BYTES {
        anyhow::bail!("SDP exceeds {MAX_SIGNAL_SDP_BYTES}-byte signaling limit");
    }
    Ok(local.sdp)
}

/// Create a WebRTC offer containing host/STUN/TURN candidates.
pub(crate) async fn create_peer_offer(
    state: &AppState,
    target_node: &str,
) -> anyhow::Result<RelayFrame> {
    let source_node = local_node_id(state)?;
    configured_peer(state, target_node)?;
    if state.peer_manager.has_direct_connection(target_node) {
        anyhow::bail!("peer '{target_node}' is already directly connected");
    }

    let session_id = format!("{source_node}-{}", uuid::Uuid::new_v4());
    let (connection, gathered_rx) = build_connection(state, target_node, &session_id).await?;
    let data_channel = connection
        .create_data_channel(DATA_CHANNEL_LABEL, None)
        .await
        .context("create peer data channel")?;
    state.peer_manager.store_session(
        session_id.clone(),
        target_node.to_owned(),
        connection.clone(),
    )?;
    tokio::spawn(run_data_channel(
        state.clone(),
        target_node.to_owned(),
        session_id.clone(),
        data_channel,
    ));

    let negotiation = async {
        let offer = connection.create_offer(None).await?;
        connection.set_local_description(offer).await?;
        let sdp = gathered_sdp(&connection, gathered_rx).await?;
        let signature = sign_sdp(state, &session_id, target_node, "offer", &sdp)?;
        Ok::<_, anyhow::Error>((sdp, signature))
    }
    .await;
    let (sdp, signature) = match negotiation {
        Ok(result) => result,
        Err(error) => {
            state
                .peer_manager
                .remove_session_if_owned(&session_id, target_node);
            if let Err(close_error) = connection.close().await {
                warn!(%close_error, "failed to close incomplete peer offer");
            }
            return Err(error);
        }
    };
    Ok(RelayFrame::PeerOffer {
        session_id,
        target_node: target_node.to_owned(),
        sdp,
        signature,
    })
}

/// Apply an authenticated offer and return its WebRTC answer.
pub(crate) async fn apply_peer_offer(
    state: &AppState,
    source_node: &str,
    session_id: &str,
    sdp: &str,
    signature: Option<&str>,
) -> anyhow::Result<RelayFrame> {
    validate_signal(session_id, sdp)?;
    verify_sdp(state, session_id, source_node, "offer", sdp, signature)?;
    let (connection, gathered_rx) = build_connection(state, source_node, session_id).await?;
    state.peer_manager.store_session(
        session_id.to_owned(),
        source_node.to_owned(),
        connection.clone(),
    )?;

    let result = async {
        connection
            .set_remote_description(RTCSessionDescription::offer(sdp.to_owned())?)
            .await?;
        let answer = connection.create_answer(None).await?;
        connection.set_local_description(answer).await?;
        let answer = gathered_sdp(&connection, gathered_rx).await?;
        let signature = sign_sdp(state, session_id, source_node, "answer", &answer)?;
        Ok::<_, anyhow::Error>((answer, signature))
    }
    .await;
    let (answer, signature) = match result {
        Ok(answer) => answer,
        Err(error) => {
            state
                .peer_manager
                .remove_session_if_owned(session_id, source_node);
            if let Err(close_error) = connection.close().await {
                warn!(%close_error, "failed to close rejected peer offer");
            }
            return Err(error);
        }
    };
    Ok(RelayFrame::PeerAnswer {
        session_id: session_id.to_owned(),
        target_node: source_node.to_owned(),
        sdp: answer,
        signature,
    })
}

/// Verify and apply an answer to an existing WebRTC offer session.
pub(crate) async fn apply_peer_answer(
    state: &AppState,
    source_node: &str,
    session_id: &str,
    sdp: &str,
    signature: Option<&str>,
) -> anyhow::Result<()> {
    validate_signal(session_id, sdp)?;
    verify_sdp(state, session_id, source_node, "answer", sdp, signature)?;
    let session = state
        .peer_manager
        .sessions
        .get(session_id)
        .ok_or_else(|| anyhow!("unknown peer session '{session_id}'"))?;
    if session.peer_node_id != source_node {
        anyhow::bail!("peer answer source does not own session '{session_id}'");
    }
    let connection = session.connection.clone();
    drop(session);
    connection
        .set_remote_description(RTCSessionDescription::answer(sdp.to_owned())?)
        .await
        .context("apply peer SDP answer")
}

fn validate_signal(session_id: &str, sdp: &str) -> anyhow::Result<()> {
    if session_id.is_empty() || session_id.len() > 128 {
        anyhow::bail!("invalid peer signaling session ID");
    }
    if sdp.is_empty() || sdp.len() > MAX_SIGNAL_SDP_BYTES {
        anyhow::bail!("invalid peer SDP size");
    }
    Ok(())
}

fn encode_chunks(message_id: u64, frame: &RelayFrame) -> anyhow::Result<Vec<Vec<u8>>> {
    let bytes = serde_json::to_vec(frame).context("serialize peer relay frame")?;
    if bytes.len() > MAX_RELAY_FRAME_BYTES {
        anyhow::bail!("peer relay frame exceeds {MAX_RELAY_FRAME_BYTES} bytes");
    }
    let chunk_count = bytes.len().max(1).div_ceil(DATA_CHUNK_BYTES);
    if chunk_count > MAX_CHUNKS {
        anyhow::bail!("peer relay frame has too many chunks");
    }
    let chunk_count_u32 = u32::try_from(chunk_count).context("peer chunk count overflow")?;
    let mut messages = Vec::with_capacity(chunk_count);
    for (index, payload) in bytes.chunks(DATA_CHUNK_BYTES).enumerate() {
        let index_u32 = u32::try_from(index).context("peer chunk index overflow")?;
        let mut message = Vec::with_capacity(DATA_CHUNK_HEADER_BYTES + payload.len());
        message.push(1);
        message.extend_from_slice(&message_id.to_be_bytes());
        message.extend_from_slice(&index_u32.to_be_bytes());
        message.extend_from_slice(&chunk_count_u32.to_be_bytes());
        message.extend_from_slice(payload);
        messages.push(message);
    }
    Ok(messages)
}

fn decode_chunk(bytes: &[u8]) -> anyhow::Result<(u64, usize, usize, &[u8])> {
    if bytes.len() < DATA_CHUNK_HEADER_BYTES || bytes.len() > MAX_DATA_MESSAGE_BYTES {
        anyhow::bail!("invalid peer data message size");
    }
    if bytes[0] != 1 {
        anyhow::bail!("unsupported peer data message version");
    }
    let message_id = u64::from_be_bytes(
        bytes[1..9]
            .try_into()
            .map_err(|_| anyhow!("invalid peer message ID"))?,
    );
    let index = u32::from_be_bytes(
        bytes[9..13]
            .try_into()
            .map_err(|_| anyhow!("invalid peer chunk index"))?,
    ) as usize;
    let chunk_count = u32::from_be_bytes(
        bytes[13..17]
            .try_into()
            .map_err(|_| anyhow!("invalid peer chunk count"))?,
    ) as usize;
    if chunk_count == 0 || chunk_count > MAX_CHUNKS || index >= chunk_count {
        anyhow::bail!("invalid peer chunk bounds");
    }
    Ok((message_id, index, chunk_count, &bytes[17..]))
}

async fn run_data_channel(
    state: AppState,
    peer_node_id: String,
    session_id: String,
    data_channel: Arc<dyn DataChannel>,
) {
    let label = match data_channel.label().await {
        Ok(label) => label,
        Err(error) => {
            warn!(peer = %peer_node_id, %error, "cannot read peer data channel label");
            return;
        }
    };
    if label != DATA_CHANNEL_LABEL {
        warn!(peer = %peer_node_id, %label, "unexpected peer data channel label");
        if let Err(error) = data_channel.close().await {
            warn!(peer = %peer_node_id, %error, "failed to close unexpected data channel");
        }
        return;
    }

    loop {
        match data_channel.poll().await {
            Some(DataChannelEvent::OnOpen) => break,
            Some(DataChannelEvent::OnClose | DataChannelEvent::OnClosing) | None => return,
            Some(DataChannelEvent::OnError) => {
                warn!(peer = %peer_node_id, "peer data channel failed before open");
                return;
            }
            Some(_) => {}
        }
    }
    match data_channel.ordered().await {
        Ok(true) => {}
        Ok(false) => {
            warn!(peer = %peer_node_id, "unordered peer data channel rejected");
            if let Err(error) = data_channel.close().await {
                warn!(peer = %peer_node_id, %error, "failed to close unordered data channel");
            }
            return;
        }
        Err(error) => {
            warn!(peer = %peer_node_id, %error, "cannot inspect peer data channel ordering");
            return;
        }
    }

    let (tx, mut rx) = mpsc::channel(DATA_QUEUE_CAPACITY);
    let generation = state
        .peer_manager
        .register_connection(&peer_node_id, &session_id, tx.clone());
    let local_node = match local_node_id(&state) {
        Ok(node_id) => node_id,
        Err(error) => {
            warn!(%error, "cannot advertise routes over peer data channel");
            state
                .peer_manager
                .unregister_connection(&peer_node_id, generation);
            return;
        }
    };
    let agents = state
        .agents
        .all()
        .into_iter()
        .map(|agent| format!("{local_node}/{}", agent.id))
        .collect();
    if let Err(error) = tx
        .send(RelayFrame::RouteLease {
            node_id: local_node.clone(),
            agents,
            ttl_ms: PEER_ROUTE_TTL.as_millis() as u64,
            epoch: 1,
        })
        .await
    {
        warn!(peer = %peer_node_id, %error, "cannot queue peer route lease");
    }

    let mut message_counter = 0u64;
    let mut reassembly: HashMap<u64, Reassembly> = HashMap::new();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    heartbeat.tick().await;

    loop {
        tokio::select! {
            event = data_channel.poll() => {
                match event {
                    Some(DataChannelEvent::OnMessage(message)) => {
                        match decode_chunk(&message.data) {
                            Ok((message_id, index, chunk_count, payload)) => {
                                reassembly.retain(|_, partial| partial.started_at.elapsed() < REASSEMBLY_TIMEOUT);
                                let aggregate_bytes: usize = reassembly
                                    .values()
                                    .map(|partial| partial.bytes)
                                    .sum();
                                if !reassembly.contains_key(&message_id)
                                    && reassembly.len() >= MAX_INFLIGHT_REASSEMBLIES
                                {
                                    warn!(peer = %peer_node_id, "too many incomplete peer frames");
                                    break;
                                }
                                if aggregate_bytes.saturating_add(payload.len()) > MAX_REASSEMBLY_BYTES {
                                    warn!(peer = %peer_node_id, "peer reassembly byte budget exceeded");
                                    break;
                                }
                                let partial = reassembly
                                    .entry(message_id)
                                    .or_insert_with(|| Reassembly::new(chunk_count));
                                if partial.chunks.len() != chunk_count {
                                    warn!(peer = %peer_node_id, message_id, "peer chunk count changed mid-frame");
                                    reassembly.remove(&message_id);
                                    continue;
                                }
                                match partial.insert(index, payload) {
                                    Ok(Some(frame_bytes)) => {
                                        reassembly.remove(&message_id);
                                        match serde_json::from_slice::<RelayFrame>(&frame_bytes) {
                                            Ok(frame) => handle_peer_frame(&state, &peer_node_id, frame),
                                            Err(error) => warn!(peer = %peer_node_id, %error, "invalid peer relay frame"),
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(error) => {
                                        warn!(peer = %peer_node_id, %error, "peer frame reassembly rejected");
                                        reassembly.remove(&message_id);
                                    }
                                }
                            }
                            Err(error) => warn!(peer = %peer_node_id, %error, "invalid peer data chunk"),
                        }
                    }
                    Some(DataChannelEvent::OnClose | DataChannelEvent::OnClosing) | None => break,
                    Some(DataChannelEvent::OnError) => {
                        warn!(peer = %peer_node_id, "peer data channel error");
                        break;
                    }
                    Some(_) => {}
                }
            }
            frame = rx.recv() => {
                let Some(frame) = frame else { break; };
                message_counter = message_counter.wrapping_add(1);
                match encode_chunks(message_counter, &frame) {
                    Ok(messages) => {
                        let mut failed = false;
                        for message in messages {
                            if let Err(error) = data_channel.send(BytesMut::from(message.as_slice())).await {
                                warn!(peer = %peer_node_id, %error, "peer data send failed");
                                failed = true;
                                break;
                            }
                        }
                        if failed {
                            break;
                        }
                    }
                    Err(error) => warn!(peer = %peer_node_id, %error, "peer relay frame rejected before send"),
                }
            }
            _ = heartbeat.tick() => {
                reassembly.retain(|_, partial| partial.started_at.elapsed() < REASSEMBLY_TIMEOUT);
                // Refresh authenticated direct routes before their lease expires.
                let agents = state
                    .agents
                    .all()
                    .into_iter()
                    .map(|agent| format!("{local_node}/{}", agent.id))
                    .collect();
                if let Err(error) = tx.try_send(RelayFrame::RouteLease {
                    node_id: local_node.clone(),
                    agents,
                    ttl_ms: PEER_ROUTE_TTL.as_millis() as u64,
                    epoch: message_counter.saturating_add(2),
                }) {
                    warn!(peer = %peer_node_id, %error, "cannot renew peer route lease");
                }
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
                    .unwrap_or(0);
                if let Err(error) = tx.try_send(RelayFrame::Ping { ts: timestamp }) {
                    warn!(peer = %peer_node_id, %error, "cannot queue peer heartbeat");
                }
            }
        }
    }

    state
        .peer_manager
        .unregister_connection(&peer_node_id, generation);
    if let Err(error) = data_channel.close().await {
        debug!(peer = %peer_node_id, %error, "peer data channel close returned an error");
    }
}

/// Handle one inbound data-channel relay frame.
pub(crate) fn handle_peer_frame(state: &AppState, peer_node_id: &str, frame: RelayFrame) {
    match frame {
        RelayFrame::Request {
            request_id,
            target,
            method,
            params,
            principal: _,
            ..
        } => {
            let Some(spoke_tx) = state
                .peer_manager
                .direct_connections
                .get(peer_node_id)
                .map(|connection| connection.tx.clone())
            else {
                warn!(peer = %peer_node_id, "peer request arrived after disconnect");
                return;
            };
            let state = state.clone();
            let peer_node_id = peer_node_id.to_owned();
            tokio::spawn(async move {
                let response = handle_peer_spoke_request(
                    &state,
                    &request_id,
                    &target,
                    &method,
                    params,
                    &peer_node_id,
                    spoke_tx.clone(),
                )
                .await;
                if let Some(response) = response
                    && let Err(error) = spoke_tx
                        .send(RelayFrame::Response {
                            request_id,
                            response,
                        })
                        .await
                {
                    warn!(peer = %peer_node_id, %error, "cannot queue peer response");
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
                debug!(%request_id, "peer event for unknown stream");
            }
        }
        RelayFrame::Cancel { request_id, .. } => {
            if let Some((_, task_id)) = state.peer_manager.peer_stream_tasks.remove(&request_id)
                && let Some((_, token)) = state.task_cancels.remove(&task_id)
            {
                token.cancel();
            }
        }
        RelayFrame::Ping { ts } => {
            if let Some(tx) = state
                .peer_manager
                .direct_connections
                .get(peer_node_id)
                .map(|connection| connection.tx.clone())
            {
                tokio::spawn(async move {
                    if let Err(error) = tx.send(RelayFrame::Pong { ts }).await {
                        warn!(%error, "cannot queue peer pong");
                    }
                });
            }
        }
        RelayFrame::Pong { .. } => {}
        RelayFrame::RouteLease {
            node_id, agents, ..
        } => {
            let prefix = format!("{peer_node_id}/");
            if node_id != peer_node_id
                || agents.iter().any(|agent| {
                    super::relay::validate_agent_ref(agent).is_err() || !agent.starts_with(&prefix)
                })
            {
                state
                    .peer_manager
                    .metrics
                    .acl_denials
                    .fetch_add(1, Ordering::Relaxed);
                warn!(peer = %peer_node_id, "invalid peer route lease rejected");
                return;
            }
            for agent in agents {
                state.peer_manager.add_route(&agent, peer_node_id);
            }
        }
        other => {
            debug!(peer = %peer_node_id, frame = ?other, "control-only relay frame ignored on peer data channel");
        }
    }
}

async fn handle_peer_spoke_request(
    state: &AppState,
    request_id: &str,
    target: &str,
    method: &str,
    mut params: Value,
    peer_node_id: &str,
    spoke_tx: mpsc::Sender<RelayFrame>,
) -> Option<JsonRpcResponse> {
    let local_node = match local_node_id(state) {
        Ok(node_id) => node_id,
        Err(error) => {
            return Some(JsonRpcResponse::err(Value::Null, -32003, error.to_string()));
        }
    };
    let Some(local_agent) = super::relay::local_agent_from_ref(target, &local_node) else {
        return Some(JsonRpcResponse::err(
            Value::Null,
            -32003,
            format!("target not hosted here: {target}"),
        ));
    };
    if !super::relay::configured_peer_invoke_allowed(state, peer_node_id, target) {
        state
            .peer_manager
            .metrics
            .acl_denials
            .fetch_add(1, Ordering::Relaxed);
        return Some(JsonRpcResponse::err(
            Value::Null,
            -32003,
            format!("peer '{peer_node_id}' is not authorized to invoke {target}"),
        ));
    }
    if let Some(metadata) = params.get_mut("metadata").and_then(Value::as_object_mut) {
        metadata.insert("agentId".to_owned(), Value::String(local_agent));
    }

    if method == "SendStreamingMessage" || method == "SubscribeToTask" {
        let caller = Some(super::auth::A2aIdentity {
            id: format!("node:{peer_node_id}"),
            scopes: Vec::new(),
        });
        let (task_id, event_rx) = if method == "SubscribeToTask" {
            super::streaming::subscribe_to_task(state, &caller, &params)
        } else {
            super::streaming::spawn_streaming_task(state.clone(), caller, params).await
        };
        state
            .peer_manager
            .peer_stream_tasks
            .insert(request_id.to_owned(), task_id.clone());
        let peer_manager = state.peer_manager.clone();
        let request_id = request_id.to_owned();
        tokio::spawn(async move {
            let mut stream = tokio_stream::wrappers::BroadcastStream::new(event_rx);
            let mut seq = 0;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(event) => {
                        let final_event = event.is_final();
                        if spoke_tx
                            .send(RelayFrame::Event {
                                request_id: request_id.clone(),
                                seq,
                                result: event.to_wire_event(),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                        seq += 1;
                        if final_event {
                            break;
                        }
                    }
                    Err(error) => warn!(%error, "peer stream lagged"),
                }
            }
            peer_manager.peer_stream_tasks.remove(&request_id);
            if let Err(error) = spoke_tx
                .send(RelayFrame::Response {
                    request_id,
                    response: JsonRpcResponse::ok(
                        Value::String(task_id),
                        serde_json::json!({"ok": true}),
                    ),
                })
                .await
            {
                warn!(%error, "cannot queue peer stream response");
            }
        });
        return None;
    }

    let request = JsonRpcRequest {
        jsonrpc: "2.0".to_owned(),
        id: Value::String(format!("peer:{}", uuid::Uuid::new_v4())),
        method: method.to_owned(),
        params,
    };
    Some(
        super::server::a2a_rpc_handler_inner(
            state.clone(),
            Some(super::auth::A2aIdentity {
                id: format!("node:{peer_node_id}"),
                scopes: Vec::new(),
            }),
            request,
        )
        .await
        .0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_chunks_round_trip_large_payload() {
        let frame = RelayFrame::Request {
            request_id: "request-1".to_owned(),
            target: "node-b/main".to_owned(),
            method: "SendMessage".to_owned(),
            params: serde_json::json!({"blob": "x".repeat(40_000)}),
            principal: "node-a".to_owned(),
            deadline_ms: 1_000,
        };
        let messages = encode_chunks(7, &frame).expect("large frame should chunk");
        assert!(messages.len() > 1);
        assert!(
            messages
                .iter()
                .all(|message| message.len() <= MAX_DATA_MESSAGE_BYTES)
        );

        let mut partial = None;
        for message in messages {
            let (_, index, count, payload) = decode_chunk(&message).expect("chunk should decode");
            let reassembly = partial.get_or_insert_with(|| Reassembly::new(count));
            if let Some(bytes) = reassembly
                .insert(index, payload)
                .expect("chunk should insert")
            {
                let decoded: RelayFrame =
                    serde_json::from_slice(&bytes).expect("frame should deserialize");
                match decoded {
                    RelayFrame::Request { params, .. } => {
                        assert_eq!(params["blob"].as_str().map(str::len), Some(40_000));
                    }
                    other => panic!("expected request, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn invalid_chunk_bounds_are_rejected() {
        let mut message = vec![0u8; DATA_CHUNK_HEADER_BYTES];
        message[0] = 1;
        message[9..13].copy_from_slice(&1u32.to_be_bytes());
        message[13..17].copy_from_slice(&1u32.to_be_bytes());
        assert!(decode_chunk(&message).is_err());
    }

    #[test]
    fn reassembly_limits_bound_incomplete_frame_memory() {
        assert!(MAX_INFLIGHT_REASSEMBLIES > 0);
        assert!(MAX_REASSEMBLY_BYTES >= MAX_RELAY_FRAME_BYTES);
        assert!(MAX_REASSEMBLY_BYTES < MAX_RELAY_FRAME_BYTES * MAX_INFLIGHT_REASSEMBLIES);
    }

    #[tokio::test]
    async fn old_generation_cannot_remove_new_connection() {
        let manager = PeerManager::default();
        let (first_tx, _first_rx) = mpsc::channel(1);
        let first = manager.register_connection("node-b", "session-1", first_tx);
        let (second_tx, _second_rx) = mpsc::channel(1);
        let second = manager.register_connection("node-b", "session-2", second_tx);

        manager.unregister_connection("node-b", first);
        assert!(manager.has_direct_connection("node-b"));
        manager.unregister_connection("node-b", second);
        assert!(!manager.has_direct_connection("node-b"));
    }
}
