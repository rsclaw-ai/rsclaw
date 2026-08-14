//! WebRTC ICE/DTLS/SCTP peer transport for the A2A relay overlay.
//!
//! The relay hub carries authenticated SDP signaling only. A2A relay frames
//! travel on a reliable, ordered WebRTC data channel after ICE selects a host,
//! server-reflexive (STUN), or relay (TURN) candidate pair.

use std::{
    collections::{HashMap, hash_map::Entry},
    sync::{
        Arc, Mutex,
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
const MAX_PEER_STREAMS: usize = 1024;
const MAX_PEER_PENDING_REQUESTS: usize = 4096;
const ICE_GATHER_TIMEOUT: Duration = Duration::from_secs(15);
const PEER_SIGNAL_SESSION_TTL: Duration = Duration::from_secs(60);
const MAX_PEER_SIGNAL_SESSIONS: usize = 1024;
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
    session_generation: Option<u64>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

struct PeerSession {
    peer_node_id: String,
    generation: u64,
    connection: Option<Arc<dyn PeerConnection>>,
    created_at: Instant,
    state: PeerSessionState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PeerSessionState {
    Reserved,
    Pending,
    ApplyingAnswer,
    AnswerApplied,
    Established,
}

struct PeerStreamTask {
    task_id: String,
    connection_generation: u64,
    cancel_owner: Option<Arc<tokio_util::sync::CancellationToken>>,
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
    sessions: Mutex<HashMap<String, PeerSession>>,
    consumed_sessions: Mutex<HashMap<String, Instant>>,
    routes: DashMap<String, super::relay::RouteEntry>,
    pending: DashMap<String, (oneshot::Sender<JsonRpcResponse>, String)>,
    pending_admission: Mutex<()>,
    stream_pending: DashMap<String, StreamPending>,
    stream_admission: Mutex<()>,
    task_routes: DashMap<String, String>,
    peer_stream_tasks: DashMap<String, PeerStreamTask>,
    request_counter: AtomicU64,
    generation_counter: AtomicU64,
    pub metrics: super::relay::RelayMetrics,
}

impl Default for PeerManager {
    fn default() -> Self {
        Self {
            direct_connections: DashMap::new(),
            sessions: Mutex::new(HashMap::new()),
            consumed_sessions: Mutex::new(HashMap::new()),
            routes: DashMap::new(),
            pending: DashMap::new(),
            pending_admission: Mutex::new(()),
            stream_pending: DashMap::new(),
            stream_admission: Mutex::new(()),
            task_routes: DashMap::new(),
            peer_stream_tasks: DashMap::new(),
            request_counter: AtomicU64::new(0),
            generation_counter: AtomicU64::new(1),
            metrics: super::relay::RelayMetrics::default(),
        }
    }
}

impl PeerManager {
    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, PeerSession>> {
        self.sessions
            .lock()
            .expect("peer signaling session mutex poisoned")
    }

    fn reserve_session(&self, session_id: &str, peer_node_id: &str) -> anyhow::Result<u64> {
        self.sweep_expired_sessions();
        {
            let mut consumed = self
                .consumed_sessions
                .lock()
                .expect("consumed peer signaling session mutex poisoned");
            consumed.retain(|_, expires_at| *expires_at > Instant::now());
            if consumed.contains_key(session_id) {
                anyhow::bail!("replayed peer signaling session '{session_id}'");
            }
        }
        let generation = self.generation_counter.fetch_add(1, Ordering::Relaxed);
        let mut sessions = self.lock_sessions();
        if sessions.len() >= MAX_PEER_SIGNAL_SESSIONS {
            anyhow::bail!("peer signaling session capacity exceeded");
        }
        match sessions.entry(session_id.to_owned()) {
            Entry::Occupied(_) => anyhow::bail!("duplicate peer signaling session '{session_id}'"),
            Entry::Vacant(entry) => {
                entry.insert(PeerSession {
                    peer_node_id: peer_node_id.to_owned(),
                    generation,
                    connection: None,
                    created_at: Instant::now(),
                    state: PeerSessionState::Reserved,
                });
                Ok(generation)
            }
        }
    }

    fn attach_session(
        &self,
        session_id: &str,
        generation: u64,
        connection: Arc<dyn PeerConnection>,
    ) -> anyhow::Result<()> {
        let mut sessions = self.lock_sessions();
        let session = sessions
            .get_mut(session_id)
            .filter(|session| session.generation == generation)
            .ok_or_else(|| anyhow!("peer signaling reservation expired"))?;
        session.connection = Some(connection);
        session.state = PeerSessionState::Pending;
        Ok(())
    }

    fn mark_session_consumed(&self, session_id: &str) {
        let mut consumed = self
            .consumed_sessions
            .lock()
            .expect("consumed peer signaling session mutex poisoned");
        consumed.retain(|_, expires_at| *expires_at > Instant::now());
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
            session_id.to_owned(),
            Instant::now() + PEER_SIGNAL_SESSION_TTL,
        );
    }

    fn remove_session_if_generation(
        &self,
        session_id: &str,
        generation: u64,
    ) -> Option<PeerSession> {
        let mut sessions = self.lock_sessions();
        if sessions
            .get(session_id)
            .is_some_and(|session| session.generation == generation)
        {
            sessions.remove(session_id)
        } else {
            None
        }
    }

    pub(crate) fn sweep_expired_sessions(&self) -> usize {
        let now = Instant::now();
        let expired = {
            let mut sessions = self.lock_sessions();
            let expired_ids: Vec<String> = sessions
                .iter()
                .filter(|(_, session)| {
                    session.state != PeerSessionState::Established
                        && now.duration_since(session.created_at) >= PEER_SIGNAL_SESSION_TTL
                })
                .map(|(session_id, _)| session_id.clone())
                .collect();
            expired_ids
                .into_iter()
                .filter_map(|session_id| {
                    sessions
                        .remove(&session_id)
                        .map(|session| (session_id, session))
                })
                .collect::<Vec<_>>()
        };
        let count = expired.len();
        for (session_id, session) in expired {
            if let Some(connection) = session.connection {
                tokio::spawn(async move {
                    if let Err(error) = connection.close().await {
                        debug!(%session_id, %error, "failed to close expired peer signaling session");
                    }
                });
            }
        }
        count
    }

    pub(crate) async fn drop_session(&self, session_id: &str, reason: &str) {
        let session = self.lock_sessions().remove(session_id);
        let Some(connection) = session.and_then(|session| session.connection) else {
            return;
        };
        if let Err(error) = connection.close().await {
            debug!(%session_id, %reason, %error, "failed to close peer signaling session");
        }
    }

    pub(crate) async fn drop_pending_sessions(&self, reason: &str) {
        let pending = {
            let mut sessions = self.lock_sessions();
            let session_ids: Vec<String> = sessions
                .iter()
                .filter(|(_, session)| session.state != PeerSessionState::Established)
                .map(|(session_id, _)| session_id.clone())
                .collect();
            session_ids
                .into_iter()
                .filter_map(|session_id| {
                    sessions.remove(&session_id).and_then(|session| {
                        session
                            .connection
                            .map(|connection| (session_id, connection))
                    })
                })
                .collect::<Vec<_>>()
        };
        for (session_id, connection) in pending {
            if let Err(error) = connection.close().await {
                debug!(%session_id, %reason, %error, "failed to close pending peer signaling session");
            }
        }
    }

    fn remove_session_if_generation_owned(
        &self,
        session_id: &str,
        peer_node_id: &str,
        generation: u64,
    ) {
        let mut sessions = self.lock_sessions();
        if sessions.get(session_id).is_some_and(|session| {
            session.peer_node_id == peer_node_id && session.generation == generation
        }) {
            sessions.remove(session_id);
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
        self.register_connection_inner(peer_node_id, session_id, None, tx)
            .expect("unscoped peer connection registration cannot fail")
    }

    fn register_session_connection(
        &self,
        peer_node_id: &str,
        session_id: &str,
        session_generation: u64,
        tx: mpsc::Sender<RelayFrame>,
    ) -> anyhow::Result<u64> {
        self.register_connection_inner(peer_node_id, session_id, Some(session_generation), tx)
    }

    fn register_connection_inner(
        &self,
        peer_node_id: &str,
        session_id: &str,
        session_generation: Option<u64>,
        tx: mpsc::Sender<RelayFrame>,
    ) -> anyhow::Result<u64> {
        let mut sessions = self.lock_sessions();
        if let Some(session_generation) = session_generation {
            let session = sessions
                .get_mut(session_id)
                .filter(|session| {
                    session.peer_node_id == peer_node_id && session.generation == session_generation
                })
                .ok_or_else(|| anyhow!("peer signaling session no longer owns data channel"))?;
            session.state = PeerSessionState::Established;
        }
        let generation = self.generation_counter.fetch_add(1, Ordering::Relaxed);
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut superseded_connection = None;
        if let Some(previous) = self.direct_connections.insert(
            peer_node_id.to_owned(),
            PeerConnectionEntry {
                tx,
                generation,
                session_id: session_id.to_owned(),
                session_generation,
                cancelled: cancelled.clone(),
            },
        ) {
            previous.cancelled.store(true, Ordering::Release);
            if let Some(previous_session_generation) = previous.session_generation
                && (previous.session_id != session_id
                    || Some(previous_session_generation) != session_generation)
                && sessions.get(&previous.session_id).is_some_and(|session| {
                    session.peer_node_id == peer_node_id
                        && session.generation == previous_session_generation
                })
            {
                superseded_connection = sessions
                    .remove(&previous.session_id)
                    .and_then(|session| session.connection);
            }
        }
        drop(sessions);
        if let Some(connection) = superseded_connection {
            tokio::spawn(async move {
                if let Err(error) = connection.close().await {
                    debug!(%error, "failed to close superseded peer connection");
                }
            });
        }
        info!(peer = %peer_node_id, session = %session_id, generation, "peer WebRTC data channel registered");
        Ok(generation)
    }

    fn connection_is_current(&self, peer_node_id: &str, generation: u64) -> bool {
        self.direct_connections
            .get(peer_node_id)
            .is_some_and(|connection| {
                connection.generation == generation && !connection.cancelled.load(Ordering::Acquire)
            })
    }

    fn connection_cancelled(&self, peer_node_id: &str, generation: u64) -> bool {
        !self.connection_is_current(peer_node_id, generation)
    }

    /// Remove a direct connection only if the caller still owns its generation.
    pub fn unregister_connection(&self, peer_node_id: &str, generation: u64) {
        let Some((_, current)) = self
            .direct_connections
            .remove_if(peer_node_id, |_, connection| {
                connection.generation == generation
            })
        else {
            return;
        };
        let session_id = current.session_id;
        if let Some(session_generation) = current.session_generation {
            self.remove_session_if_generation_owned(&session_id, peer_node_id, session_generation);
        }

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
            self.metrics.inflight_losses.fetch_add(1, Ordering::Relaxed);
        }

        info!(peer = %peer_node_id, %session_id, generation, "peer WebRTC data channel unregistered");
    }

    fn take_stream_tasks(&self, connection_generation: u64) -> Vec<PeerStreamTask> {
        let request_ids: Vec<String> = self
            .peer_stream_tasks
            .iter()
            .filter(|entry| entry.value().connection_generation == connection_generation)
            .map(|entry| entry.key().clone())
            .collect();
        request_ids
            .into_iter()
            .filter_map(|request_id| {
                self.peer_stream_tasks
                    .remove_if(&request_id, |_, task| {
                        task.connection_generation == connection_generation
                    })
                    .map(|(_, task)| task)
            })
            .collect()
    }

    fn take_stream_task(
        &self,
        request_id: &str,
        connection_generation: u64,
    ) -> Option<PeerStreamTask> {
        self.peer_stream_tasks
            .remove_if(request_id, |_, task| {
                task.connection_generation == connection_generation
            })
            .map(|(_, task)| task)
    }

    fn cancel_stream_tasks(
        &self,
        task_cancels: &DashMap<String, Arc<tokio_util::sync::CancellationToken>>,
        connection_generation: u64,
    ) {
        for task in self.take_stream_tasks(connection_generation) {
            if let Some(owner) = task.cancel_owner
                && let Some(token) =
                    crate::server::remove_task_cancel_if_owner(task_cancels, &task.task_id, &owner)
            {
                token.cancel();
            }
        }
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

    fn send(&self, peer_node_id: &str, frame: RelayFrame) -> anyhow::Result<()> {
        let connection = self
            .direct_connections
            .get(peer_node_id)
            .ok_or_else(|| anyhow!("no direct connection to '{peer_node_id}'"))?;
        if connection.cancelled.load(Ordering::Acquire) {
            anyhow::bail!("direct connection to '{peer_node_id}' was replaced");
        }
        connection
            .tx
            .try_send(frame)
            .map_err(|error| anyhow!("peer send to '{peer_node_id}' failed: {error}"))
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
        {
            let _admission = self
                .pending_admission
                .lock()
                .expect("peer request admission mutex poisoned");
            if self.pending.len() >= MAX_PEER_PENDING_REQUESTS {
                return Err(PeerInvokeError::Unavailable(
                    "peer request capacity exceeded".to_owned(),
                ));
            }
            self.pending
                .insert(request_id.clone(), (tx, peer_node_id.to_owned()));
        }
        if let Err(error) = self.send(
            peer_node_id,
            RelayFrame::Request {
                request_id: request_id.clone(),
                target: target.to_owned(),
                method: method.to_owned(),
                params,
                principal: principal.to_owned(),
                deadline_ms: PEER_REQUEST_TIMEOUT.as_millis() as u64,
            },
        ) {
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
    ) -> Result<(String, String, broadcast::Receiver<Value>), PeerInvokeError> {
        let request_id = format!(
            "peer:stream:{}",
            self.request_counter.fetch_add(1, Ordering::Relaxed)
        );
        let (event_tx, event_rx) = broadcast::channel(128);
        {
            let _admission = self
                .stream_admission
                .lock()
                .expect("peer stream admission mutex poisoned");
            if self.stream_pending.len() >= MAX_PEER_STREAMS {
                return Err(PeerInvokeError::Unavailable(
                    "peer stream capacity exceeded".to_owned(),
                ));
            }
            self.stream_pending.insert(
                request_id.clone(),
                StreamPending {
                    tx: event_tx,
                    agent_ref: target.to_owned(),
                    node_id: peer_node_id.to_owned(),
                    deadline: Instant::now() + PEER_STREAM_MAX_LIFETIME,
                },
            );
        }
        if let Err(error) = self.send(
            peer_node_id,
            RelayFrame::Request {
                request_id: request_id.clone(),
                target: target.to_owned(),
                method: method.to_owned(),
                params,
                principal: principal.to_owned(),
                deadline_ms: PEER_REQUEST_TIMEOUT.as_millis() as u64,
            },
        ) {
            self.stream_pending.remove(&request_id);
            return Err(PeerInvokeError::Unavailable(error.to_string()));
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

/// Return whether a signaling node ID cannot alter canonical field boundaries.
pub(crate) fn valid_signaling_node_id(value: &str) -> bool {
    !value.is_empty() && !value.contains(['\r', '\n'])
}

/// Return whether a signaling session ID is bounded and cannot alter canonical
/// field boundaries.
pub(crate) fn valid_signaling_id(value: &str) -> bool {
    value.len() <= 128 && valid_signaling_node_id(value)
}

fn validate_signaling_fields(
    session_id: &str,
    source_node: &str,
    target_node: &str,
    kind: &str,
) -> anyhow::Result<()> {
    if !valid_signaling_id(session_id) {
        anyhow::bail!("invalid peer signaling session ID");
    }
    if !valid_signaling_node_id(source_node) || !valid_signaling_node_id(target_node) {
        anyhow::bail!("invalid peer signaling node ID");
    }
    if !matches!(kind, "offer" | "answer") {
        anyhow::bail!("invalid peer signaling kind");
    }
    Ok(())
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
    validate_signaling_fields(session_id, &source_node, target_node, kind)?;
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
    validate_signaling_fields(session_id, source_node, &target_node, kind)?;
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
    session_generation: u64,
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
            self.state.peer_manager.remove_session_if_generation_owned(
                &self.session_id,
                &self.peer_node_id,
                self.session_generation,
            );
            if let Some(connection) = self
                .state
                .peer_manager
                .direct_connections
                .get(&self.peer_node_id)
                .filter(|entry| {
                    entry.session_id == self.session_id
                        && entry.session_generation == Some(self.session_generation)
                })
            {
                let generation = connection.generation;
                drop(connection);
                self.state
                    .peer_manager
                    .cancel_stream_tasks(&self.state.task_cancels, generation);
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
        let session_generation = self.session_generation;
        tokio::spawn(async move {
            run_data_channel(
                state,
                peer_node_id,
                session_id,
                session_generation,
                data_channel,
            )
            .await;
        });
    }
}

async fn build_connection(
    state: &AppState,
    peer_node_id: &str,
    session_id: &str,
    session_generation: u64,
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
        session_generation,
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
    let session_generation = state
        .peer_manager
        .reserve_session(&session_id, target_node)?;
    let (connection, gathered_rx) =
        match build_connection(state, target_node, &session_id, session_generation).await {
            Ok(connection) => connection,
            Err(error) => {
                state
                    .peer_manager
                    .remove_session_if_generation(&session_id, session_generation);
                return Err(error);
            }
        };
    let data_channel = match connection
        .create_data_channel(DATA_CHANNEL_LABEL, None)
        .await
        .context("create peer data channel")
    {
        Ok(data_channel) => data_channel,
        Err(error) => {
            state
                .peer_manager
                .remove_session_if_generation(&session_id, session_generation);
            if let Err(close_error) = connection.close().await {
                warn!(%close_error, "failed to close peer offer after data channel failure");
            }
            return Err(error);
        }
    };
    if let Err(error) =
        state
            .peer_manager
            .attach_session(&session_id, session_generation, connection.clone())
    {
        if let Err(close_error) = connection.close().await {
            warn!(%close_error, "failed to close untracked peer offer");
        }
        return Err(error);
    }
    tokio::spawn(run_data_channel(
        state.clone(),
        target_node.to_owned(),
        session_id.clone(),
        session_generation,
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
                .remove_session_if_generation(&session_id, session_generation);
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
    let session_generation = state
        .peer_manager
        .reserve_session(session_id, source_node)?;
    let (connection, gathered_rx) =
        match build_connection(state, source_node, session_id, session_generation).await {
            Ok(connection) => connection,
            Err(error) => {
                state
                    .peer_manager
                    .remove_session_if_generation(session_id, session_generation);
                return Err(error);
            }
        };
    if let Err(error) =
        state
            .peer_manager
            .attach_session(session_id, session_generation, connection.clone())
    {
        if let Err(close_error) = connection.close().await {
            warn!(%close_error, "failed to close untracked peer answerer");
        }
        return Err(error);
    }

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
                .remove_session_if_generation(session_id, session_generation);
            if let Err(close_error) = connection.close().await {
                warn!(%close_error, "failed to close rejected peer offer");
            }
            return Err(error);
        }
    };
    state.peer_manager.mark_session_consumed(session_id);
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
    state.peer_manager.sweep_expired_sessions();
    let (generation, connection) = {
        let mut sessions = state.peer_manager.lock_sessions();
        let Some(session) = sessions.get_mut(session_id) else {
            anyhow::bail!("unknown or replayed peer session '{session_id}'");
        };
        if session.peer_node_id != source_node {
            anyhow::bail!("peer answer source does not own session '{session_id}'");
        }
        if session.state != PeerSessionState::Pending {
            anyhow::bail!("peer answer session '{session_id}' was already consumed");
        }
        session.state = PeerSessionState::ApplyingAnswer;
        (
            session.generation,
            session
                .connection
                .clone()
                .ok_or_else(|| anyhow!("peer signaling session has no connection"))?,
        )
    };
    let answer = match RTCSessionDescription::answer(sdp.to_owned()) {
        Ok(answer) => answer,
        Err(error) => {
            state
                .peer_manager
                .remove_session_if_generation(session_id, generation);
            if let Err(close_error) = connection.close().await {
                warn!(%session_id, %close_error, "failed to close malformed peer answer");
            }
            return Err(error.into());
        }
    };
    if let Err(error) = connection
        .set_remote_description(answer)
        .await
        .context("apply peer SDP answer")
    {
        state
            .peer_manager
            .remove_session_if_generation(session_id, generation);
        if let Err(close_error) = connection.close().await {
            warn!(%session_id, %close_error, "failed to close rejected peer answer");
        }
        return Err(error);
    }
    let mut sessions = state.peer_manager.lock_sessions();
    let session = sessions
        .get_mut(session_id)
        .filter(|session| session.generation == generation)
        .ok_or_else(|| anyhow!("peer signaling session disappeared while applying answer"))?;
    session.state = PeerSessionState::AnswerApplied;
    Ok(())
}

fn validate_signal(session_id: &str, sdp: &str) -> anyhow::Result<()> {
    if !valid_signaling_id(session_id) {
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
    session_generation: u64,
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
    let generation = match state.peer_manager.register_session_connection(
        &peer_node_id,
        &session_id,
        session_generation,
        tx.clone(),
    ) {
        Ok(generation) => generation,
        Err(error) => {
            warn!(peer = %peer_node_id, session = %session_id, %error, "stale peer data channel rejected");
            if let Err(close_error) = data_channel.close().await {
                warn!(peer = %peer_node_id, %close_error, "failed to close stale data channel");
            }
            return;
        }
    };
    let local_node = match local_node_id(&state) {
        Ok(node_id) => node_id,
        Err(error) => {
            warn!(%error, "cannot advertise routes over peer data channel");
            state
                .peer_manager
                .cancel_stream_tasks(&state.task_cancels, generation);
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
    if let Err(error) = tx.try_send(RelayFrame::RouteLease {
        node_id: local_node.clone(),
        agents,
        ttl_ms: PEER_ROUTE_TTL.as_millis() as u64,
        epoch: 1,
    }) {
        warn!(peer = %peer_node_id, %error, "cannot queue peer route lease");
    }

    let mut message_counter = 0u64;
    let mut reassembly: HashMap<u64, Reassembly> = HashMap::new();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    heartbeat.tick().await;

    loop {
        if state
            .peer_manager
            .connection_cancelled(&peer_node_id, generation)
        {
            break;
        }
        tokio::select! {
            event = data_channel.poll() => {
                match event {
                    Some(DataChannelEvent::OnMessage(message)) => {
                        if state
                            .peer_manager
                            .connection_cancelled(&peer_node_id, generation)
                        {
                            break;
                        }
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
                                            Ok(frame)
                                                if !state
                                                    .peer_manager
                                                    .connection_cancelled(&peer_node_id, generation) =>
                                            {
                                                handle_peer_frame(
                                                    &state,
                                                    &peer_node_id,
                                                    generation,
                                                    frame,
                                                );
                                            }
                                            Ok(_) => break,
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
                if state
                    .peer_manager
                    .connection_cancelled(&peer_node_id, generation)
                {
                    break;
                }
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
        .cancel_stream_tasks(&state.task_cancels, generation);
    state
        .peer_manager
        .unregister_connection(&peer_node_id, generation);
    if let Err(error) = data_channel.close().await {
        debug!(peer = %peer_node_id, %error, "peer data channel close returned an error");
    }
}

/// Handle one inbound data-channel relay frame.
pub(crate) fn handle_peer_frame(
    state: &AppState,
    peer_node_id: &str,
    connection_generation: u64,
    frame: RelayFrame,
) {
    // Keep the current-generation map guard through frame admission. Connection
    // replacement takes the same shard lock, so a stale channel cannot cross
    // this check concurrently and start work as the replacement generation.
    let Some(connection) = state
        .peer_manager
        .direct_connections
        .get(peer_node_id)
        .filter(|connection| {
            connection.generation == connection_generation
                && !connection.cancelled.load(Ordering::Acquire)
        })
    else {
        warn!(peer = %peer_node_id, generation = connection_generation, "stale peer frame rejected");
        return;
    };
    let spoke_tx = connection.tx.clone();
    let connection_cancelled = connection.cancelled.clone();

    match frame {
        RelayFrame::Request {
            request_id,
            target,
            method,
            params,
            principal: _,
            ..
        } => {
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
                    connection_generation,
                    connection_cancelled,
                    spoke_tx.clone(),
                )
                .await;
                if let Some(response) = response
                    && let Err(error) = spoke_tx.try_send(RelayFrame::Response {
                        request_id,
                        response,
                    })
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
            if let Some(task) = state
                .peer_manager
                .take_stream_task(&request_id, connection_generation)
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
        RelayFrame::Ping { ts } => {
            tokio::spawn(async move {
                if let Err(error) = spoke_tx.try_send(RelayFrame::Pong { ts }) {
                    warn!(%error, "cannot queue peer pong");
                }
            });
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
    drop(connection);
}

async fn handle_peer_spoke_request(
    state: &AppState,
    request_id: &str,
    target: &str,
    method: &str,
    mut params: Value,
    peer_node_id: &str,
    connection_generation: u64,
    connection_cancelled: Arc<std::sync::atomic::AtomicBool>,
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
        let (task_id, event_rx, cancel_owner) = if method == "SubscribeToTask" {
            let (task_id, event_rx) = super::streaming::subscribe_to_task(state, &caller, &params);
            (task_id, event_rx, None)
        } else {
            super::streaming::spawn_streaming_task(state.clone(), caller, params).await
        };
        state.peer_manager.peer_stream_tasks.insert(
            request_id.to_owned(),
            PeerStreamTask {
                task_id: task_id.clone(),
                connection_generation,
                cancel_owner,
            },
        );
        if connection_cancelled.load(Ordering::Acquire) {
            if let Some(task) = state
                .peer_manager
                .take_stream_task(request_id, connection_generation)
                && let Some(owner) = task.cancel_owner
                && let Some(token) = crate::server::remove_task_cancel_if_owner(
                    &state.task_cancels,
                    &task.task_id,
                    &owner,
                )
            {
                token.cancel();
            }
            return None;
        }
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
                            .try_send(RelayFrame::Event {
                                request_id: request_id.clone(),
                                seq,
                                result: event.to_wire_event(),
                            })
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
            peer_manager.take_stream_task(&request_id, connection_generation);
            if let Err(error) = spoke_tx.try_send(RelayFrame::Response {
                request_id,
                response: JsonRpcResponse::ok(
                    Value::String(task_id),
                    serde_json::json!({"ok": true}),
                ),
            }) {
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

    struct NoopPeerHandler;

    #[async_trait::async_trait]
    impl PeerConnectionEventHandler for NoopPeerHandler {}

    struct TestPeerHandler {
        gathered_tx: mpsc::Sender<()>,
        remote_data_channel_tx: Option<mpsc::Sender<Arc<dyn DataChannel>>>,
    }

    #[async_trait::async_trait]
    impl PeerConnectionEventHandler for TestPeerHandler {
        async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
            if state == RTCIceGatheringState::Complete
                && let Err(error) = self.gathered_tx.try_send(())
                && !matches!(error, mpsc::error::TrySendError::Full(_))
            {
                panic!("ICE gathering receiver closed unexpectedly");
            }
        }

        async fn on_data_channel(&self, data_channel: Arc<dyn DataChannel>) {
            if let Some(tx) = &self.remote_data_channel_tx
                && tx.try_send(data_channel).is_err()
            {
                panic!("remote data channel receiver unavailable");
            }
        }
    }

    #[test]
    fn signaling_identifiers_reject_canonical_delimiters() {
        assert!(validate_signal("session-1", "v=0\r\n").is_ok());
        assert!(validate_signal("session\nnode-a", "v=0\r\n").is_err());
        assert!(validate_signal("session\rnode-a", "v=0\r\n").is_err());
        assert!(!valid_signaling_id("node-a\nnode-b"));
        assert!(!valid_signaling_id("node-a\rnode-b"));
        assert!(!valid_signaling_node_id("node-a\nnode-b"));
        assert!(!valid_signaling_node_id("node-a\rnode-b"));
        assert!(validate_signaling_fields("session-1", "node-a", "node-b", "offer").is_ok());
        assert!(validate_signaling_fields("session-1", "node-a", "node-b", "answer").is_ok());
        assert!(validate_signaling_fields("session\nnode-a", "node-a", "node-b", "offer").is_err());
        assert!(validate_signaling_fields("session-1", "node-a", "node-b", "offer\n").is_err());
    }

    #[test]
    fn signaling_signature_binds_all_canonical_fields() {
        let (private_key, public_key) = crate::a2a::relay_identity::generate_keypair_b64();
        let signing_key = signing_key_from_b64(&private_key).expect("test signing key");
        let payload = signaling_payload("session-1", "node-a", "node-b", "offer", "v=0\r\n");
        assert_eq!(
            payload,
            b"rsclaw.a2a.webrtc.v1\nsession-1\nnode-a\nnode-b\noffer\nv=0\r\n"
        );
        let signature = sign_payload(&signing_key, &payload);
        verify_payload(&public_key, &payload, &signature)
            .expect("canonical signature should verify");

        for tampered in [
            signaling_payload("session-2", "node-a", "node-b", "offer", "v=0\r\n"),
            signaling_payload("session-1", "node-c", "node-b", "offer", "v=0\r\n"),
            signaling_payload("session-1", "node-a", "node-c", "offer", "v=0\r\n"),
            signaling_payload("session-1", "node-a", "node-b", "answer", "v=0\r\n"),
            signaling_payload("session-1", "node-a", "node-b", "offer", "v=1\r\n"),
        ] {
            assert!(
                verify_payload(&public_key, &tampered, &signature).is_err(),
                "tampering with any signaling field must reject the signature"
            );
        }
        assert!(verify_payload(&public_key, &payload, "not-base64").is_err());
        assert!(verify_payload(&public_key, &payload, "").is_err());
    }

    async fn wait_for_data_channel_open(data_channel: &Arc<dyn DataChannel>) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match data_channel.poll().await {
                    Some(DataChannelEvent::OnOpen) => return,
                    Some(DataChannelEvent::OnError) => panic!("data channel failed before opening"),
                    Some(DataChannelEvent::OnClose | DataChannelEvent::OnClosing) | None => {
                        panic!("data channel closed before opening")
                    }
                    Some(_) => {}
                }
            }
        })
        .await
        .expect("data channel should open before timeout");
    }

    async fn send_test_frame(
        data_channel: &Arc<dyn DataChannel>,
        message_id: u64,
        frame: &RelayFrame,
    ) {
        for chunk in encode_chunks(message_id, frame).expect("test frame should encode") {
            data_channel
                .send(BytesMut::from(chunk.as_slice()))
                .await
                .expect("test data channel send should succeed");
        }
    }

    async fn receive_test_frame(data_channel: &Arc<dyn DataChannel>) -> RelayFrame {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut partials: HashMap<u64, Reassembly> = HashMap::new();
            loop {
                match data_channel.poll().await {
                    Some(DataChannelEvent::OnMessage(message)) => {
                        let (message_id, index, count, payload) =
                            decode_chunk(&message.data).expect("test chunk should decode");
                        let partial = partials
                            .entry(message_id)
                            .or_insert_with(|| Reassembly::new(count));
                        if let Some(bytes) = partial
                            .insert(index, payload)
                            .expect("test chunk should reassemble")
                        {
                            return serde_json::from_slice(&bytes)
                                .expect("reassembled test frame should deserialize");
                        }
                    }
                    Some(DataChannelEvent::OnError) => panic!("data channel receive failed"),
                    Some(DataChannelEvent::OnClose | DataChannelEvent::OnClosing) | None => {
                        panic!("data channel closed before frame arrived")
                    }
                    Some(_) => {}
                }
            }
        })
        .await
        .expect("relay frame should arrive before timeout")
    }

    #[tokio::test]
    async fn real_webrtc_data_channel_exchanges_chunked_relay_frames() {
        let (offer_gathered_tx, mut offer_gathered_rx) = mpsc::channel(1);
        let (answer_gathered_tx, mut answer_gathered_rx) = mpsc::channel(1);
        let (remote_data_channel_tx, mut remote_data_channel_rx) = mpsc::channel(1);

        let offerer = PeerConnectionBuilder::new()
            .with_handler(Arc::new(TestPeerHandler {
                gathered_tx: offer_gathered_tx,
                remote_data_channel_tx: None,
            }))
            .with_udp_addrs(vec!["127.0.0.1:0"])
            .build()
            .await
            .expect("build offerer WebRTC connection");
        let answerer = PeerConnectionBuilder::new()
            .with_handler(Arc::new(TestPeerHandler {
                gathered_tx: answer_gathered_tx,
                remote_data_channel_tx: Some(remote_data_channel_tx),
            }))
            .with_udp_addrs(vec!["127.0.0.1:0"])
            .build()
            .await
            .expect("build answerer WebRTC connection");

        let offer_data_channel = offerer
            .create_data_channel(DATA_CHANNEL_LABEL, None)
            .await
            .expect("create offerer data channel");
        let offer = offerer
            .create_offer(None)
            .await
            .expect("create WebRTC offer");
        offerer
            .set_local_description(offer)
            .await
            .expect("set offerer local description");
        tokio::time::timeout(ICE_GATHER_TIMEOUT, offer_gathered_rx.recv())
            .await
            .expect("offer ICE gathering should finish")
            .expect("offer ICE gathering channel should remain open");
        let offer = offerer
            .local_description()
            .await
            .expect("offerer local description should exist");

        answerer
            .set_remote_description(offer)
            .await
            .expect("apply offer to answerer");
        let answer = answerer
            .create_answer(None)
            .await
            .expect("create WebRTC answer");
        answerer
            .set_local_description(answer)
            .await
            .expect("set answerer local description");
        tokio::time::timeout(ICE_GATHER_TIMEOUT, answer_gathered_rx.recv())
            .await
            .expect("answer ICE gathering should finish")
            .expect("answer ICE gathering channel should remain open");
        let answer = answerer
            .local_description()
            .await
            .expect("answerer local description should exist");
        offerer
            .set_remote_description(answer)
            .await
            .expect("apply answer to offerer");

        let answer_data_channel =
            tokio::time::timeout(Duration::from_secs(10), remote_data_channel_rx.recv())
                .await
                .expect("answerer should receive the negotiated data channel")
                .expect("remote data channel sender should remain open");
        wait_for_data_channel_open(&offer_data_channel).await;
        wait_for_data_channel_open(&answer_data_channel).await;

        let request = RelayFrame::Request {
            request_id: "webrtc-request-1".to_owned(),
            target: "node-b/main".to_owned(),
            method: "SendMessage".to_owned(),
            params: serde_json::json!({"blob": "x".repeat(40_000)}),
            principal: "node-a".to_owned(),
            deadline_ms: 30_000,
        };
        send_test_frame(&offer_data_channel, 1, &request).await;
        let received = receive_test_frame(&answer_data_channel).await;
        assert!(matches!(
            received,
            RelayFrame::Request {
                request_id,
                target,
                params,
                ..
            } if request_id == "webrtc-request-1"
                && target == "node-b/main"
                && params["blob"].as_str().map(str::len) == Some(40_000)
        ));

        let response = RelayFrame::Response {
            request_id: "webrtc-request-1".to_owned(),
            response: JsonRpcResponse::ok(Value::Null, serde_json::json!({"id": "task-1"})),
        };
        send_test_frame(&answer_data_channel, 2, &response).await;
        let received = receive_test_frame(&offer_data_channel).await;
        assert!(matches!(
            received,
            RelayFrame::Response {
                request_id,
                response: JsonRpcResponse { result: Some(result), .. },
            } if request_id == "webrtc-request-1" && result["id"] == "task-1"
        ));

        let stream_request = RelayFrame::Request {
            request_id: "webrtc-stream-1".to_owned(),
            target: "node-b/main".to_owned(),
            method: "SendStreamingMessage".to_owned(),
            params: serde_json::json!({"message": "stream me"}),
            principal: "node-a".to_owned(),
            deadline_ms: 30_000,
        };
        send_test_frame(&offer_data_channel, 3, &stream_request).await;
        assert!(matches!(
            receive_test_frame(&answer_data_channel).await,
            RelayFrame::Request { request_id, method, .. }
                if request_id == "webrtc-stream-1" && method == "SendStreamingMessage"
        ));

        let event = RelayFrame::Event {
            request_id: "webrtc-stream-1".to_owned(),
            seq: 3,
            result: serde_json::json!({"kind": "status-update", "final": false}),
        };
        let terminal = RelayFrame::Response {
            request_id: "webrtc-stream-1".to_owned(),
            response: JsonRpcResponse::ok(Value::Null, serde_json::json!({"state": "completed"})),
        };
        send_test_frame(&answer_data_channel, 4, &event).await;
        send_test_frame(&answer_data_channel, 5, &terminal).await;
        assert!(matches!(
            receive_test_frame(&offer_data_channel).await,
            RelayFrame::Event { request_id, seq: 3, result }
                if request_id == "webrtc-stream-1" && result["final"] == false
        ));
        assert!(matches!(
            receive_test_frame(&offer_data_channel).await,
            RelayFrame::Response {
                request_id,
                response: JsonRpcResponse { result: Some(result), .. },
            } if request_id == "webrtc-stream-1" && result["state"] == "completed"
        ));

        let cancel = RelayFrame::Cancel {
            request_id: "webrtc-stream-2".to_owned(),
            task_id: Some("task-2".to_owned()),
        };
        send_test_frame(&offer_data_channel, 6, &cancel).await;
        assert!(matches!(
            receive_test_frame(&answer_data_channel).await,
            RelayFrame::Cancel { request_id, task_id: Some(task_id) }
                if request_id == "webrtc-stream-2" && task_id == "task-2"
        ));

        offer_data_channel
            .close()
            .await
            .expect("close offerer data channel");
        offerer.close().await.expect("close offerer connection");
        answerer.close().await.expect("close answerer connection");
    }

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
    async fn signaling_session_sweep_removes_expired_entries() {
        let manager = PeerManager::default();
        let connection = PeerConnectionBuilder::new()
            .with_handler(Arc::new(NoopPeerHandler))
            .with_udp_addrs(vec!["127.0.0.1:0"])
            .build()
            .await
            .expect("build test peer connection");
        manager.lock_sessions().insert(
            "expired".to_owned(),
            PeerSession {
                peer_node_id: "node-b".to_owned(),
                generation: 1,
                connection: Some(Arc::new(connection)),
                created_at: Instant::now() - PEER_SIGNAL_SESSION_TTL,
                state: PeerSessionState::Pending,
            },
        );

        manager.sweep_expired_sessions();

        assert!(manager.lock_sessions().is_empty());
    }

    #[tokio::test]
    async fn signaling_session_sweep_preserves_active_connection() {
        let manager = PeerManager::default();
        let connection = PeerConnectionBuilder::new()
            .with_handler(Arc::new(NoopPeerHandler))
            .with_udp_addrs(vec!["127.0.0.1:0"])
            .build()
            .await
            .expect("build test peer connection");
        manager.lock_sessions().insert(
            "active".to_owned(),
            PeerSession {
                peer_node_id: "node-b".to_owned(),
                generation: 1,
                connection: Some(Arc::new(connection)),
                created_at: Instant::now() - PEER_SIGNAL_SESSION_TTL,
                state: PeerSessionState::Pending,
            },
        );
        let (tx, _rx) = mpsc::channel(1);
        manager
            .register_session_connection("node-b", "active", 1, tx)
            .expect("matching signaling generation should register");

        manager.sweep_expired_sessions();

        assert!(manager.lock_sessions().contains_key("active"));
    }

    #[tokio::test]
    async fn control_disconnect_only_drops_pending_sessions() {
        let manager = PeerManager::default();
        for (session_id, state) in [
            ("pending", PeerSessionState::Pending),
            ("established", PeerSessionState::Established),
        ] {
            let connection = PeerConnectionBuilder::new()
                .with_handler(Arc::new(NoopPeerHandler))
                .with_udp_addrs(vec!["127.0.0.1:0"])
                .build()
                .await
                .expect("build test peer connection");
            manager.lock_sessions().insert(
                session_id.to_owned(),
                PeerSession {
                    peer_node_id: "node-b".to_owned(),
                    generation: 1,
                    connection: Some(Arc::new(connection)),
                    created_at: Instant::now(),
                    state,
                },
            );
        }

        manager
            .drop_pending_sessions("test relay control disconnect")
            .await;

        let sessions = manager.lock_sessions();
        assert!(!sessions.contains_key("pending"));
        assert!(sessions.contains_key("established"));
    }

    #[tokio::test]
    async fn signaling_session_admission_rejects_duplicates_and_capacity_overflow() {
        let manager = PeerManager::default();
        manager
            .reserve_session("duplicate", "node-b")
            .expect("initial signaling reservation");
        assert!(manager.reserve_session("duplicate", "node-b").is_err());

        for index in 1..MAX_PEER_SIGNAL_SESSIONS {
            manager
                .reserve_session(&format!("session-{index}"), "node-b")
                .expect("signaling reservation within capacity");
        }
        assert!(manager.reserve_session("overflow", "node-b").is_err());
        assert_eq!(manager.lock_sessions().len(), MAX_PEER_SIGNAL_SESSIONS);
    }

    #[tokio::test]
    async fn direct_unary_correlation_admission_is_bounded() {
        let manager = PeerManager::default();
        for index in 0..MAX_PEER_PENDING_REQUESTS {
            let (tx, _rx) = oneshot::channel();
            manager
                .pending
                .insert(format!("existing-{index}"), (tx, "node-b".to_owned()));
        }

        let result = manager
            .invoke_jsonrpc(
                "node-b/main",
                "SendMessage",
                serde_json::json!({}),
                "caller",
                "node-b",
            )
            .await;

        assert!(matches!(result, Err(PeerInvokeError::Unavailable(_))));
        assert_eq!(manager.pending.len(), MAX_PEER_PENDING_REQUESTS);
    }

    #[tokio::test]
    async fn direct_stream_correlation_admission_is_bounded() {
        let manager = PeerManager::default();
        for index in 0..MAX_PEER_STREAMS {
            let (tx, _rx) = broadcast::channel(1);
            manager.stream_pending.insert(
                format!("existing-{index}"),
                StreamPending {
                    tx,
                    agent_ref: "node-b/main".to_owned(),
                    node_id: "node-b".to_owned(),
                    deadline: Instant::now() + PEER_STREAM_MAX_LIFETIME,
                },
            );
        }

        let result = manager
            .invoke_streaming(
                "node-b/main",
                "SendStreamingMessage",
                serde_json::json!({}),
                "caller",
                "node-b",
            )
            .await;

        assert!(matches!(result, Err(PeerInvokeError::Unavailable(_))));
        assert_eq!(manager.stream_pending.len(), MAX_PEER_STREAMS);
    }

    #[test]
    fn direct_send_returns_immediately_when_queue_is_full() {
        let manager = PeerManager::default();
        let (tx, _rx) = mpsc::channel(1);
        manager.register_connection("node-b", "session", tx);

        manager
            .send("node-b", RelayFrame::Ping { ts: 1 })
            .expect("first frame should enter queue");
        assert!(manager.send("node-b", RelayFrame::Ping { ts: 2 }).is_err());
    }

    #[test]
    fn consumed_signaling_session_rejects_replay_after_live_cleanup() {
        let manager = PeerManager::default();
        let generation = manager
            .reserve_session("replayed-offer", "node-b")
            .expect("initial offer should reserve");
        manager.remove_session_if_generation("replayed-offer", generation);
        manager.mark_session_consumed("replayed-offer");

        assert!(manager.reserve_session("replayed-offer", "node-b").is_err());
    }

    #[test]
    fn stale_data_channel_generation_cannot_claim_reused_session() {
        let manager = PeerManager::default();
        let old_generation = manager
            .reserve_session("reused-session", "node-b")
            .expect("old session should reserve");
        manager.remove_session_if_generation("reused-session", old_generation);
        let new_generation = manager
            .reserve_session("reused-session", "node-b")
            .expect("replacement session should reserve");
        let (stale_tx, _stale_rx) = mpsc::channel(1);

        assert!(
            manager
                .register_session_connection("node-b", "reused-session", old_generation, stale_tx,)
                .is_err()
        );
        assert!(!manager.has_direct_connection("node-b"));
        assert_eq!(
            manager
                .lock_sessions()
                .get("reused-session")
                .map(|session| session.generation),
            Some(new_generation)
        );
    }

    #[test]
    fn replacement_connection_removes_superseded_established_session() {
        let manager = PeerManager::default();
        let first_session_generation = manager
            .reserve_session("session-1", "node-b")
            .expect("first session should reserve");
        let (first_tx, _first_rx) = mpsc::channel(1);
        manager
            .register_session_connection("node-b", "session-1", first_session_generation, first_tx)
            .expect("first direct connection should register");

        let second_session_generation = manager
            .reserve_session("session-2", "node-b")
            .expect("second session should reserve");
        let (second_tx, _second_rx) = mpsc::channel(1);
        manager
            .register_session_connection(
                "node-b",
                "session-2",
                second_session_generation,
                second_tx,
            )
            .expect("replacement direct connection should register");

        let sessions = manager.lock_sessions();
        assert!(!sessions.contains_key("session-1"));
        assert!(sessions.contains_key("session-2"));
    }

    #[test]
    fn disconnect_cancels_only_owned_inbound_stream_tasks() {
        let manager = PeerManager::default();
        let task_cancels = DashMap::new();
        let owned = Arc::new(tokio_util::sync::CancellationToken::new());
        let replacement = Arc::new(tokio_util::sync::CancellationToken::new());
        task_cancels.insert("task-owned".to_owned(), owned.clone());
        task_cancels.insert("task-replacement".to_owned(), replacement.clone());
        manager.peer_stream_tasks.insert(
            "request-owned".to_owned(),
            PeerStreamTask {
                task_id: "task-owned".to_owned(),
                connection_generation: 1,
                cancel_owner: Some(owned.clone()),
            },
        );
        manager.peer_stream_tasks.insert(
            "request-replacement".to_owned(),
            PeerStreamTask {
                task_id: "task-replacement".to_owned(),
                connection_generation: 2,
                cancel_owner: Some(replacement.clone()),
            },
        );

        assert!(manager.take_stream_task("request-replacement", 1).is_none());
        manager.cancel_stream_tasks(&task_cancels, 1);

        assert!(owned.is_cancelled());
        assert!(!replacement.is_cancelled());
        assert!(!task_cancels.contains_key("task-owned"));
        assert!(task_cancels.contains_key("task-replacement"));
        assert!(!manager.peer_stream_tasks.contains_key("request-owned"));
        assert!(
            manager
                .peer_stream_tasks
                .contains_key("request-replacement")
        );

        let old_same_id = Arc::new(tokio_util::sync::CancellationToken::new());
        let replacement_same_id = Arc::new(tokio_util::sync::CancellationToken::new());
        task_cancels.insert("shared-task".to_owned(), replacement_same_id.clone());
        manager.peer_stream_tasks.insert(
            "old-shared-request".to_owned(),
            PeerStreamTask {
                task_id: "shared-task".to_owned(),
                connection_generation: 3,
                cancel_owner: Some(old_same_id.clone()),
            },
        );

        manager.cancel_stream_tasks(&task_cancels, 3);

        assert!(!old_same_id.is_cancelled());
        assert!(!replacement_same_id.is_cancelled());
        assert!(task_cancels.contains_key("shared-task"));
    }

    #[tokio::test]
    async fn stale_generation_cannot_admit_inbound_frames() {
        let manager = PeerManager::default();
        let (first_tx, _first_rx) = mpsc::channel(1);
        let first = manager.register_connection("node-b", "session-1", first_tx);
        let (second_tx, _second_rx) = mpsc::channel(1);
        let second = manager.register_connection("node-b", "session-2", second_tx);

        assert!(!manager.connection_is_current("node-b", first));
        assert!(manager.connection_is_current("node-b", second));
    }

    #[tokio::test]
    async fn old_generation_cannot_remove_new_connection() {
        let manager = PeerManager::default();
        let (first_tx, _first_rx) = mpsc::channel(1);
        let first = manager.register_connection("node-b", "session-1", first_tx);
        let first_cancelled = manager
            .direct_connections
            .get("node-b")
            .expect("first connection should be registered")
            .cancelled
            .clone();
        let (second_tx, _second_rx) = mpsc::channel(1);
        let second = manager.register_connection("node-b", "session-2", second_tx);

        assert!(first_cancelled.load(Ordering::Acquire));
        manager.unregister_connection("node-b", first);
        assert!(manager.has_direct_connection("node-b"));
        manager.unregister_connection("node-b", second);
        assert!(!manager.has_direct_connection("node-b"));
    }
}
