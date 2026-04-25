//! Persistent task queue with priority, dedup, merge, and TTL.
//!
//! When an inbound message arrives while the agent is busy, it is enqueued
//! here and processed in priority order (System > Cron > User, FIFO within
//! the same priority level).

use std::sync::Arc;

use anyhow::Result;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};

use crate::store::redb_store::RedbStore;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Task priority. Lower numeric value = higher priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum Priority {
    /// Internal system tasks (highest).
    System = 0,
    /// Scheduled / cron tasks.
    Cron = 1,
    /// User-initiated messages (default).
    User = 2,
}

/// Lifecycle status of a queued task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Pending,
    Running,
    Done,
    Failed,
    /// Exceeded max retries — dead-letter.
    Dead,
}

/// A message captured for later processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedMessage {
    pub text: String,
    pub sender: String,
    pub channel: String,
    pub timestamp: i64,
    /// Base64-encoded images or file-system paths.
    pub images: Vec<String>,
    /// Attached file paths.
    pub files: Vec<String>,
}

/// A task sitting in the persistent queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedTask {
    pub id: String,
    pub session_key: String,
    pub messages: Vec<QueuedMessage>,
    pub priority: Priority,
    pub status: TaskStatus,
    pub retries: u32,
    pub max_retries: u32,
    pub created_at: i64,
    pub updated_at: i64,
    /// Time-to-live in seconds. 0 means no expiry.
    pub ttl_secs: u64,
    /// MD5 hash of the first message text (for dedup).
    pub content_hash: String,
    /// Last error message, if any.
    pub error: Option<String>,
}

impl QueuedTask {
    /// Create a new pending task from a single inbound message.
    pub fn new(session_key: String, message: QueuedMessage, priority: Priority) -> Self {
        let now = chrono::Utc::now().timestamp();
        let hash = compute_hash(&message.text);
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            session_key,
            messages: vec![message],
            priority,
            status: TaskStatus::Pending,
            retries: 0,
            max_retries: 3,
            created_at: now,
            updated_at: now,
            ttl_secs: 3600,
            content_hash: hash,
            error: None,
        }
    }

    /// Whether this task has exceeded its TTL.
    pub fn is_expired(&self) -> bool {
        if self.ttl_secs == 0 {
            return false;
        }
        let now = chrono::Utc::now().timestamp();
        now - self.created_at > self.ttl_secs as i64
    }

    /// Combine all queued messages into a single prompt string.
    pub fn merged_text(&self) -> String {
        if self.messages.len() == 1 {
            return self.messages[0].text.clone();
        }
        self.messages
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n---\n\n")
    }
}

/// Compute an MD5 hex digest of `text` (for content dedup, not security).
fn compute_hash(text: &str) -> String {
    let hash = Md5::digest(text.as_bytes());
    hex::encode(hash)
}

// ---------------------------------------------------------------------------
// Queue stats
// ---------------------------------------------------------------------------

/// Snapshot of queue occupancy by status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueStats {
    pub pending: usize,
    pub running: usize,
    pub done: usize,
    pub failed: usize,
    pub dead: usize,
}

// ---------------------------------------------------------------------------
// TaskQueueManager
// ---------------------------------------------------------------------------

/// High-level task queue API used by the gateway.
pub struct TaskQueueManager {
    store: Arc<RedbStore>,
}

impl TaskQueueManager {
    /// Create a new manager backed by the given store.
    pub fn new(store: Arc<RedbStore>) -> Self {
        Self { store }
    }

    /// Submit a new task. Handles dedup and merge automatically.
    ///
    /// Returns `(task_id, was_merged)`. If the message is a duplicate of an
    /// existing pending task the id `"dedup"` is returned. If the message was
    /// merged into an existing pending task for the same session the id
    /// `"merged"` is returned.
    pub fn submit(
        &self,
        session_key: &str,
        message: QueuedMessage,
        priority: Priority,
    ) -> Result<(String, bool)> {
        let hash = compute_hash(&message.text);

        // Dedup: same content within short window.
        if self.store.has_duplicate(session_key, &hash)? {
            tracing::info!(session_key, "task_queue: duplicate message dropped");
            return Ok(("dedup".to_string(), false));
        }

        // Merge: if there is already a pending task for this session, append.
        if self.store.merge_into_pending(session_key, &message)? {
            tracing::info!(session_key, "task_queue: message merged into pending task");
            return Ok(("merged".to_string(), true));
        }

        // New task.
        let task = QueuedTask::new(session_key.to_string(), message, priority);
        let id = task.id.clone();
        self.store.enqueue_task(&task)?;
        tracing::info!(session_key, task_id = %id, "task_queue: new task enqueued");
        Ok((id, false))
    }

    /// Get the next task to process (highest priority, oldest first).
    ///
    /// Expired tasks are cleaned up before dequeuing.
    pub fn next(&self) -> Result<Option<QueuedTask>> {
        let cleaned = self.store.cleanup_expired_tasks()?;
        if cleaned > 0 {
            tracing::info!(count = cleaned, "task_queue: cleaned expired tasks");
        }
        self.store.dequeue_task()
    }

    /// Mark a task as done.
    pub fn complete(&self, task_id: &str) -> Result<()> {
        self.store.update_task_status(task_id, TaskStatus::Done)
    }

    /// Mark a task as failed. Auto-retries up to `max_retries`; beyond that
    /// the task moves to `Dead` status.
    pub fn fail(&self, task_id: &str, _error: &str, max_retries: u32) -> Result<TaskStatus> {
        self.store.fail_task(task_id, max_retries)
    }

    /// Return a snapshot of queue occupancy by status.
    pub fn stats(&self) -> Result<QueueStats> {
        let all = self.store.list_tasks(None)?;
        Ok(QueueStats {
            pending: all.iter().filter(|t| t.status == TaskStatus::Pending).count(),
            running: all.iter().filter(|t| t.status == TaskStatus::Running).count(),
            done: all.iter().filter(|t| t.status == TaskStatus::Done).count(),
            failed: all.iter().filter(|t| t.status == TaskStatus::Failed).count(),
            dead: all.iter().filter(|t| t.status == TaskStatus::Dead).count(),
        })
    }
}
