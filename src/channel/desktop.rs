//! Desktop channel — delivers messages to connected WebSocket clients (Tauri UI).
//!
//! This channel does not receive inbound messages; it only supports outbound
//! delivery (used by cron jobs configured with `delivery.channel = "desktop"`).
//! Messages are broadcast as `"notification"` events to all active WS connections.

use std::sync::Arc;

use anyhow::Result;
use futures::future::BoxFuture;
use tracing::debug;

use super::{Channel, OutboundMessage};
use crate::ws::{ConnRegistry, types::EventFrame};

/// A channel that delivers outbound messages to the desktop UI via WebSocket broadcast.
pub struct DesktopChannel {
    conns: Arc<ConnRegistry>,
}

impl DesktopChannel {
    /// Create a new desktop channel backed by the given WebSocket connection registry.
    pub fn new(conns: Arc<ConnRegistry>) -> Self {
        Self { conns }
    }
}

impl Channel for DesktopChannel {
    fn name(&self) -> &str {
        "desktop"
    }

    fn send(&self, msg: OutboundMessage) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let payload = serde_json::json!({
                "type": "notification",
                "channel": "desktop",
                "to": msg.target_id,
                "text": msg.text,
            });
            let frame = EventFrame::new("notification", payload, 0);
            debug!(target_id = %msg.target_id, text_len = msg.text.len(), "desktop: broadcasting notification");
            self.conns.broadcast_all(frame).await;
            Ok(())
        })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        // Desktop channel is send-only; no inbound loop needed.
        Box::pin(async { Ok(()) })
    }
}
