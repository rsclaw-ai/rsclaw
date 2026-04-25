//! Persistent task queue with priority, dedup, merge, and TTL.
//!
//! When an inbound message arrives while the agent is busy, it is enqueued
//! here and processed in priority order (System > Cron > User, FIFO within
//! the same priority level).

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Result;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Notify};
use tracing::{error, info};

use crate::{
    agent::{AgentMessage, AgentRegistry, FileAttachment, ImageAttachment},
    channel::OutboundMessage,
    store::redb_store::RedbStore,
};

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

/// A file attachment staged on disk for queue persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedFile {
    /// Original filename from the channel.
    pub filename: String,
    /// Path to the staged file on disk (under `var/data/queue/staging/`).
    pub path: String,
    /// MIME type.
    pub mime_type: String,
}

/// A message captured for later processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedMessage {
    pub text: String,
    pub sender: String,
    pub channel: String,
    /// Platform-specific chat/conversation ID (e.g. Telegram chat_id).
    pub chat_id: String,
    /// Whether this message originated from a group conversation.
    pub is_group: bool,
    pub timestamp: i64,
    /// Base64-encoded images or file-system paths.
    pub images: Vec<String>,
    /// File attachments staged on disk.
    pub files: Vec<QueuedFile>,
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
    notify: Notify,
}

impl TaskQueueManager {
    /// Create a new manager backed by the given store.
    pub fn new(store: Arc<RedbStore>) -> Self {
        Self {
            store,
            notify: Notify::new(),
        }
    }

    /// Wait until a new task is submitted.
    ///
    /// Use inside `tokio::select!` with a fallback timeout so that the worker
    /// also picks up tasks that were persisted before the current process started.
    pub async fn notified(&self) {
        self.notify.notified().await;
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
            self.notify.notify_one();
            return Ok(("merged".to_string(), true));
        }

