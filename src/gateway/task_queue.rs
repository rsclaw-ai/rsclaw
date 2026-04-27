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
use tracing::{error, info, warn};

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

/// Outcome of a single agent turn, used by the auto-continue supervisor.
#[derive(Debug)]
pub enum TaskOutcome {
    /// Agent clearly completed the task.
    Done,
    /// Agent made progress but explicitly needs to continue.
    Partial,
    /// Agent is stuck — no progress, empty reply, or error pattern.
    Stuck(String),
    /// Infrastructure error (timeout, channel closed, rate limit).
    Error(String),
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
    /// Platform message ID for reply quoting (e.g. QQ msg_id).
    #[serde(default)]
    pub reply_to: Option<String>,
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
    /// How many agent turns have been executed for this task.
    #[serde(default)]
    pub turns: u32,
    /// Max agent turns before giving up (0 = single turn, no auto-continue).
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
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
            turns: 0,
            max_turns: default_max_turns(),
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

/// Default max turns for regular messages (no auto-continue).
fn default_max_turns() -> u32 {
    0
}

/// Default max turns for /task mode.
pub const TASK_DEFAULT_MAX_TURNS: u32 = 10;
/// Default TTL for /task mode (1 hour).
pub const TASK_DEFAULT_TTL_SECS: u64 = 3600;

/// Parse `/task` prefix and extract `--turns N` / `--timeout Xh` flags.
///
/// Returns `(max_turns, ttl_secs)`. If the text does not start with `/task`,
/// returns `(0, 3600)` (regular chat mode). Modifies `text` in-place to
/// strip the `/task` prefix and flags, leaving only the actual message.
///
/// Examples:
/// - `/task fix the login bug` → turns=10, ttl=3600, text="fix the login bug"
/// - `/task --turns 20 refactor` → turns=20, ttl=3600, text="refactor"
/// - `/task --timeout 4h big job` → turns=10, ttl=14400, text="big job"
/// - `/task --turns 50 --timeout 8h x` → turns=50, ttl=28800, text="x"
/// - `hello` → turns=0, ttl=3600, text unchanged
fn parse_task_prefix(text: &mut String) -> (u32, u64) {
    let trimmed = text.trim();
    if !trimmed.starts_with("/task ") && trimmed != "/task" {
        // Natural language detection: if it looks like a task, auto-enable.
        if looks_like_task(trimmed) {
            return (TASK_DEFAULT_MAX_TURNS, TASK_DEFAULT_TTL_SECS);
        }
        return (0, TASK_DEFAULT_TTL_SECS);
    }

    // Strip "/task " prefix.
    let rest = trimmed.strip_prefix("/task").unwrap_or(trimmed).trim();
    let mut max_turns = TASK_DEFAULT_MAX_TURNS;
    let mut ttl_secs = TASK_DEFAULT_TTL_SECS;
    let mut remaining = rest.to_string();

    // Parse --turns N
    if let Some(pos) = remaining.find("--turns") {
        let after = &remaining[pos + 7..].trim_start();
        if let Some(end) = after.find(|c: char| c.is_whitespace()).or(Some(after.len())) {
            if let Ok(n) = after[..end].parse::<u32>() {
                max_turns = n;
            }
            remaining = format!(
                "{}{}",
                &remaining[..pos],
                after.get(end..).unwrap_or("")
            )
            .trim()
            .to_string();
        }
    }

    // Parse --timeout Xh / Xm / Xs
    if let Some(pos) = remaining.find("--timeout") {
        let after = &remaining[pos + 9..].trim_start();
        if let Some(end) = after.find(|c: char| c.is_whitespace()).or(Some(after.len())) {
            let val_str = &after[..end];
            if let Some(parsed) = parse_duration_str(val_str) {
                ttl_secs = parsed;
            }
            remaining = format!(
                "{}{}",
                &remaining[..pos],
                after.get(end..).unwrap_or("")
            )
            .trim()
            .to_string();
        }
    }

    *text = remaining;
    (max_turns, ttl_secs)
}

/// Parse a human-readable duration like "4h", "30m", "3600s", "2h30m".
fn parse_duration_str(s: &str) -> Option<u64> {
    let mut total: u64 = 0;
    let mut num_buf = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num_buf.push(c);
        } else {
            let n: u64 = num_buf.parse().ok()?;
            num_buf.clear();
            match c {
                'h' | 'H' => total += n * 3600,
                'm' | 'M' => total += n * 60,
                's' | 'S' => total += n,
                _ => return None,
            }
        }
    }
    // Bare number without unit → seconds.
    if !num_buf.is_empty() {
        total += num_buf.parse::<u64>().ok()?;
    }
    if total > 0 { Some(total) } else { None }
}

