//! Connection registry — tracks active WebSocket connections and their
//! session subscriptions.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use tokio::sync::RwLock;
use tracing::warn;

use super::types::EventFrame;

pub type ConnId = String;
pub type OutboundTx = tokio::sync::mpsc::Sender<String>;

/// Summary of an active WS connection for the `acp list` API.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConnSummary {
    pub conn_id: String,
    pub client_id: Option<String>,
    pub version: Option<String>,
    pub platform: Option<String>,
    pub mode: Option<String>,
    pub sessions: usize,
    pub uptime_secs: u64,
}

// ---------------------------------------------------------------------------
// ConnHandle
// ---------------------------------------------------------------------------

pub struct ConnHandle {
    pub id: ConnId,
    pub event_tx: OutboundTx,
    pub subscribed_sessions: HashSet<String>,
    pub seq: u64,
    /// Client metadata from the WS connect handshake.
    pub client_info: Option<super::types::ClientInfo>,
    /// When this connection was established.
    pub connected_at: std::time::Instant,
}

impl ConnHandle {
    /// Create a new connection handle.
    pub fn new(id: ConnId, event_tx: OutboundTx) -> Self {
        Self {
            id,
            event_tx,
            subscribed_sessions: HashSet::new(),
            seq: 0,
            client_info: None,
            connected_at: std::time::Instant::now(),
        }
    }

    pub fn next_seq(&mut self) -> u64 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }
}

// ---------------------------------------------------------------------------
// ConnRegistry
// ---------------------------------------------------------------------------

pub struct ConnRegistry {
    inner: RwLock<HashMap<ConnId, Arc<RwLock<ConnHandle>>>>,
}

impl Default for ConnRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnRegistry {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    pub async fn register(&self, handle: Arc<RwLock<ConnHandle>>) {
        let id = handle.read().await.id.clone();
        self.inner.write().await.insert(id, handle);
    }

    pub async fn unregister(&self, id: &str) {
        self.inner.write().await.remove(id);
    }

    /// Number of active WS connections.
    pub async fn count(&self) -> usize {
        self.inner.read().await.len()
    }

    /// Snapshot of all active connections with client metadata.
    pub async fn list_connections(&self) -> Vec<ConnSummary> {
        let guard = self.inner.read().await;
        let mut out = Vec::with_capacity(guard.len());
        for handle in guard.values() {
            let h = handle.read().await;
            out.push(ConnSummary {
                conn_id: h.id.clone(),
                client_id: h.client_info.as_ref().and_then(|c| c.id.clone()),
                version: h.client_info.as_ref().and_then(|c| c.version.clone()),
                platform: h.client_info.as_ref().and_then(|c| c.platform.clone()),
                mode: h.client_info.as_ref().and_then(|c| c.mode.clone()),
                sessions: h.subscribed_sessions.len(),
                uptime_secs: h.connected_at.elapsed().as_secs(),
            });
        }
        out
    }

    /// Broadcast a serialized event frame to every connected WebSocket client.
    ///
    /// NOTE: We serialize once and `clone()` the `String` per connection.
    /// Using `Arc<String>` would save the per-connection clone, but the
    /// `OutboundTx` channel type (`mpsc::Sender<String>`) is shared with
    /// per-connection streaming paths (chat, sessions) that produce unique
    /// strings.  Changing the channel to `Arc<String>` would add an `Arc`
    /// allocation in every single-connection send for a marginal win only in
    /// broadcast, so we keep the simpler `String` channel.
    pub async fn broadcast_all(&self, frame: EventFrame) {
        let text = match serde_json::to_string(&frame) {
            Ok(t) => t,
            Err(e) => {
                warn!("ws: failed to serialize broadcast frame: {e}");
                return;
            }
        };
        let guard = self.inner.read().await;
        for handle in guard.values() {
            let h = handle.read().await;
            if let Err(e) = h.event_tx.try_send(text.clone()) {
                warn!(conn = %h.id, "ws broadcast: outbound channel full or closed: {e}");
            }
        }
    }
}
