//! Notification sink trait — proper home for the cap bridge's P2
//! notification path. Moved from `src/channel/feishu.rs` (where it lived
//! transitionally after `src/acp/notification.rs` was deleted in Task 12).

use futures::future::BoxFuture;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationPriority {
    Low = 0,
    Medium = 1,
    High = 2,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notification {
    pub session_id: Option<String>,
    pub priority: NotificationPriority,
    pub title: String,
    pub body: String,
    pub burn_after_read: bool,
}

pub trait NotificationSink: Send + Sync {
    fn name(&self) -> &str;
    fn priority_filter(&self) -> NotificationPriority;
    fn send(&self, notification: &Notification) -> BoxFuture<'_, anyhow::Result<()>>;
}
