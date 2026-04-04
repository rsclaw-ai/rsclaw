//! Notification system for ACP client.
//!
//! Provides a pluggable notification sink architecture that supports multiple
//! channels (Feishu, Telegram, Discord, etc.) with configurable priority
//! filtering and burn-after-read semantics.

use std::sync::Arc;

use anyhow::Result;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::acp::types::SessionId;

// ---------------------------------------------------------------------------
// Notification types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationPriority {
    Low = 0,
    Medium = 1,
    High = 2,
}

impl NotificationPriority {
    pub fn as_str(&self) -> &'static str {
        match self {
            NotificationPriority::High => "high",
            NotificationPriority::Medium => "medium",
            NotificationPriority::Low => "low",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notification {
    pub session_id: Option<SessionId>,
    pub priority: NotificationPriority,
    pub title: String,
    pub body: String,
    pub burn_after_read: bool,
}

impl Notification {
    pub fn new(priority: NotificationPriority, title: &str, body: &str) -> Self {
        Self {
            session_id: None,
            priority,
            title: title.to_string(),
            body: body.to_string(),
            burn_after_read: false,
        }
    }

    pub fn with_session_id(mut self, session_id: SessionId) -> Self {
        self.session_id = Some(session_id);
        self
    }

    pub fn with_burn_after_read(mut self) -> Self {
        self.burn_after_read = true;
        self
    }
}

// ---------------------------------------------------------------------------
// NotificationSink trait
// ---------------------------------------------------------------------------

pub trait NotificationSink: Send + Sync {
    fn name(&self) -> &str;

    fn priority_filter(&self) -> NotificationPriority;

    fn send(&self, notification: &Notification) -> BoxFuture<'_, Result<()>>;
}

// ---------------------------------------------------------------------------
// NotificationManager
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct NotificationManager {
    sinks: Vec<Arc<dyn NotificationSink>>,
}

impl NotificationManager {
    pub fn new() -> Self {
        Self { sinks: Vec::new() }
    }

    pub fn add_sink(&mut self, sink: Arc<dyn NotificationSink>) {
        self.sinks.push(sink);
    }

    pub async fn send(&self, notification: &Notification) {
        for sink in &self.sinks {
            if notification.priority >= sink.priority_filter() {
                let sink_name = sink.name().to_string();
                let notification = notification.clone();
                let sink = Arc::clone(sink);
                tokio::spawn(async move {
                    if let Err(e) = sink.send(&notification).await {
                        tracing::warn!(channel = sink_name, "notification send failed: {}", e);
                    }
                });
            }
        }
    }
}