/// Heuristic check: does this message look like a task (vs. casual chat)?
///
/// Returns true when the text contains action-oriented patterns that
/// suggest the user wants sustained work, not a quick Q&A.
fn looks_like_task(text: &str) -> bool {
    // Too short — likely a greeting or quick question.
    if text.len() < 15 {
        return false;
    }

    let lower = text.to_lowercase();

    // Chinese task indicators.
    for pat in [
        "帮我", "帮忙", "请你", "麻烦", "实现", "开发", "编写", "修复",
        "重构", "优化", "部署", "测试", "写一个", "搞一个", "做一个",
        "创建", "生成", "分析", "调研", "设计", "搭建", "迁移",
    ] {
        if lower.contains(pat) {
            return true;
        }
    }

    // English task indicators.
    for pat in [
        "implement", "develop", "build", "create", "write a",
        "fix the", "fix this", "refactor", "optimize", "deploy",
        "set up", "migrate", "design", "analyze", "generate",
        "please write", "please create", "please fix", "please implement",
        "can you write", "can you create", "can you fix",
        "i need you to", "i want you to",
    ] {
        if lower.contains(pat) {
            return true;
        }
    }

    false
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

    /// Submit a new message. Handles dedup, merge, and `/task` parsing.
    ///
    /// If the message text starts with `/task`, it is parsed as a task-mode
    /// message with optional `--turns N` and `--timeout Nh/Nm/Ns` flags.
    /// Otherwise, it is a regular chat message (max_turns=0, no auto-continue).
    ///
    /// Returns `(task_id, was_merged)`.
    pub fn submit(
        &self,
        session_key: &str,
        mut message: QueuedMessage,
        priority: Priority,
    ) -> Result<(String, bool)> {
        // Parse /task prefix to extract mode + overrides.
        let (max_turns, ttl_secs) = parse_task_prefix(&mut message.text);

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
        let mut task = QueuedTask::new(session_key.to_string(), message, priority);
        task.max_turns = max_turns;
        task.ttl_secs = ttl_secs;
        let id = task.id.clone();
        self.store.enqueue_task(&task)?;
        if max_turns > 0 {
            tracing::info!(session_key, task_id = %id, max_turns, ttl_secs, "task_queue: task enqueued (task mode)");
        } else {
            tracing::info!(session_key, task_id = %id, "task_queue: message enqueued");
        }
        self.notify.notify_one();
        Ok((id, false))
    }

    /// Submit a task-mode message with custom turns and timeout.
    ///
    /// Unlike `submit()` which creates chat-mode tasks (max_turns=0),
    /// this creates a task that auto-continues until done.
    pub fn submit_task(
        &self,
        session_key: &str,
        message: QueuedMessage,
        priority: Priority,
        max_turns: u32,
        ttl_secs: u64,
    ) -> Result<(String, bool)> {
        let hash = compute_hash(&message.text);
        if self.store.has_duplicate(session_key, &hash)? {
            tracing::info!(session_key, "task_queue: duplicate task dropped");
            return Ok(("dedup".to_string(), false));
        }
        if self.store.merge_into_pending(session_key, &message)? {
            tracing::info!(session_key, "task_queue: message merged into pending task");
            self.notify.notify_one();
            return Ok(("merged".to_string(), true));
        }
        let mut task = QueuedTask::new(session_key.to_string(), message, priority);
        task.max_turns = max_turns;
        task.ttl_secs = ttl_secs;
        let id = task.id.clone();
        self.store.enqueue_task(&task)?;
        tracing::info!(session_key, task_id = %id, max_turns, ttl_secs, "task_queue: task enqueued");
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
        reply_to: None,
        timestamp: chrono::Utc::now().timestamp(),
        images: vec![],
        files: vec![],
    };
    manager.submit(session_key, message, priority)
}

