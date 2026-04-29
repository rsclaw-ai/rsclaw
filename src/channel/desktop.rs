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
            // Detect optional kind marker. Other channel impls don't see this
            // because routing only sends desktop-targeted messages here. The
            // marker is a structural prefix the sender embeds when the
            // notification represents a non-conversational event (async
            // /task completion, async /send delivery) so the UI can badge
            // it visually instead of confusing the user with a duplicate
            // chat-style bubble.
            const KIND_PREFIX: &str = "\u{e000}rsclaw:kind=";
            const KIND_PAYLOAD_SEP: char = '\u{e001}';
            let (kind, body) = if let Some(rest) = msg.text.strip_prefix(KIND_PREFIX) {
                if let Some(sep_idx) = rest.find(KIND_PAYLOAD_SEP) {
                    let (k, b) = rest.split_at(sep_idx);
                    (Some(k.to_owned()), b[KIND_PAYLOAD_SEP.len_utf8()..].to_owned())
                } else {
                    (None, msg.text.clone())
                }
            } else {
                (None, msg.text.clone())
            };

            let mut payload = serde_json::json!({
                "type": "notification",
                "channel": "desktop",
                "to": msg.target_id,
                "text": body,
            });
            if let Some(k) = kind.as_ref() {
                if let Some(obj) = payload.as_object_mut() {
                    obj.insert("kind".to_string(), serde_json::Value::String(k.clone()));
                }
            }
            let frame = EventFrame::new("notification", payload, 0);
            debug!(
                target_id = %msg.target_id,
                text_len = msg.text.len(),
                kind = ?kind,
                "desktop: broadcasting notification"
            );
            self.conns.broadcast_all(frame).await;
            Ok(())
        })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        // Desktop channel is send-only; no inbound loop needed.
        Box::pin(async { Ok(()) })
    }
}