        // New task.
        let task = QueuedTask::new(session_key.to_string(), message, priority);
        let id = task.id.clone();
        self.store.enqueue_task(&task)?;
        tracing::info!(session_key, task_id = %id, "task_queue: new task enqueued");
        self.notify.notify_one();
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

// ---------------------------------------------------------------------------
// File staging
// ---------------------------------------------------------------------------

/// Return the staging directory for queue file attachments.
fn staging_dir() -> std::path::PathBuf {
    crate::config::loader::base_dir()
        .join("var/data/queue/staging")
}

/// Write file bytes to the staging directory and return a [`QueuedFile`].
///
/// The staged file is named `{uuid}_{original_filename}` to avoid collisions.
pub fn stage_file(filename: &str, data: &[u8], mime_type: &str) -> Result<QueuedFile> {
    let dir = staging_dir();
    std::fs::create_dir_all(&dir)?;
    let safe_name = filename.replace(['/', '\\'], "_");
    let staged = format!("{}_{}", uuid::Uuid::new_v4(), safe_name);
    let path = dir.join(&staged);
    std::fs::write(&path, data)?;
    Ok(QueuedFile {
        filename: filename.to_string(),
        path: path.to_string_lossy().to_string(),
        mime_type: mime_type.to_string(),
    })
}

/// Read a staged file back into a [`FileAttachment`].
fn unstage_file(qf: &QueuedFile) -> FileAttachment {
    let data = std::fs::read(&qf.path).unwrap_or_default();
    FileAttachment {
        filename: qf.filename.clone(),
        data,
        mime_type: qf.mime_type.clone(),
    }
}

/// Remove staged files for a completed/dead task.
fn cleanup_staged_files(task: &QueuedTask) {
    for msg in &task.messages {
        for qf in &msg.files {
            if let Err(e) = std::fs::remove_file(&qf.path) {
                // File may already be cleaned up or missing — not critical.
                tracing::debug!(path = %qf.path, "staging cleanup: {e}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Submit helper
// ---------------------------------------------------------------------------

/// Submit a message to the task queue instead of directly to the agent.
///
/// This is the recommended way for channels to send messages when the queue
/// is enabled. Returns `(task_id, was_merged)`.
pub fn submit_to_queue(
    manager: &TaskQueueManager,
    session_key: &str,
    text: &str,
    channel: &str,
    peer_id: &str,
    chat_id: &str,
    is_group: bool,
    priority: Priority,
) -> Result<(String, bool)> {
    let message = QueuedMessage {
        text: text.to_string(),
        sender: peer_id.to_string(),
        channel: channel.to_string(),
        chat_id: chat_id.to_string(),
        is_group,
        timestamp: chrono::Utc::now().timestamp(),
        images: vec![],
        files: vec![],
    };
    manager.submit(session_key, message, priority)
}

// ---------------------------------------------------------------------------
// TaskQueueWorker
// ---------------------------------------------------------------------------

/// Background worker that polls the task queue and dispatches tasks to agents.
///
/// Each dequeued task is spawned as a separate tokio task so multiple
/// channel messages can be processed concurrently.
pub struct TaskQueueWorker {
    manager: Arc<TaskQueueManager>,
    registry: Arc<AgentRegistry>,
    channel_senders: Arc<std::sync::RwLock<HashMap<String, mpsc::Sender<OutboundMessage>>>>,
    shutdown: super::shutdown::ShutdownCoordinator,
}

impl TaskQueueWorker {
    /// Create a new worker.
    pub fn new(
        manager: Arc<TaskQueueManager>,
        registry: Arc<AgentRegistry>,
        channel_senders: Arc<std::sync::RwLock<HashMap<String, mpsc::Sender<OutboundMessage>>>>,
        shutdown: super::shutdown::ShutdownCoordinator,
    ) -> Self {
        Self {
            manager,
            registry,
            channel_senders,
            shutdown,
        }
    }

    /// Main loop: wait for task notifications and dispatch them. Exits when
    /// the shutdown coordinator signals drain — already-running tasks complete,
    /// but no new ones are pulled. Persistent tasks left in the queue are
    /// picked up by the next gateway process on startup.
    ///
    /// Uses `tokio::select!` between the manager's `Notify` (instant wake on
    /// submit) and a 5-second fallback (picks up pre-existing or
    /// crash-recovered tasks).
    pub async fn run(self: Arc<Self>) {
        info!("task queue worker started");
        loop {
            if self.shutdown.is_draining() {
                info!("task queue worker: drain signaled, stopping dequeue");
                break;
            }
            match self.manager.next() {
                Ok(Some(task)) => {
                    let guard = self.shutdown.begin_work();
                    let worker = Arc::clone(&self);
                    tokio::spawn(async move {
                        worker.process_task(task).await;
                        drop(guard);
                    });
                    // Immediately loop back to check for more tasks.
                    continue;
                }
                Ok(None) => {
                    // No pending tasks — wait for a notification or fallback.
                    tokio::select! {
                        () = self.manager.notified() => {}
                        () = tokio::time::sleep(Duration::from_secs(5)) => {}
                    }
                }
                Err(e) => {
                    error!("task queue worker: dequeue error: {e:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
        info!("task queue worker exited");
    }

    /// Process a single queued task: send to agent, wait for reply, route back.
    async fn process_task(&self, task: QueuedTask) {
        let task_id = task.id.clone();
        let session_key = task.session_key.clone();

        // Determine channel + peer + chat from the first message.
        let Some(first_msg) = task.messages.first() else {
            error!(task_id = %task_id, "task queue worker: task has no messages, skipping");
            return;
        };
        let channel_name = first_msg.channel.clone();
        let peer_id = first_msg.sender.clone();
        let chat_id = first_msg.chat_id.clone();
        let is_group = first_msg.is_group;

        info!(
            task_id = %task_id,
            session_key = %session_key,
            channel = %channel_name,
            messages = task.messages.len(),
            "task queue worker: processing task"
        );

        // Resolve agent handle — route by channel, fall back to default.
        let handle = match self.registry.route(&channel_name) {
            Ok(h) => h,
            Err(_) => match self.registry.default_agent() {
                Ok(h) => h,
                Err(e) => {
                    error!(task_id = %task_id, "task queue worker: no agent for channel {channel_name}: {e:#}");
                    if let Err(fe) = self.manager.fail(&task_id, &format!("{e:#}"), task.max_retries) {
                        error!(task_id = %task_id, "task queue worker: fail() error: {fe:#}");
                    }
                    return;
                }
            },
        };

        // Build AgentMessage from merged text.
        let text = task.merged_text();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

        // Collect images and files from all queued messages.
        let images: Vec<ImageAttachment> = task
            .messages
            .iter()
            .flat_map(|m| {
                m.images.iter().map(|data| ImageAttachment {
                    data: data.clone(),
                    mime_type: "image/png".to_string(),
                })
            })
            .collect();
        let files: Vec<FileAttachment> = task
            .messages
            .iter()
            .flat_map(|m| m.files.iter().map(unstage_file))
            .collect();

        let msg = AgentMessage {
            session_key: session_key.clone(),
            text,
            channel: channel_name.clone(),
            peer_id: peer_id.clone(),
            chat_id: chat_id.clone(),
            reply_tx,
            extra_tools: vec![],
            images,
            files,
        };

        if handle.tx.send(msg).await.is_err() {
            error!(task_id = %task_id, "task queue worker: agent channel closed");
            if let Err(fe) = self.manager.fail(&task_id, "agent channel closed", task.max_retries) {
                error!(task_id = %task_id, "task queue worker: fail() error: {fe:#}");
            }
            return;
        }

        // Wait for reply with timeout (10 minutes, matching handle_pending_analysis).
        match tokio::time::timeout(Duration::from_secs(600), reply_rx).await {
            Ok(Ok(reply)) => {
                info!(task_id = %task_id, "task queue worker: task completed");
                if let Err(e) = self.manager.complete(&task_id) {
                    error!(task_id = %task_id, "task queue worker: complete() error: {e:#}");
                }
                cleanup_staged_files(&task);

                // Route reply back to the originating channel.
                if !reply.text.is_empty() || !reply.images.is_empty() || !reply.files.is_empty() {
                    // Use chat_id as target when available (e.g. Telegram group),
                    // fall back to peer_id for DM-style channels.
                    let target = if chat_id.is_empty() { peer_id.clone() } else { chat_id };
                    let out = OutboundMessage {
                        target_id: target,
                        is_group,
                        text: reply.text,
                        reply_to: None,
                        images: reply.images,
                        files: reply.files,
                        channel: Some(channel_name.clone()),
                    };
                    let tx = {
                        let guard = self
                            .channel_senders
                            .read()
                            .expect("channel_senders lock poisoned");
                        guard.get(&channel_name).cloned()
                    };
                    if let Some(tx) = tx {
                        if let Err(e) = tx.send(out).await {
                            error!(
                                task_id = %task_id,
                                channel = %channel_name,
                                "task queue worker: send reply failed: {e}"
                            );
                        }
                    } else {
                        tracing::warn!(
                            task_id = %task_id,
                            channel = %channel_name,
                            "task queue worker: no channel sender registered"
                        );
                    }
                }
            }
            Ok(Err(_)) => {
                error!(task_id = %task_id, "task queue worker: reply channel dropped");
                match self.manager.fail(&task_id, "reply channel dropped", task.max_retries) {
                    Ok(TaskStatus::Dead) => cleanup_staged_files(&task),
                    Err(fe) => error!(task_id = %task_id, "task queue worker: fail() error: {fe:#}"),
                    _ => {}
                }
            }
            Err(_) => {
                error!(task_id = %task_id, "task queue worker: reply timeout (600s)");
                match self.manager.fail(&task_id, "reply timeout", task.max_retries) {
                    Ok(TaskStatus::Dead) => cleanup_staged_files(&task),
                    Err(fe) => error!(task_id = %task_id, "task queue worker: fail() error: {fe:#}"),
                    _ => {}
                }
            }
        }
    }
}
