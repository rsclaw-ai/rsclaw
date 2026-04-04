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

// ---------------------------------------------------------------------------
// ConnHandle
// ---------------------------------------------------------------------------

pub struct ConnHandle {
    pub id: ConnId,
    pub event_tx: OutboundTx,
    pub subscribed_sessions: HashSet<String>,
    pub seq: u64,
}

impl ConnHandle {
    pub fn new(id: ConnId, event_tx: OutboundTx) -> Self {
        Self {
            id,
            event_tx,
            subscribed_sessions: HashSet::new(),
            seq: 0,
        }
    }

    pub fn next_seq(&mut self) -> u64 {
        self.seq += 1;
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

    pub async fn count(&self) -> usize {
        self.inner.read().await.len()
    }

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
            if h.event_tx.try_send(text.clone()).is_err() {
                // Channel full or closed; skip this connection.
            }
        }
    }
}