// ---------------------------------------------------------------------------
// Outcome classifier
// ---------------------------------------------------------------------------

/// Classify an agent reply to decide whether to auto-continue.
fn classify_outcome(reply: &crate::agent::AgentReply) -> TaskOutcome {
    let text = reply.text.trim();

    // Empty reply — agent produced nothing.
    if text.is_empty() && reply.images.is_empty() && reply.files.is_empty() {
        return TaskOutcome::Stuck("empty reply".to_string());
    }

    let lower = text.to_lowercase();

    // Error patterns from the LLM or infrastructure.
    for pat in [
        "rate limit",
        "rate_limit",
        "quota exceeded",
        "context length exceeded",
        "context_length_exceeded",
        "maximum context",
        "too many tokens",
    ] {
        if lower.contains(pat) {
            return TaskOutcome::Error(pat.to_string());
        }
    }

    // Stuck patterns — agent explicitly says it cannot proceed.
    for pat in [
        "i can't",
        "i cannot",
        "i'm unable",
        "i am unable",
        "i don't know how",
        "i'm not sure how",
        "i need more information",
        "please provide",
        "could you clarify",
        "i'm stuck",
    ] {
        if lower.contains(pat) {
            return TaskOutcome::Stuck(pat.to_string());
        }
    }

    // Partial patterns — agent made progress but signals more work needed.
    for pat in [
        "i'll continue",
        "i will continue",
        "next step",
        "let me continue",
        "continuing",
        "in progress",
        "working on",
        "todo",
        "to be continued",
        "not yet complete",
        "partially done",
    ] {
        if lower.contains(pat) {
            return TaskOutcome::Partial;
        }
    }

    // Default: assume done.
    TaskOutcome::Done
}

/// Build a continuation prompt based on the outcome.
fn continuation_prompt(outcome: &TaskOutcome, turn: u32) -> String {
    match outcome {
        TaskOutcome::Partial => {
            format!("[auto-continue turn {turn}] Continue from where you left off. Complete the remaining work.")
        }
        TaskOutcome::Stuck(reason) => {
            format!(
                "[auto-continue turn {turn}] Previous attempt got stuck ({reason}). \
                 Try a different approach. If truly impossible, explain why \
                 concisely and stop."
            )
        }
        TaskOutcome::Error(err) => {
            format!(
                "[auto-continue turn {turn}] Previous attempt encountered an error: {err}. \
                 Retry or work around it."
            )
        }
        TaskOutcome::Done => String::new(),
    }
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
    config: crate::config::runtime::RuntimeConfig,
}

impl TaskQueueWorker {
    /// Create a new worker.
    pub fn new(
        manager: Arc<TaskQueueManager>,
        registry: Arc<AgentRegistry>,
        channel_senders: Arc<std::sync::RwLock<HashMap<String, mpsc::Sender<OutboundMessage>>>>,
        shutdown: super::shutdown::ShutdownCoordinator,
        config: crate::config::runtime::RuntimeConfig,
    ) -> Self {
        Self {
            manager,
            registry,
            channel_senders,
            shutdown,
            config,
        }
    }

