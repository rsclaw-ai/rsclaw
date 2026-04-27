//! Persistent task queue with priority, dedup, merge, and TTL.
//!
//! When an inbound message arrives while the agent is busy, it is enqueued
//! here and processed in priority order (System > Cron > User, FIFO within
//! the same priority level).

use std::{
    collections::HashMap,
    sync::{Arc, OnceLock, RwLock},
    time::Duration,
};

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
    /// Whether the final reply was confirmed delivered to the channel. Used
    /// by WS reconnect (and any other channel that re-attaches) to replay
    /// completions that fired while the client was offline.
    #[serde(default)]
    pub notified: bool,
    /// Most recent agent reply text — captured per turn so reconnect-replay
    /// can re-deliver the answer without consulting the chat-history index.
    #[serde(default)]
    pub last_reply: Option<String>,
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
            notified: false,
            last_reply: None,
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

/// Parse `/task` prefix and extract turn/timeout flags.
///
/// Supports two flag forms:
///   * Long: `--turns N` / `--timeout Xh`
///   * Short: `-n N` / `-t Xh`  (avoids autocorrect on chat clients that
///     replace `--` with an em-dash, e.g. Feishu/WeChat)
///
/// Em-dash and en-dash characters are normalized to `--` before parsing,
/// so `—turns 10` (auto-corrected by the chat client) still works.
///
/// Returns `(max_turns, ttl_secs)`. If the text does not start with `/task`,
/// returns `(0, 3600)` (regular chat mode). Modifies `text` in-place to
/// strip the `/task` prefix and flags, leaving only the actual message.
///
/// Examples:
/// - `/task fix the login bug` → turns=10, ttl=3600, text="fix the login bug"
/// - `/task --turns 20 refactor` → turns=20, ttl=3600, text="refactor"
/// - `/task -n 20 refactor` → turns=20, ttl=3600, text="refactor"
/// - `/task -n 50 -t 8h x` → turns=50, ttl=28800, text="x"
/// - `hello` → turns=0, ttl=3600, text unchanged
fn parse_task_prefix(text: &mut String) -> (u32, u64) {
    // Defensive: chat clients (Feishu/WeChat) often replace ASCII `--` with
    // an em-dash on send. Normalize em/en/figure-dashes back so flag parsing
    // stays robust regardless of the source client.
    let normalized: String = text
        .replace('\u{2014}', "--") // EM DASH
        .replace('\u{2013}', "--") // EN DASH
        .replace('\u{2012}', "--") // FIGURE DASH
        .replace('\u{2015}', "--"); // HORIZONTAL BAR
    let trimmed = normalized.trim();
    if !trimmed.starts_with("/task ") && trimmed != "/task" {
        // Natural language detection: if it looks like a task, auto-enable.
        if looks_like_task(trimmed) {
            *text = normalized;
            return (TASK_DEFAULT_MAX_TURNS, TASK_DEFAULT_TTL_SECS);
        }
        *text = normalized;
        return (0, TASK_DEFAULT_TTL_SECS);
    }

    // Strip "/task" prefix and tokenize the remainder.
    let rest = trimmed.strip_prefix("/task").unwrap_or(trimmed).trim();
    let mut max_turns = TASK_DEFAULT_MAX_TURNS;
    let mut ttl_secs = TASK_DEFAULT_TTL_SECS;
    let mut msg_parts: Vec<&str> = Vec::new();
    let mut iter = rest.split_whitespace().peekable();
    while let Some(tok) = iter.next() {
        match tok {
            "--turns" | "-n" => {
                if let Some(val) = iter.peek().and_then(|v| v.parse::<u32>().ok()) {
                    max_turns = val;
                    iter.next();
                    continue;
                }
                msg_parts.push(tok);
            }
            "--timeout" | "-t" => {
                if let Some(val) = iter.peek().and_then(|v| parse_duration_str(v)) {
                    ttl_secs = val;
                    iter.next();
                    continue;
                }
                msg_parts.push(tok);
            }
            _ => msg_parts.push(tok),
        }
    }

    *text = msg_parts.join(" ");
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

// ---------------------------------------------------------------------------
// Cross-module channel senders registry
// ---------------------------------------------------------------------------
//
// Lets non-worker code (e.g. submit() acks) deliver messages back through the
// originating channel without threading the senders map through the manager
// constructor. Populated once at gateway startup with the same Arc the worker
// uses.

type ChannelSendersMap = Arc<RwLock<HashMap<String, mpsc::Sender<OutboundMessage>>>>;
static CHANNEL_SENDERS: OnceLock<ChannelSendersMap> = OnceLock::new();

/// Install the channel senders map. Called once at gateway startup.
/// Subsequent installs are silently ignored (idempotent).
pub fn install_channel_senders(senders: ChannelSendersMap) {
    if CHANNEL_SENDERS.set(senders).is_err() {
        warn!("task_queue: channel senders already installed, ignoring duplicate install");
    }
}

/// Look up the outbound mpsc sender for a channel by name. Returns None if
/// the channel is not registered (or `install_channel_senders` was never
/// called).
fn lookup_channel_sender(name: &str) -> Option<mpsc::Sender<OutboundMessage>> {
    CHANNEL_SENDERS
        .get()?
        .read()
        .ok()?
        .get(name)
        .cloned()
}

/// Format a localized "task received" ack string.
fn task_ack_text(task_id: &str, max_turns: u32, ttl_secs: u64, lang: &str) -> String {
    // Render ttl as Xh / Xm — keeps the line short.
    let ttl_human = if ttl_secs >= 3600 && ttl_secs % 3600 == 0 {
        format!("{}h", ttl_secs / 3600)
    } else if ttl_secs >= 60 && ttl_secs % 60 == 0 {
        format!("{}m", ttl_secs / 60)
    } else {
        format!("{ttl_secs}s")
    };
    if lang == "zh" {
        format!(
            "任务已收到，开始处理（最多 {max_turns} 轮，超时 {ttl_human}）\nID: {task_id}\n中止: /abort"
        )
    } else {
        format!(
            "Task received, working on it (up to {max_turns} turns, timeout {ttl_human})\nID: {task_id}\nAbort: /abort"
        )
    }
}

/// Best-effort ack delivery for a freshly enqueued task-mode message.
/// Uses `try_send` so a saturated channel buffer never blocks the submit()
/// fast path; if the channel sender is missing or full, the ack is dropped
/// and a warning is logged.
fn send_task_ack(task: &QueuedTask, max_turns: u32, ttl_secs: u64) {
    let Some(msg) = task.messages.first() else { return };
    let Some(tx) = lookup_channel_sender(&msg.channel) else {
        warn!(channel = %msg.channel, task_id = %task.id, "task_queue: channel sender not registered, ack dropped");
        return;
    };
    let lang = crate::i18n::default_lang();
    let ack = OutboundMessage {
        target_id: msg.chat_id.clone(),
        is_group: msg.is_group,
        text: task_ack_text(&task.id, max_turns, ttl_secs, lang),
        reply_to: msg.reply_to.clone(),
        images: vec![],
        files: vec![],
        channel: Some(msg.channel.clone()),
    };
    if let Err(e) = tx.try_send(ack) {
        warn!(channel = %msg.channel, task_id = %task.id, error = %e, "task_queue: ack send failed");
    }
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
            // User-facing ack: tell them the long-running task was accepted
            // and give them the id so they can /abort or /status it.
            send_task_ack(&task, max_turns, ttl_secs);
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
        send_task_ack(&task, max_turns, ttl_secs);
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

    /// Crash-recovery sweep — call once at worker startup. Any task left in
    /// `Running` from a previous process is moved back to `Pending` so it
    /// can be re-dispatched.
    pub fn recover_orphan_tasks(&self) -> Result<usize> {
        self.store.requeue_running_tasks()
    }

    /// Mark a task's final reply as delivered (for reconnect-replay tracking).
    pub fn mark_notified(&self, task_id: &str) -> Result<()> {
        self.store.mark_task_notified(task_id)
    }

    /// Persist the most recent agent reply on a task so reconnect-replay
    /// can re-deliver it.
    pub fn record_last_reply(&self, task_id: &str, text: &str) -> Result<()> {
        self.store.update_task_last_reply(task_id, text)
    }

    /// Persist the per-turn counter so a /task resumed after a crash starts
    /// from the next turn instead of replaying earlier ones.
    pub fn record_turn(&self, task_id: &str, turn: u32) -> Result<()> {
        self.store.update_task_turn(task_id, turn)
    }

    /// Whether `key` has already been recorded as delivered. Used by the
    /// worker to skip re-sending a turn's reply after a crash-resume.
    pub fn is_idem_delivered(&self, key: &str) -> Result<bool> {
        self.store.is_idem_delivered(key)
    }

    /// Record a successful side-effect under `key` so a subsequent
    /// crash-resume can skip it.
    pub fn mark_idem_delivered(&self, key: &str) -> Result<()> {
        self.store.mark_idem_delivered(key)
    }

    /// Drop idempotency keys older than `retention_secs`. Returns count
    /// removed.
    pub fn cleanup_idem_keys(&self, retention_secs: i64) -> Result<usize> {
        self.store.cleanup_idem_keys(retention_secs)
    }

    /// List Done tasks for a session whose final reply has not yet been
    /// confirmed delivered. Used by WS subscribe to replay completions that
    /// fired while the client was offline.
    pub fn list_pending_notifications(
        &self,
        session_key: &str,
    ) -> Result<Vec<QueuedTask>> {
        let mut all = self.store.list_tasks(Some(TaskStatus::Done))?;
        all.retain(|t| t.session_key == session_key && !t.notified);
        Ok(all)
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
        match self.manager.recover_orphan_tasks() {
            Ok(0) => {}
            Ok(n) => info!(count = n, "task queue worker: revived orphan Running tasks → Pending"),
            Err(e) => error!("task queue worker: orphan recovery failed: {e:#}"),
        }
        // Idempotency-key retention: anything older than 24h is safe to
        // drop — a real crash-resume completes on the next tick, not a day
        // later. Counter ticks each idle/active iteration; ~720 ticks at
        // the 5s fallback floor → roughly hourly cleanup.
        let mut idem_gc_counter: u32 = 0;
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

            idem_gc_counter = idem_gc_counter.wrapping_add(1);
            if idem_gc_counter % 720 == 0 {
                match self.manager.cleanup_idem_keys(24 * 3600) {
                    Ok(0) => {}
                    Ok(n) => info!(count = n, "task queue worker: cleaned old idem keys"),
                    Err(e) => warn!("task queue worker: idem cleanup failed: {e:#}"),
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
        // Resume from the persisted turn counter — non-zero only when this
        // task is being re-picked up after a crash (requeue_running_tasks
        // moved it back to Pending). Fresh tasks start at 0.
        let mut turn: u32 = task.turns;
        if turn > 0 {
            info!(task_id = %task_id, resume_turn = turn, "task queue worker: resuming /task after recovery");
        }
        let mut next_text = first_text;
        let mut next_images = first_images;
        let mut next_files = first_files;
        // Tracks whether the latest reply made it to the channel; consulted
        // when the loop terminates so we only mark `notified=true` if the
        // user actually got the final answer.
        let mut last_send_ok = false;

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

            // Wait for reply (45 min per turn). Long enough to cover the
            // worst observed jimeng video flow: ~30 min queue wait + ~10
            // min actual generation + downloads/sends. Setting it lower
            // would kill the agent mid-task while the upstream provider
            // is still working, and the user doesn't know the partial
            // result happened. Lowering this knob is fine for deploys
            // that don't run video gen, but the default has to cover
            // it because that's our largest legitimate per-turn wait.
            let reply = match tokio::time::timeout(Duration::from_secs(2700), reply_rx).await {
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
                    error!(task_id = %task_id, turn, "task queue worker: reply timeout (2700s)");
                    self.notify_user_failure(&channel_name, &target, is_group, reply_to.clone(), turn, "reply timeout (45m)").await;
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
            let had_reply_payload = !reply.text.is_empty()
                || !reply.images.is_empty()
                || !reply.files.is_empty();
            if !reply.text.is_empty() {
                if let Err(e) = self.manager.record_last_reply(&task_id, &reply.text) {
                    tracing::warn!(task_id = %task_id, "record_last_reply failed: {e:#}");
                }
            }
            if had_reply_payload {
                // Idempotency: a previous run of THIS turn may have already
                // delivered to the channel before the gateway crashed. The
                // post-crash requeue resumes at the same turn and runs the
                // LLM again — but we must not re-send to the user.
                let idem_key = format!("task:{task_id}:turn:{turn}");
                let already_delivered = match self.manager.is_idem_delivered(&idem_key) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(task_id = %task_id, "is_idem_delivered failed: {e:#}");
                        false
                    }
                };
                if already_delivered {
                    info!(
                        task_id = %task_id, turn,
                        "task queue worker: turn reply already delivered, skipping channel send"
                    );
                    last_send_ok = true;
                } else {
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
                        match tx.send(out).await {
                            Ok(_) => {
                                last_send_ok = true;
                                if let Err(e) = self.manager.mark_idem_delivered(&idem_key) {
                                    warn!(task_id = %task_id, "mark_idem_delivered failed: {e:#}");
                                }
                            }
                            Err(e) => {
                                last_send_ok = false;
                                error!(task_id = %task_id, "send reply failed: {e}");
                            }
                        }
                    } else {
                        last_send_ok = false;
                        tracing::warn!(
                            task_id = %task_id,
                            channel = %channel_name,
                            "no channel sender registered, reply dropped"
                        );
                    }
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

            // Persist turn counter so a crash mid-/task resumes from the
            // right place rather than replaying earlier turns.
            if let Err(e) = self.manager.record_turn(&task_id, turn) {
                tracing::warn!(task_id = %task_id, "record_turn failed: {e:#}");
            }

            match outcome {
                TaskOutcome::Done => {
                    info!(task_id = %task_id, turn, "task queue worker: task completed");
                    if let Err(e) = self.manager.complete(&task_id) {
                        error!(task_id = %task_id, "complete() error: {e:#}");
                    }
                    if last_send_ok {
                        if let Err(e) = self.manager.mark_notified(&task_id) {
                            error!(task_id = %task_id, "mark_notified() error: {e:#}");
                        }
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
                        if last_send_ok {
                            if let Err(e) = self.manager.mark_notified(&task_id) {
                                error!(task_id = %task_id, "mark_notified() error: {e:#}");
                            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_task_prefix_short_flags() {
        let mut text = "/task -n 20 fix the login bug".to_string();
        let (turns, ttl) = parse_task_prefix(&mut text);
        assert_eq!(turns, 20);
        assert_eq!(ttl, TASK_DEFAULT_TTL_SECS);
        assert_eq!(text, "fix the login bug");
    }

    #[test]
    fn parse_task_prefix_short_flags_combined() {
        let mut text = "/task -n 50 -t 4h refactor payments".to_string();
        let (turns, ttl) = parse_task_prefix(&mut text);
        assert_eq!(turns, 50);
        assert_eq!(ttl, 4 * 3600);
        assert_eq!(text, "refactor payments");
    }

    #[test]
    fn parse_task_prefix_long_flags_still_work() {
        let mut text = "/task --turns 30 --timeout 2h work".to_string();
        let (turns, ttl) = parse_task_prefix(&mut text);
        assert_eq!(turns, 30);
        assert_eq!(ttl, 2 * 3600);
        assert_eq!(text, "work");
    }

    #[test]
    fn parse_task_prefix_em_dash_normalized() {
        // Feishu/WeChat autocorrect `--` to em-dash. Result must still parse.
        let mut text = "/task \u{2014}turns 25 \u{2014}timeout 30m do x".to_string();
        let (turns, ttl) = parse_task_prefix(&mut text);
        assert_eq!(turns, 25);
        assert_eq!(ttl, 30 * 60);
        assert_eq!(text, "do x");
    }

    #[test]
    fn parse_task_prefix_no_task_prefix_chat_mode() {
        let mut text = "hello there".to_string();
        let (turns, _ttl) = parse_task_prefix(&mut text);
        assert_eq!(turns, 0);
    }

    #[test]
    fn parse_task_prefix_n_without_value_kept_as_text() {
        // `-n` not followed by a number must not consume the next token.
        let mut text = "/task -n investigate logs".to_string();
        let (turns, _ttl) = parse_task_prefix(&mut text);
        assert_eq!(turns, TASK_DEFAULT_MAX_TURNS);
        assert_eq!(text, "-n investigate logs");
    }
}