    /// Look up the outbound sender for a channel by name.
    fn channel_tx(&self, name: &str) -> Option<mpsc::Sender<OutboundMessage>> {
        self.channel_senders
            .read()
            .expect("channel_senders lock poisoned")
            .get(name)
            .cloned()
    }

    /// Push a user-facing failure message back through the channel so the
    /// user sees something instead of silence when a turn fails (timeout,
    /// dropped reply, etc). Best-effort: if the channel sender is gone or
    /// the send fails, only logs.
    async fn notify_user_failure(
        &self,
        channel_name: &str,
        target: &str,
        is_group: bool,
        reply_to: Option<String>,
        turn: u32,
        reason: &str,
    ) {
        let Some(tx) = self.channel_tx(channel_name) else {
            warn!(channel = %channel_name, "no channel sender registered, failure notice dropped");
            return;
        };
        // TODO: lookup per-peer language once channels expose a per-target
        // language hint (currently they don't — falls back to gateway-wide).
        let text = crate::i18n::t_fmt(
            "task_notify_failure",
            crate::i18n::default_lang(),
            &[("reason", reason)],
        );
        let out = OutboundMessage {
            target_id: target.to_owned(),
            is_group,
            text,
            reply_to: if turn == 1 { reply_to } else { None },
            images: vec![],
            files: vec![],
            channel: Some(channel_name.to_owned()),
        };
        if let Err(e) = tx.send(out).await {
            error!(channel = %channel_name, "failure notice send failed: {e}");
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

    /// Process a single queued task with auto-continue supervisor loop.
    ///
    /// Each turn: send to agent → classify outcome → route reply → continue
    /// if not done and turns remain. This enables 24/7 autonomous operation
    /// where the agent keeps working until the task is truly complete.
    async fn process_task(&self, task: QueuedTask) {
        let task_id = task.id.clone();
        let session_key = task.session_key.clone();
        let max_turns = task.max_turns;

        // Determine channel + peer + chat from the first message.
        let Some(first_msg) = task.messages.first() else {
            error!(task_id = %task_id, "task queue worker: task has no messages, skipping");
            return;
        };
        let channel_name = first_msg.channel.clone();
        let peer_id = first_msg.sender.clone();
        let chat_id = first_msg.chat_id.clone();
        let is_group = first_msg.is_group;
        let reply_to = first_msg.reply_to.clone();

        info!(
            task_id = %task_id,
            session_key = %session_key,
            channel = %channel_name,
            messages = task.messages.len(),
            max_turns,
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

        // First turn: use the original merged text + attachments.
        let first_text = task.merged_text();
        let first_images: Vec<ImageAttachment> = task
            .messages
            .iter()
            .flat_map(|m| {
                m.images.iter().map(|data| ImageAttachment {
                    data: data.clone(),
                    mime_type: "image/png".to_string(),
                })
            })
            .collect();
        let first_files: Vec<FileAttachment> = task
            .messages
            .iter()
            .flat_map(|m| m.files.iter().map(unstage_file))
            .collect();

        let target = if chat_id.is_empty() { peer_id.clone() } else { chat_id.clone() };
        let mut turn: u32 = 0;
        let mut next_text = first_text;
        let mut next_images = first_images;
        let mut next_files = first_files;

        loop {
            turn += 1;

            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let msg = AgentMessage {
                session_key: session_key.clone(),
                text: next_text,
                channel: channel_name.clone(),
                peer_id: peer_id.clone(),
                chat_id: chat_id.clone(),
                reply_tx,
                extra_tools: vec![],
                images: next_images,
                files: next_files,
            };

            info!(task_id = %task_id, turn, "task queue worker: agent turn");

            if handle.tx.send(msg).await.is_err() {
                error!(task_id = %task_id, "task queue worker: agent channel closed");
                if let Err(fe) = self.manager.fail(&task_id, "agent channel closed", task.max_retries) {
                    error!(task_id = %task_id, "task queue worker: fail() error: {fe:#}");
                }
                break;
            }

            // Wait for reply (10 min timeout per turn).
            let reply = match tokio::time::timeout(Duration::from_secs(600), reply_rx).await {
                Ok(Ok(r)) => r,
                Ok(Err(_)) => {
                    error!(task_id = %task_id, turn, "task queue worker: reply channel dropped");
                    self.notify_user_failure(&channel_name, &target, is_group, reply_to.clone(), turn, "reply channel dropped").await;
                    match self.manager.fail(&task_id, "reply channel dropped", task.max_retries) {
                        Ok(TaskStatus::Dead) => cleanup_staged_files(&task),
                        Err(fe) => error!(task_id = %task_id, "fail() error: {fe:#}"),
                        _ => {}
                    }
                    break;
                }
                Err(_) => {
                    error!(task_id = %task_id, turn, "task queue worker: reply timeout (600s)");
                    self.notify_user_failure(&channel_name, &target, is_group, reply_to.clone(), turn, "reply timeout (600s)").await;
                    match self.manager.fail(&task_id, "reply timeout", task.max_retries) {
                        Ok(TaskStatus::Dead) => cleanup_staged_files(&task),
                        Err(fe) => error!(task_id = %task_id, "fail() error: {fe:#}"),
                        _ => {}
                    }
                    break;
                }
            };

            // Classify outcome before moving fields out of reply.
            let outcome = classify_outcome(&reply);
            let pending = reply.pending_analysis;

            // Route reply to user (every turn, so they see progress).
            if !reply.text.is_empty() || !reply.images.is_empty() || !reply.files.is_empty() {
                let out = OutboundMessage {
                    target_id: target.clone(),
                    is_group,
                    text: reply.text.clone(),
                    reply_to: if turn == 1 { reply_to.clone() } else { None },
                    images: reply.images.clone(),
                    files: reply.files.clone(),
                    channel: Some(channel_name.clone()),
                };
                if let Some(tx) = self.channel_tx(&channel_name) {
                    if let Err(e) = tx.send(out).await {
                        error!(task_id = %task_id, "send reply failed: {e}");
                    }
                } else {
                    tracing::warn!(
                        task_id = %task_id,
                        channel = %channel_name,
                        "no channel sender registered, reply dropped"
                    );
                }
            }

            // Handle pending analysis.
            if let Some(analysis) = pending {
                if let Some(tx) = self.channel_tx(&channel_name) {
                    crate::gateway::startup::handle_pending_analysis(
                        analysis,
                        Arc::clone(&handle),
                        &tx,
                        target.clone(),
                        is_group,
                        &self.config,
                    )
                    .await;
                }
            }

            info!(task_id = %task_id, turn, outcome = ?outcome, "task queue worker: turn outcome");

            match outcome {
                TaskOutcome::Done => {
                    info!(task_id = %task_id, turn, "task queue worker: task completed");
                    if let Err(e) = self.manager.complete(&task_id) {
                        error!(task_id = %task_id, "complete() error: {e:#}");
                    }
                    cleanup_staged_files(&task);
                    break;
                }
                TaskOutcome::Partial | TaskOutcome::Stuck(_) | TaskOutcome::Error(_) => {
                    if max_turns == 0 || turn >= max_turns {
                        info!(
                            task_id = %task_id, turn, max_turns,
                            "task queue worker: max turns reached, marking done"
                        );
                        if let Err(e) = self.manager.complete(&task_id) {
                            error!(task_id = %task_id, "complete() error: {e:#}");
                        }
                        cleanup_staged_files(&task);
                        break;
                    }

                    // Auto-continue: send continuation prompt to the same session.
                    let prompt = continuation_prompt(&outcome, turn);
                    info!(task_id = %task_id, turn, "task queue worker: auto-continue");
                    next_text = prompt;
                    next_images = vec![];
                    next_files = vec![];
                    // Small delay before retry to avoid tight loops on errors.
                    if matches!(outcome, TaskOutcome::Error(_)) {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        }
    }
}
