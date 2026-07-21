//! Persistent task queue with priority, dedup, merge, and TTL.
//!
//! When an inbound message arrives while the agent is busy, it is enqueued
//! here and processed in priority order (System > Cron > User, FIFO within
//! the same priority level).

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, OnceLock, RwLock},
    time::Duration,
};

use anyhow::Result;
use futures::StreamExt as _;
use md5::{Digest, Md5};
use rsclaw_agent::{AgentMessage, AgentRegistry, FileAttachment, ImageAttachment};
use rsclaw_channel::OutboundMessage;
use rsclaw_store::redb_store::RedbStore;
// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

// Records lifted to rsclaw-types (crate-split); re-exported.
pub use rsclaw_types::{
    Priority, QueuedFile, QueuedMessage, QueuedTask, TaskStatus, compute_hash, default_max_turns,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Outcome of a single agent turn, used by the auto-continue supervisor.
///
/// `Done` / `Partial` / `Stuck` / `Error` are produced by the legacy
/// string-matching `classify_outcome` fallback. `Structured` carries an
/// agent-declared outcome from the `task_finish` tool and takes precedence
/// when present. `NeedsInput` lets the agent surface a clarifying question
/// to the user without triggering auto-continue.
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
    /// Agent self-reported via `task_finish` tool. Replaces string-matching.
    Structured(StructuredOutcome),
    /// Agent explicitly asked the user for input. Worker should NOT
    /// auto-continue; the question is surfaced back to the channel.
    NeedsInput(String),
}

// StructuredOutcome/Completion/Recommend lifted to rsclaw-types (crate-split).
pub use rsclaw_types::{Completion, Recommend, SkipEntry, StructuredOutcome};

/// What the worker should do after grading a turn's outcome.
///
/// Extracted as a pure function ([`decide_action`]) so the worker's giant
/// run loop can stay thin and the routing matrix is unit-testable in
/// isolation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchAction {
    /// Mark the task `Done` (success path), close, deliver to user.
    Complete,
    /// Mark the task `Failed`. Used for `Recommend::Abandon` and for
    /// `Recommend::Retry` when no turn budget remains.
    Fail,
    /// Continue the same task with the given continuation prompt as next
    /// agent input. Used for `Partial` / `Stuck` / `Error` / `Retry`.
    AutoContinue { prompt: String, slow: bool },
    /// Spawn each task description as a new queued task, then mark the
    /// current task `Done`. Used for `Recommend::Continue` with
    /// `follow_up_tasks` populated.
    Spawn { tasks: Vec<String> },
}

/// Pure decision function: given an outcome and the turn-budget state, what
/// should the worker do? Lifted out of `TaskQueueWorker::run` for testability.
pub fn decide_action(outcome: &TaskOutcome, turn: u32, max_turns: u32) -> DispatchAction {
    let at_max = max_turns == 0 || turn >= max_turns;

    match outcome {
        TaskOutcome::Done => DispatchAction::Complete,

        // Structured outcome routes by the agent's own `recommend` field.
        TaskOutcome::Structured(out) => match out.recommend {
            // Ship: standard completion. NeedsHuman: also terminal — the
            // agent's text reply already contains the blocker question; the
            // user's next message resumes naturally as a fresh task.
            Recommend::Ship | Recommend::NeedsHuman => DispatchAction::Complete,
            // Abandon: agent gave up; mark Failed so retry/replay paths skip it.
            Recommend::Abandon => DispatchAction::Fail,
            Recommend::Retry => {
                if at_max {
                    DispatchAction::Fail
                } else {
                    DispatchAction::AutoContinue {
                        prompt: format!(
                            "[auto-continue turn {turn}] Retry the task — \
                             your previous attempt asked for a fresh retry. \
                             Try again, change something if you can."
                        ),
                        slow: true, // brief delay to avoid tight retry loops
                    }
                }
            }
            Recommend::Continue => {
                if out.follow_up_tasks.is_empty() {
                    // recommend=continue but no follow-ups specified — treat
                    // as Ship rather than wedge the task open.
                    DispatchAction::Complete
                } else {
                    DispatchAction::Spawn {
                        tasks: out.follow_up_tasks.clone(),
                    }
                }
            }
        },

        // Agent explicitly asked the user — its reply already carries the
        // question, complete the task and let the user's reply start a
        // fresh inbound message.
        TaskOutcome::NeedsInput(_) => DispatchAction::Complete,

        // Legacy string-classifier path: auto-continue until max turns.
        TaskOutcome::Partial | TaskOutcome::Stuck(_) | TaskOutcome::Error(_) => {
            if at_max {
                DispatchAction::Complete
            } else {
                DispatchAction::AutoContinue {
                    prompt: continuation_prompt(outcome, turn),
                    slow: matches!(outcome, TaskOutcome::Error(_)),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pending-outcome stash
// ---------------------------------------------------------------------------
//
// Bridge between the `task_finish` tool (called from agent runtime, no direct
// access to the queue worker) and the auto-continue supervisor in
// `TaskQueueWorker::run`. The tool stages an outcome under the session key;
// the worker drains the slot after each turn and converts it into
// `TaskOutcome::Structured`, taking precedence over the string classifier.
//
// `OnceLock<Mutex<HashMap>>` is intentional — no DashMap dependency, and the
// contention profile (one writer per turn per session) doesn't warrant it.

// pending-outcome staging map lifted to rsclaw-types (crate-split);
// re-exported.
/// Default max turns for /task mode.
pub use rsclaw_types::{TASK_DEFAULT_MAX_TURNS, TASK_DEFAULT_TTL_SECS};
pub use rsclaw_types::{drain_pending_outcome, stage_pending_outcome};

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
    // EM / EN / FIGURE / HORIZONTAL dashes — all collapse to ASCII "--".
    let normalized: String = text.replace(['\u{2014}', '\u{2013}', '\u{2012}', '\u{2015}'], "--");
    let trimmed = normalized.trim();
    if !trimmed.starts_with("/task ") && trimmed != "/task" {
        // No keyword auto-detection here — that path mistook short Chinese
        // questions like "你可以帮我做啥？" for task requests because
        // `text.len() < 15` was a byte length and "帮我" matched. Decision
        // is now delegated to the LLM via the `task` function-call tool;
        // see agent::tools_builder. Only the explicit `/task` prefix
        // bypasses the LLM judgement.
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
static TASK_QUEUE: OnceLock<Arc<TaskQueueManager>> = OnceLock::new();
static PLUGIN_BACKGROUND_KEYS: OnceLock<Arc<RwLock<HashSet<String>>>> = OnceLock::new();
static PLUGIN_SSE_TOKENS: OnceLock<Arc<RwLock<HashMap<String, CancellationToken>>>> =
    OnceLock::new();

fn plugin_background_keys() -> Arc<RwLock<HashSet<String>>> {
    PLUGIN_BACKGROUND_KEYS
        .get_or_init(|| Arc::new(RwLock::new(HashSet::new())))
        .clone()
}

fn plugin_sse_tokens() -> Arc<RwLock<HashMap<String, CancellationToken>>> {
    PLUGIN_SSE_TOKENS
        .get_or_init(|| Arc::new(RwLock::new(HashMap::new())))
        .clone()
}

/// Install the channel senders map. Called once at gateway startup.
/// Subsequent installs are silently ignored (idempotent).
pub fn install_channel_senders(senders: ChannelSendersMap) {
    if CHANNEL_SENDERS.set(senders).is_err() {
        warn!("task_queue: channel senders already installed, ignoring duplicate install");
    }
}

/// Install the global TaskQueueManager handle. Lets the agent runtime's
/// `task` function-call tool submit new tasks without threading the
/// manager Arc through every tool-dispatch surface.
pub fn install_task_queue(manager: Arc<TaskQueueManager>) {
    if TASK_QUEUE.set(manager).is_err() {
        warn!("task_queue: manager already installed, ignoring duplicate install");
    }
}

/// Get the installed TaskQueueManager Arc, if any.
pub fn get_task_queue() -> Option<Arc<TaskQueueManager>> {
    TASK_QUEUE.get().cloned()
}

/// Look up the outbound mpsc sender for a channel.
///
/// When `account` is `Some`, the account-suffixed key `{name}/{account}` is
/// tried first so multi-account channels (feishu) route replies back through
/// the originating account instead of whichever one registered the bare
/// `{name}` key last. Falls back to the bare `{name}` key. Returns `None` if
/// the channel is not registered (or `install_channel_senders` was never
/// called).
/// Push an outbound message directly to a channel without going through
/// the task queue. Used by background subsystems (cron briefings, plugin
/// SSE bridge, etc.) that have a pre-formatted message and don't need
/// an LLM turn to produce it. Falls back to bare `{name}` lookup when
/// `account` is `None` or absent from the sender map.
///
/// Best-effort via `try_send` so a saturated channel buffer never blocks
/// the caller's hot path; if delivery fails, returns the error and lets
/// the caller decide whether to retry or drop. Same semantics as
/// `send_task_ack`.
pub fn push_outbound(
    channel: &str,
    account: Option<&str>,
    msg: OutboundMessage,
) -> Result<(), String> {
    let tx = lookup_channel_sender_for(channel, account)
        .ok_or_else(|| format!("channel sender '{channel}' not registered"))?;
    tx.try_send(msg)
        .map_err(|e| format!("channel '{channel}' send failed: {e}"))
}

fn lookup_channel_sender_for(
    name: &str,
    account: Option<&str>,
) -> Option<mpsc::Sender<OutboundMessage>> {
    let map = CHANNEL_SENDERS.get()?.read().ok()?;
    if let Some(acct) = account.filter(|s| !s.is_empty()) {
        let key = format!("{name}/{acct}");
        if let Some(tx) = map.get(&key).cloned() {
            return Some(tx);
        }
    }
    map.get(name).cloned()
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
    let Some(msg) = task.messages.first() else {
        return;
    };
    let Some(tx) = lookup_channel_sender_for(&msg.channel, msg.account.as_deref()) else {
        warn!(channel = %msg.channel, task_id = %task.id, "task_queue: channel sender not registered, ack dropped");
        return;
    };
    let lang = rsclaw_i18n::default_lang();
    let ack = OutboundMessage {
        target_id: msg.chat_id.clone(),
        is_group: msg.is_group,
        text: task_ack_text(&task.id, max_turns, ttl_secs, lang),
        reply_to: msg.reply_to.clone(),
        images: vec![],
        files: vec![],
        channel: Some(msg.channel.clone()),
        account: msg.account.clone(),
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
    /// also picks up tasks that were persisted before the current process
    /// started.
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
    pub fn list_pending_notifications(&self, session_key: &str) -> Result<Vec<QueuedTask>> {
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
            pending: all
                .iter()
                .filter(|t| t.status == TaskStatus::Pending)
                .count(),
            running: all
                .iter()
                .filter(|t| t.status == TaskStatus::Running)
                .count(),
            done: all.iter().filter(|t| t.status == TaskStatus::Done).count(),
            failed: all
                .iter()
                .filter(|t| t.status == TaskStatus::Failed)
                .count(),
            dead: all.iter().filter(|t| t.status == TaskStatus::Dead).count(),
        })
    }
}

// ---------------------------------------------------------------------------
// File staging
// ---------------------------------------------------------------------------

/// Return the staging directory for queue file attachments.
fn staging_dir() -> std::path::PathBuf {
    rsclaw_config::loader::base_dir().join("var/data/queue/staging")
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
        account: None,
    };
    manager.submit(session_key, message, priority)
}

// ---------------------------------------------------------------------------
// Outcome classifier
// ---------------------------------------------------------------------------

/// Classify an agent reply to decide whether to auto-continue.
fn classify_outcome(reply: &rsclaw_agent::AgentReply) -> TaskOutcome {
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
    // English uses lower-cased `lower`; Chinese keywords are matched on the
    // original `text` because lowercasing CJK is a no-op anyway.
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
    for pat in [
        "无法完成",
        "做不到",
        "我没法",
        "我不知道怎么",
        "需要更多信息",
        "请提供",
        "请告诉我",
        "卡住了",
        "我不太确定",
    ] {
        if text.contains(pat) {
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
    for pat in [
        "继续",
        "下一步",
        "未完成",
        "还需要",
        "稍后",
        "进行中",
        "正在",
        "待办",
        "尚未完成",
        "部分完成",
    ] {
        if text.contains(pat) {
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
            format!(
                "[auto-continue turn {turn}] Continue from where you left off. Complete the remaining work."
            )
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
        // Structured / NeedsInput cannot be produced by the current
        // `classify_outcome` path; they appear only once `task_finish` wiring
        // lands. Return empty so the worker treats them as no-continuation
        // (terminal) for now.
        TaskOutcome::Structured(_) | TaskOutcome::NeedsInput(_) => String::new(),
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
    config: rsclaw_config::runtime::RuntimeConfig,
}

impl TaskQueueWorker {
    /// Create a new worker.
    pub fn new(
        manager: Arc<TaskQueueManager>,
        registry: Arc<AgentRegistry>,
        channel_senders: Arc<std::sync::RwLock<HashMap<String, mpsc::Sender<OutboundMessage>>>>,
        shutdown: super::shutdown::ShutdownCoordinator,
        config: rsclaw_config::runtime::RuntimeConfig,
    ) -> Self {
        Self {
            manager,
            registry,
            channel_senders,
            shutdown,
            config,
        }
    }

    /// Look up the outbound sender, preferring the account-suffixed key
    /// `{channel}/{account}` when an account tag is present. Multi-account
    /// channels (e.g. feishu) register both `feishu` (legacy) and
    /// `feishu/<acct>` keys; without the account-aware lookup, the bare key
    /// is overwritten by whichever account starts last and replies get sent
    /// via the wrong app token (Feishu rejects with 230002).
    fn channel_tx_for(
        &self,
        name: &str,
        account: Option<&str>,
    ) -> Option<mpsc::Sender<OutboundMessage>> {
        if let Some(acct) = account.filter(|s| !s.is_empty()) {
            let key = format!("{name}/{acct}");
            if let Some(tx) = self
                .channel_senders
                .read()
                .ok()
                .and_then(|map| map.get(&key).cloned())
            {
                return Some(tx);
            }
        }
        self.channel_tx(name)
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
        account: Option<&str>,
        target: &str,
        is_group: bool,
        reply_to: Option<String>,
        turn: u32,
        reason: &str,
    ) {
        let Some(tx) = self.channel_tx_for(channel_name, account) else {
            warn!(channel = %channel_name, "no channel sender registered, failure notice dropped");
            return;
        };
        // TODO: lookup per-peer language once channels expose a per-target
        // language hint (currently they don't — falls back to gateway-wide).
        let text = rsclaw_i18n::t_fmt(
            "task_notify_failure",
            rsclaw_i18n::default_lang(),
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
            account: account.map(str::to_owned),
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
            Ok(n) => info!(
                count = n,
                "task queue worker: revived orphan Running tasks → Pending"
            ),
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
        let account = first_msg.account.clone();
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
                    if let Err(fe) =
                        self.manager
                            .fail(&task_id, &format!("{e:#}"), task.max_retries)
                    {
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
                    source_path: None,
                })
            })
            .collect();
        let first_files: Vec<FileAttachment> = task
            .messages
            .iter()
            .flat_map(|m| m.files.iter().map(unstage_file))
            .collect();

        let target = if chat_id.is_empty() {
            peer_id.clone()
        } else {
            chat_id.clone()
        };
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
            if self.shutdown.is_draining() {
                info!(task_id = %task_id, turn, "task queue worker: drain signaled, aborting multi-turn loop");
                break;
            }
            turn += 1;

            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let msg = AgentMessage {
                session_key: session_key.clone(),
                text: next_text,
                channel: channel_name.clone(),
                peer_id: peer_id.clone(),
                chat_id: chat_id.clone(),
                reply_tx,
                task_id: None,
                context_id: None,
                event_tx: None,
                cancel_token: None,
                input_request_tx: None,
                extra_tools: vec![],
                images: next_images,
                files: next_files,
                account: account.clone(),
            };

            info!(task_id = %task_id, turn, "task queue worker: agent turn");

            if handle.tx.send(msg).await.is_err() {
                error!(task_id = %task_id, "task queue worker: agent channel closed");
                if let Err(fe) =
                    self.manager
                        .fail(&task_id, "agent channel closed", task.max_retries)
                {
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
                    self.notify_user_failure(
                        &channel_name,
                        account.as_deref(),
                        &target,
                        is_group,
                        reply_to.clone(),
                        turn,
                        "reply channel dropped",
                    )
                    .await;
                    match self
                        .manager
                        .fail(&task_id, "reply channel dropped", task.max_retries)
                    {
                        Ok(TaskStatus::Dead) => cleanup_staged_files(&task),
                        Err(fe) => error!(task_id = %task_id, "fail() error: {fe:#}"),
                        _ => {}
                    }
                    break;
                }
                Err(_) => {
                    error!(task_id = %task_id, turn, "task queue worker: reply timeout (2700s)");
                    self.notify_user_failure(
                        &channel_name,
                        account.as_deref(),
                        &target,
                        is_group,
                        reply_to.clone(),
                        turn,
                        "reply timeout (45m)",
                    )
                    .await;
                    match self
                        .manager
                        .fail(&task_id, "reply timeout", task.max_retries)
                    {
                        Ok(TaskStatus::Dead) => cleanup_staged_files(&task),
                        Err(fe) => error!(task_id = %task_id, "fail() error: {fe:#}"),
                        _ => {}
                    }
                    break;
                }
            };

            // Classify outcome before moving fields out of reply.
            //
            // First check for an agent-declared structured outcome from the
            // `task_finish` tool (staged under the session key). Falling back
            // to the string classifier preserves behaviour for agents that
            // don't (yet) call `task_finish`.
            let outcome = match drain_pending_outcome(&task.session_key) {
                Some(structured) => {
                    info!(
                        task_id = %task_id,
                        completion = ?structured.completion,
                        recommend = ?structured.recommend,
                        "task queue worker: using agent-declared structured outcome"
                    );
                    TaskOutcome::Structured(structured)
                }
                None => classify_outcome(&reply),
            };
            let pending = reply.pending_analysis;

            // Route reply to user (every turn, so they see progress).
            let had_reply_payload =
                !reply.text.is_empty() || !reply.images.is_empty() || !reply.files.is_empty();
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
                        account: account.clone(),
                    };
                    if let Some(tx) = self.channel_tx_for(&channel_name, account.as_deref()) {
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
                if let Some(tx) = self.channel_tx_for(&channel_name, account.as_deref()) {
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

            // Routing matrix lives in `decide_action` so the logic is
            // testable in isolation.
            let action = decide_action(&outcome, turn, max_turns);
            info!(task_id = %task_id, turn, ?action, "task queue worker: action");
            match action {
                DispatchAction::Complete => {
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
                DispatchAction::Fail => {
                    // Agent declared abandon / retry exhausted. Mark Failed
                    // so the queue's retry/replay logic doesn't loop.
                    if let Err(e) = self.manager.fail(&task_id, "agent abandoned", 0) {
                        error!(task_id = %task_id, "fail() error: {e:#}");
                    }
                    if last_send_ok {
                        if let Err(e) = self.manager.mark_notified(&task_id) {
                            error!(task_id = %task_id, "mark_notified() error: {e:#}");
                        }
                    }
                    cleanup_staged_files(&task);
                    break;
                }
                DispatchAction::Spawn { tasks } => {
                    // Recommend::Continue with follow_up_tasks. Spawn each as
                    // a fresh queued task on the same session so the agent
                    // keeps its conversational context, then mark this turn's
                    // task complete.
                    let base = task.messages.first().cloned();
                    let now = chrono::Utc::now().timestamp();
                    let spawned = tasks.len();
                    for follow_up in tasks {
                        let Some(ref base_msg) = base else {
                            warn!(task_id = %task_id, "spawn: no base message to inherit channel from");
                            break;
                        };
                        let msg = QueuedMessage {
                            text: follow_up,
                            sender: format!("{}:follow_up", base_msg.sender),
                            channel: base_msg.channel.clone(),
                            account: base_msg.account.clone(),
                            chat_id: base_msg.chat_id.clone(),
                            is_group: base_msg.is_group,
                            reply_to: None,
                            timestamp: now,
                            images: vec![],
                            files: vec![],
                        };
                        // Inherit budget from the parent task. Use System
                        // priority so follow-ups jump the queue ahead of new
                        // user input (the chain shouldn't get starved).
                        match self.manager.submit_task(
                            &task.session_key,
                            msg,
                            Priority::System,
                            task.max_turns,
                            task.ttl_secs,
                        ) {
                            Ok((new_id, _)) => {
                                info!(parent = %task_id, child = %new_id, "spawn: follow-up enqueued");
                            }
                            Err(e) => {
                                warn!(parent = %task_id, "spawn: submit_task failed: {e:#}");
                            }
                        }
                    }
                    info!(task_id = %task_id, spawned, "spawn: parent task completing");
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
                DispatchAction::AutoContinue { prompt, slow } => {
                    next_text = prompt;
                    next_images = vec![];
                    next_files = vec![];
                    if slow {
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

    // -----------------------------------------------------------------------
    // Structured outcome — schema + dispatch matrix
    // -----------------------------------------------------------------------

    fn make_outcome(completion: Completion, recommend: Recommend) -> StructuredOutcome {
        StructuredOutcome {
            completion,
            recommend,
            verified: false,
            verification_log: None,
            accomplished: vec!["did the thing".into()],
            skipped: vec![],
            blocked_on: vec![],
            assumptions: vec![],
            follow_up_tasks: vec![],
            summary: None,
        }
    }

    #[test]
    fn structured_outcome_serializes_snake_case() {
        // Outcome serializes with snake_case keys so A2A consumers and the
        // task_finish tool schema agree on the wire format.
        let mut out = make_outcome(Completion::Partial, Recommend::Continue);
        out.follow_up_tasks = vec!["task A".into(), "task B".into()];
        out.blocked_on = vec!["disk full".into()];

        let json = serde_json::to_value(&out).expect("serialize");
        assert_eq!(json["completion"], "partial");
        assert_eq!(json["recommend"], "continue");
        assert_eq!(json["follow_up_tasks"][0], "task A");
        assert_eq!(json["blocked_on"][0], "disk full");
    }

    #[test]
    fn pending_outcome_stash_roundtrip() {
        let session = "test:stash:roundtrip";
        // No outcome staged → drain returns None.
        assert!(drain_pending_outcome(session).is_none());

        let outcome = make_outcome(Completion::Full, Recommend::Ship);
        stage_pending_outcome(session, outcome);

        let drained = drain_pending_outcome(session).expect("staged outcome");
        assert_eq!(drained.completion, Completion::Full);
        assert_eq!(drained.recommend, Recommend::Ship);

        // Second drain is empty — drain consumes.
        assert!(drain_pending_outcome(session).is_none());
    }

    #[test]
    fn decide_action_done_completes() {
        assert_eq!(
            decide_action(&TaskOutcome::Done, 1, 10),
            DispatchAction::Complete
        );
    }

    #[test]
    fn decide_action_structured_ship_completes() {
        let outcome = TaskOutcome::Structured(make_outcome(Completion::Full, Recommend::Ship));
        assert_eq!(decide_action(&outcome, 1, 10), DispatchAction::Complete);
    }

    #[test]
    fn decide_action_structured_needs_human_completes() {
        let outcome =
            TaskOutcome::Structured(make_outcome(Completion::Partial, Recommend::NeedsHuman));
        assert_eq!(decide_action(&outcome, 1, 10), DispatchAction::Complete);
    }

    #[test]
    fn decide_action_structured_abandon_fails() {
        let outcome = TaskOutcome::Structured(make_outcome(Completion::Failed, Recommend::Abandon));
        assert_eq!(decide_action(&outcome, 1, 10), DispatchAction::Fail);
    }

    #[test]
    fn decide_action_structured_retry_continues() {
        let outcome = TaskOutcome::Structured(make_outcome(Completion::Minimal, Recommend::Retry));
        match decide_action(&outcome, 2, 10) {
            DispatchAction::AutoContinue { prompt, slow } => {
                assert!(prompt.contains("Retry"));
                assert!(slow, "retry should rate-limit");
            }
            other => panic!("expected AutoContinue, got {other:?}"),
        }
    }

    #[test]
    fn decide_action_structured_retry_at_max_fails() {
        // At the turn budget cap, Retry is downgraded to Fail (we can't
        // retry forever).
        let outcome = TaskOutcome::Structured(make_outcome(Completion::Minimal, Recommend::Retry));
        assert_eq!(decide_action(&outcome, 5, 5), DispatchAction::Fail);
    }

    #[test]
    fn decide_action_structured_continue_with_followups_spawns() {
        let mut out = make_outcome(Completion::Partial, Recommend::Continue);
        out.follow_up_tasks = vec!["step 1".into(), "step 2".into()];
        let outcome = TaskOutcome::Structured(out);
        match decide_action(&outcome, 1, 10) {
            DispatchAction::Spawn { tasks } => {
                assert_eq!(tasks, vec!["step 1".to_string(), "step 2".to_string()]);
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn decide_action_structured_continue_without_followups_completes() {
        // recommend=continue but no follow-ups specified → don't wedge open,
        // treat as Complete.
        let outcome =
            TaskOutcome::Structured(make_outcome(Completion::Partial, Recommend::Continue));
        assert_eq!(decide_action(&outcome, 1, 10), DispatchAction::Complete);
    }

    #[test]
    fn decide_action_needs_input_completes() {
        let outcome = TaskOutcome::NeedsInput("which file?".into());
        assert_eq!(decide_action(&outcome, 1, 10), DispatchAction::Complete);
    }

    #[test]
    fn decide_action_partial_continues_under_budget() {
        match decide_action(&TaskOutcome::Partial, 2, 10) {
            DispatchAction::AutoContinue { prompt, slow } => {
                assert!(prompt.contains("Continue"));
                assert!(!slow, "partial should not rate-limit");
            }
            other => panic!("expected AutoContinue, got {other:?}"),
        }
    }

    #[test]
    fn decide_action_partial_at_max_completes() {
        // Legacy behaviour preserved: at max turns, Partial/Stuck/Error fall
        // back to Complete (deliver whatever the agent has produced).
        assert_eq!(
            decide_action(&TaskOutcome::Partial, 5, 5),
            DispatchAction::Complete
        );
    }

    #[test]
    fn decide_action_error_slow_retry() {
        match decide_action(&TaskOutcome::Error("rate limit".into()), 1, 10) {
            DispatchAction::AutoContinue { slow, .. } => {
                assert!(slow, "Error should rate-limit before retry");
            }
            other => panic!("expected AutoContinue, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // classify_outcome — Chinese keyword coverage
    // -----------------------------------------------------------------------

    fn fake_reply(text: &str) -> rsclaw_agent::AgentReply {
        rsclaw_agent::AgentReply {
            text: text.to_string(),
            is_empty: text.is_empty(),
            tool_calls: None,
            images: vec![],
            files: vec![],
            pending_analysis: None,
            needs_outer_done_emit: false,
            outcome: rsclaw_agent::registry::ReplyOutcome::Ok,
        }
    }

    #[test]
    fn classify_chinese_stuck_phrase() {
        let reply = fake_reply("抱歉，我无法完成这个任务");
        assert!(matches!(classify_outcome(&reply), TaskOutcome::Stuck(_)));
    }

    #[test]
    fn classify_chinese_partial_phrase() {
        let reply = fake_reply("先做了一半，下一步来处理剩下的");
        assert!(matches!(classify_outcome(&reply), TaskOutcome::Partial));
    }

    #[test]
    fn classify_empty_reply_is_stuck() {
        assert!(matches!(
            classify_outcome(&fake_reply("")),
            TaskOutcome::Stuck(_)
        ));
    }

    #[test]
    fn classify_plain_reply_is_done() {
        let reply = fake_reply("Sure, here's the result: 42.");
        assert!(matches!(classify_outcome(&reply), TaskOutcome::Done));
    }
}

/// Root-side implementation of `rsclaw_types::BriefingSink` for trusted
/// plugin background registrations that submit briefings + push outbound
/// through the gateway task queue.
pub struct GatewayBriefingSink;

impl rsclaw_types::BriefingSink for GatewayBriefingSink {
    fn submit_briefing(
        &self,
        session_key: &str,
        text: &str,
        channel: &str,
        peer_id: &str,
        chat_id: &str,
        is_group: bool,
        priority: Priority,
    ) -> anyhow::Result<(String, bool)> {
        let tq = get_task_queue().ok_or_else(|| anyhow::anyhow!("task queue not installed"))?;
        submit_to_queue(
            &tq,
            session_key,
            text,
            channel,
            peer_id,
            chat_id,
            is_group,
            priority,
        )
    }

    fn push_outbound(
        &self,
        channel: &str,
        account: Option<&str>,
        msg: OutboundMessage,
    ) -> Result<(), String> {
        push_outbound(channel, account, msg)
    }
}

/// Root-side implementation of trusted WASM plugin background host methods.
pub struct GatewayPluginBackgroundHost;

impl rsclaw_plugin::PluginBackgroundHost for GatewayPluginBackgroundHost {
    fn cron_register(
        &self,
        plugin: String,
        name: String,
        schedule_json: String,
        ctx: Option<rsclaw_plugin::PluginInvocationContext>,
    ) -> futures::future::BoxFuture<'static, std::result::Result<String, String>> {
        Box::pin(async move {
            let key = plugin_background_key("cron", &plugin, &name, None, ctx.as_ref());
            if !claim_plugin_background_key(&key) {
                return Ok("already_registered".to_owned());
            }
            let schedule: Value = serde_json::from_str(&schedule_json)
                .map_err(|e| format!("plugin cron schedule JSON invalid: {e}"))?;
            let slots = schedule
                .get("slots")
                .and_then(Value::as_object)
                .ok_or_else(|| "plugin cron schedule requires object field `slots`".to_owned())?;
            let prompt_template = schedule
                .get("promptTemplate")
                .and_then(Value::as_str)
                .unwrap_or("Run plugin cron job {plugin}.{name} slot {slot}.")
                .to_owned();
            let session_key = schedule
                .get("sessionKey")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let mut count = 0usize;
            for (slot, hhmm) in slots {
                let Some(hhmm) = hhmm.as_str() else {
                    continue;
                };
                let Some((hour, minute)) = parse_hhmm(hhmm) else {
                    continue;
                };
                let ctx_for_task = ctx.clone();
                let prompt_template = prompt_template.clone();
                let plugin = plugin.clone();
                let name = name.clone();
                let slot = slot.clone();
                let session_key = session_key.clone();
                tokio::spawn(async move {
                    loop {
                        let wait = duration_until_shanghai(hour, minute);
                        tokio::time::sleep(wait).await;
                        let prompt = prompt_template
                            .replace("{plugin}", &plugin)
                            .replace("{name}", &name)
                            .replace("{slot}", &slot);
                        let session = session_key
                            .clone()
                            .or_else(|| ctx_for_task.as_ref().map(|c| c.session_key.clone()))
                            .unwrap_or_else(|| format!("plugin:{plugin}:{name}:{slot}"));
                        if let Err(e) =
                            submit_plugin_agent_turn(&session, &prompt, "{}", ctx_for_task.as_ref())
                        {
                            warn!(plugin, name, slot, error = %e, "plugin cron submit failed");
                        }
                    }
                });
                count += 1;
            }
            Ok(format!("registered {count} cron slot(s)"))
        })
    }

    fn sse_subscribe(
        &self,
        plugin: String,
        name: String,
        url: String,
        headers_json: String,
        resume_key: String,
        ctx: Option<rsclaw_plugin::PluginInvocationContext>,
    ) -> futures::future::BoxFuture<'static, std::result::Result<String, String>> {
        Box::pin(async move {
            let key = plugin_background_key("sse", &plugin, &name, Some(&url), ctx.as_ref());
            if !claim_plugin_background_key(&key) {
                return Ok("already_registered".to_owned());
            }
            let Some(ctx) = ctx else {
                return Err("plugin SSE subscribe requires invocation context".to_owned());
            };
            let token = CancellationToken::new();
            if let Ok(mut guard) = plugin_sse_tokens().write() {
                guard.insert(key.clone(), token.clone());
            }
            let key_clone = key.clone();
            tokio::spawn(async move {
                run_plugin_sse(plugin, name, url, headers_json, resume_key, ctx, token).await;
                // Clean up after natural completion.
                if let Ok(mut guard) = plugin_background_keys().write() {
                    guard.remove(&key_clone);
                }
                if let Ok(mut guard) = plugin_sse_tokens().write() {
                    guard.remove(&key_clone);
                }
            });
            Ok("registered".to_owned())
        })
    }

    fn sse_status(
        &self,
        plugin: String,
        name: String,
        ctx: Option<rsclaw_plugin::PluginInvocationContext>,
    ) -> futures::future::BoxFuture<'static, std::result::Result<String, String>> {
        Box::pin(async move { Ok(plugin_sse_status_json(&plugin, &name, ctx.as_ref())) })
    }

    fn sse_unsubscribe(
        &self,
        plugin: String,
        name: String,
        ctx: Option<rsclaw_plugin::PluginInvocationContext>,
    ) -> futures::future::BoxFuture<'static, std::result::Result<String, String>> {
        Box::pin(async move {
            let prefix = plugin_background_key_prefix("sse", &plugin, &name, ctx.as_ref());
            let cancelled = cancel_plugin_sse_by_prefix(&prefix);
            Ok(format!("cancelled:{cancelled}"))
        })
    }

    fn push_outbound(
        &self,
        channel: String,
        peer_id: String,
        message_json: String,
        ctx: Option<rsclaw_plugin::PluginInvocationContext>,
    ) -> futures::future::BoxFuture<'static, std::result::Result<String, String>> {
        Box::pin(
            async move { push_plugin_outbound(&channel, &peer_id, &message_json, ctx.as_ref()) },
        )
    }

    fn submit_agent_turn(
        &self,
        session_key: String,
        prompt: String,
        route_json: String,
        ctx: Option<rsclaw_plugin::PluginInvocationContext>,
    ) -> futures::future::BoxFuture<'static, std::result::Result<String, String>> {
        Box::pin(async move {
            let session = if session_key.trim().is_empty() {
                ctx.as_ref()
                    .map(|c| c.session_key.as_str())
                    .unwrap_or("plugin:agent-turn")
            } else {
                session_key.as_str()
            };
            submit_plugin_agent_turn(session, &prompt, &route_json, ctx.as_ref())
        })
    }
}

fn claim_plugin_background_key(key: &str) -> bool {
    let keys = plugin_background_keys();
    let Ok(mut guard) = keys.write() else {
        return false;
    };
    guard.insert(key.to_owned())
}

fn plugin_background_key_prefix(
    kind: &str,
    plugin: &str,
    name: &str,
    ctx: Option<&rsclaw_plugin::PluginInvocationContext>,
) -> String {
    let ctx = ctx
        .map(plugin_invocation_context_key)
        .unwrap_or_else(|| "global".to_owned());
    format!("{kind}:{plugin}:{name}:{ctx}")
}

fn plugin_background_key(
    kind: &str,
    plugin: &str,
    name: &str,
    extra: Option<&str>,
    ctx: Option<&rsclaw_plugin::PluginInvocationContext>,
) -> String {
    let ctx = ctx
        .map(plugin_invocation_context_key)
        .unwrap_or_else(|| "global".to_owned());
    match extra {
        Some(extra) if !extra.is_empty() => format!("{kind}:{plugin}:{name}:{ctx}:{extra}"),
        _ => format!("{kind}:{plugin}:{name}:{ctx}"),
    }
}

fn plugin_invocation_context_key(ctx: &rsclaw_plugin::PluginInvocationContext) -> String {
    format!(
        "agent={}:channel={}:peer={}:chat={}:session={}",
        ctx.agent_id, ctx.channel, ctx.peer_id, ctx.chat_id, ctx.session_key
    )
}

fn cancel_plugin_sse_by_prefix(prefix: &str) -> usize {
    let keys_to_remove: Vec<String> = plugin_sse_tokens()
        .read()
        .map(|guard| {
            guard
                .iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .map(|(k, token)| {
                    token.cancel();
                    k.clone()
                })
                .collect()
        })
        .unwrap_or_default();
    let count = keys_to_remove.len();
    if let Ok(mut guard) = plugin_sse_tokens().write() {
        for k in &keys_to_remove {
            guard.remove(k);
        }
    }
    if let Ok(mut guard) = plugin_background_keys().write() {
        for k in &keys_to_remove {
            guard.remove(k);
        }
    }
    count
}

fn plugin_sse_status_json(
    plugin: &str,
    name: &str,
    ctx: Option<&rsclaw_plugin::PluginInvocationContext>,
) -> String {
    let prefix = plugin_background_key_prefix("sse", plugin, name, ctx);
    let count = plugin_background_keys()
        .read()
        .map(|keys| {
            keys.iter()
                .filter(|key| *key == &prefix || key.starts_with(&format!("{prefix}:")))
                .count()
        })
        .unwrap_or(0);
    serde_json::json!({
        "ok": true,
        "name": name,
        "active": count > 0,
        "count": count,
    })
    .to_string()
}

fn parse_hhmm(raw: &str) -> Option<(u32, u32)> {
    let (h, m) = raw.split_once(':')?;
    let hour = h.parse::<u32>().ok()?;
    let minute = m.parse::<u32>().ok()?;
    if hour < 24 && minute < 60 {
        Some((hour, minute))
    } else {
        None
    }
}

fn duration_until_shanghai(hour: u32, minute: u32) -> Duration {
    use chrono::{Datelike, TimeZone};
    let tz = chrono_tz::Asia::Shanghai;
    let now = chrono::Utc::now().with_timezone(&tz);
    let today = tz
        .with_ymd_and_hms(now.year(), now.month(), now.day(), hour, minute, 0)
        .single()
        .unwrap_or(now);
    let next = if today > now {
        today
    } else {
        today + chrono::Duration::days(1)
    };
    (next - now).to_std().unwrap_or(Duration::from_secs(60))
}

fn submit_plugin_agent_turn(
    session_key: &str,
    prompt: &str,
    route_json: &str,
    ctx: Option<&rsclaw_plugin::PluginInvocationContext>,
) -> Result<String, String> {
    let route: Value = serde_json::from_str(route_json).unwrap_or(Value::Null);
    let route_ctx = route.get("context").unwrap_or(&Value::Null);
    let channel = route
        .get("channel")
        .and_then(Value::as_str)
        .or_else(|| route_ctx.get("channel").and_then(Value::as_str))
        .or_else(|| ctx.map(|c| c.channel.as_str()))
        .unwrap_or("plugin");
    let peer_id = route
        .get("peer_id")
        .and_then(Value::as_str)
        .or_else(|| route_ctx.get("peer_id").and_then(Value::as_str))
        .or_else(|| ctx.map(|c| c.peer_id.as_str()))
        .unwrap_or("plugin");
    let chat_id = route
        .get("chat_id")
        .and_then(Value::as_str)
        .or_else(|| route_ctx.get("chat_id").and_then(Value::as_str))
        .or_else(|| ctx.map(|c| c.chat_id.as_str()))
        .unwrap_or(peer_id);
    let is_group = route
        .get("is_group")
        .and_then(Value::as_bool)
        .or_else(|| route_ctx.get("is_group").and_then(Value::as_bool))
        .or_else(|| ctx.map(|c| c.is_group))
        .unwrap_or(false);
    let tq = get_task_queue().ok_or_else(|| "task queue not installed".to_owned())?;
    let (task_id, merged) = submit_to_queue(
        &tq,
        session_key,
        prompt,
        channel,
        peer_id,
        chat_id,
        is_group,
        Priority::Cron,
    )
    .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "taskId": task_id, "merged": merged }).to_string())
}

fn push_plugin_outbound(
    channel: &str,
    peer_id: &str,
    message_json: &str,
    ctx: Option<&rsclaw_plugin::PluginInvocationContext>,
) -> Result<String, String> {
    let message: Value = serde_json::from_str(message_json)
        .map_err(|e| format!("plugin outbound message JSON invalid: {e}"))?;
    let text = message
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let images = json_array_strings(&message, "images");
    let files = json_array_files(&message, "files");
    let account = message
        .get("account")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let target_id = if peer_id.is_empty() {
        ctx.map(|c| c.peer_id.clone()).unwrap_or_default()
    } else {
        peer_id.to_owned()
    };
    // Batch fan-out: `batch_targets` (≤200 ids) is encoded into target_id via a
    // sentinel prefix so the existing OutboundMessage pipe carries it unchanged;
    // only feishu decodes it into a single `im/v1/batch_messages` call.
    let batch_targets = json_array_strings(&message, "batch_targets");
    let target_id = if batch_targets.is_empty() {
        target_id
    } else {
        format!(
            "{}{}",
            rsclaw_types::OUTBOUND_BATCH_PREFIX,
            batch_targets.join(",")
        )
    };
    let msg = OutboundMessage {
        target_id,
        is_group: message
            .get("is_group")
            .and_then(Value::as_bool)
            .or_else(|| ctx.map(|c| c.is_group))
            .unwrap_or(false),
        text,
        reply_to: None,
        images,
        files,
        channel: Some(channel.to_owned()),
        account,
    };
    let account = msg.account.clone();
    push_outbound(channel, account.as_deref(), msg)?;
    Ok("dispatched".to_owned())
}

fn json_array_strings(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn json_array_files(value: &Value, key: &str) -> Vec<(String, String, String)> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(json_file_tuple).collect())
        .unwrap_or_default()
}

fn json_file_tuple(value: &Value) -> Option<(String, String, String)> {
    let (path, filename, mime) = if let Some(path) = value.as_str() {
        let filename = std::path::Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin-file")
            .to_owned();
        (
            path.to_owned(),
            filename,
            "application/octet-stream".to_owned(),
        )
    } else {
        let obj = value.as_object()?;
        let path = obj.get("path").and_then(Value::as_str)?.to_owned();
        let filename = obj
            .get("filename")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                std::path::Path::new(&path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("plugin-file")
                    .to_owned()
            });
        let mime = obj
            .get("mime")
            .or_else(|| obj.get("mimeType"))
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream")
            .to_owned();
        (path, filename, mime)
    };
    Some((filename, mime, path))
}

async fn run_plugin_sse(
    plugin: String,
    name: String,
    url: String,
    headers_json: String,
    resume_key: String,
    ctx: rsclaw_plugin::PluginInvocationContext,
    token: CancellationToken,
) {
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            warn!(plugin, name, error = %e, "plugin SSE client build failed");
            return;
        }
    };
    let headers: Value = serde_json::from_str(&headers_json).unwrap_or_else(|_| Value::Null);
    let mut backoff = Duration::from_secs(1);
    loop {
        if token.is_cancelled() {
            break;
        }
        let mut req = client.get(&url).header("Accept", "text/event-stream");
        if let Some(obj) = headers.as_object() {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    req = req.header(k, s);
                }
            }
        }
        let send_fut = req.send();
        let resp = tokio::select! {
            _ = token.cancelled() => break,
            r = send_fut => r,
        };
        match resp {
            Ok(resp) => match resp.error_for_status() {
                Ok(resp) => {
                    backoff = Duration::from_secs(1);
                    let mut buf = String::new();
                    let mut event_name = String::new();
                    let mut data_lines: Vec<String> = Vec::new();
                    let mut stream = resp.bytes_stream();
                    loop {
                        let chunk = tokio::select! {
                            _ = token.cancelled() => break,
                            c = stream.next() => c,
                        };
                        let Some(chunk) = chunk else { break };
                        match chunk {
                            Ok(bytes) => {
                                buf.push_str(&String::from_utf8_lossy(&bytes));
                                while let Some(nl) = buf.find('\n') {
                                    let line = buf[..nl].trim_end_matches('\r').to_owned();
                                    buf.drain(..=nl);
                                    if line.is_empty() {
                                        if !data_lines.is_empty() {
                                            let data = data_lines.join("\n");
                                            let text = format_plugin_sse_text(
                                                &plugin,
                                                &name,
                                                &event_name,
                                                &data,
                                            );
                                            let _ = push_plugin_outbound(
                                                &ctx.channel,
                                                &ctx.peer_id,
                                                &serde_json::json!({ "text": text }).to_string(),
                                                Some(&ctx),
                                            );
                                        }
                                        event_name.clear();
                                        data_lines.clear();
                                    } else if let Some(rest) = line.strip_prefix("event: ") {
                                        event_name = rest.trim().to_owned();
                                    } else if let Some(rest) = line.strip_prefix("data: ") {
                                        data_lines.push(rest.to_owned());
                                    }
                                }
                            }
                            Err(e) => {
                                warn!(plugin, name, resume_key, error = %e, "plugin SSE read failed");
                                break;
                            }
                        }
                    }
                    if token.is_cancelled() {
                        break;
                    }
                }
                Err(e) => warn!(plugin, name, error = %e, "plugin SSE HTTP status error"),
            },
            Err(e) => warn!(plugin, name, error = %e, "plugin SSE connect failed"),
        }
        tokio::select! {
            _ = token.cancelled() => break,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
    info!(plugin, name, "plugin SSE stopped");
}

fn format_plugin_sse_text(plugin: &str, name: &str, event_name: &str, data: &str) -> String {
    let label = if event_name.is_empty() {
        "event"
    } else {
        event_name
    };
    if let Ok(v) = serde_json::from_str::<Value>(data) {
        if let Some(code) = v.get("code").and_then(Value::as_str) {
            let stock_name = v.get("name").and_then(Value::as_str).unwrap_or("");
            let filter = v.get("filter").and_then(Value::as_str).unwrap_or(label);
            return format!("[{plugin}/{name}] {filter}: {code} {stock_name}");
        }
    }
    format!("[{plugin}/{name}] {label}: {data}")
}

/// Root-side `rsclaw_types::TaskQueueHost` impl (crate-split P3 trait
/// inversion). Lets rsclaw-agent's `task` tool enqueue background tasks via the
/// gateway queue without depending on the gateway crate. Injected at startup.
pub struct GatewayTaskQueueHost;

impl rsclaw_types::TaskQueueHost for GatewayTaskQueueHost {
    fn submit_task(
        &self,
        session_key: &str,
        message: QueuedMessage,
        priority: Priority,
        max_turns: u32,
        ttl_secs: u64,
    ) -> anyhow::Result<(String, bool)> {
        let manager = get_task_queue()
            .ok_or_else(|| anyhow::anyhow!("task queue not available (gateway not started?)"))?;
        manager.submit_task(session_key, message, priority, max_turns, ttl_secs)
    }
}

#[cfg(test)]
mod plugin_background_tests {
    use super::*;

    fn ctx(peer: &str) -> rsclaw_plugin::PluginInvocationContext {
        rsclaw_plugin::PluginInvocationContext {
            target_id: peer.to_owned(),
            channel: "test".to_owned(),
            agent_id: "main".to_owned(),
            peer_id: peer.to_owned(),
            chat_id: peer.to_owned(),
            session_key: format!("agent:main:test:direct:{peer}"),
            is_group: false,
        }
    }

    #[test]
    fn plugin_background_keys_are_peer_scoped() {
        let a = ctx("peer-a");
        let b = ctx("peer-b");
        let key_a = plugin_background_key("cron", "market", "market.briefing", None, Some(&a));
        let key_a_again =
            plugin_background_key("cron", "market", "market.briefing", None, Some(&a));
        let key_b = plugin_background_key("cron", "market", "market.briefing", None, Some(&b));

        assert_eq!(key_a, key_a_again);
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn plugin_background_sse_keys_include_url_and_peer() {
        let a = ctx("peer-a");
        let b = ctx("peer-b");
        let url = "https://plugin.example/v1/stream?filter=alpha";
        let key_a = plugin_background_key("sse", "market", "market.alpha", Some(url), Some(&a));
        let key_b = plugin_background_key("sse", "market", "market.alpha", Some(url), Some(&b));
        let key_other_url = plugin_background_key(
            "sse",
            "market",
            "market.alpha",
            Some("https://plugin.example/v1/stream?filter=beta"),
            Some(&a),
        );

        assert_ne!(key_a, key_b);
        assert_ne!(key_a, key_other_url);
    }

    #[test]
    fn plugin_sse_status_is_context_scoped() {
        let a = ctx("peer-status-a");
        let b = ctx("peer-status-b");
        let url = "https://plugin.example/v1/stream?filter=alpha";
        let status_name = "market.alpha.status_test";

        let before: Value =
            serde_json::from_str(&plugin_sse_status_json("market", status_name, Some(&a)))
                .expect("status JSON before");
        assert_eq!(before["active"].as_bool(), Some(false));
        assert_eq!(before["count"].as_u64(), Some(0));

        let key = plugin_background_key("sse", "market", status_name, Some(url), Some(&a));
        assert!(claim_plugin_background_key(&key));

        let active: Value =
            serde_json::from_str(&plugin_sse_status_json("market", status_name, Some(&a)))
                .expect("status JSON active");
        assert_eq!(active["active"].as_bool(), Some(true));
        assert_eq!(active["count"].as_u64(), Some(1));

        let other_peer: Value =
            serde_json::from_str(&plugin_sse_status_json("market", status_name, Some(&b)))
                .expect("status JSON other peer");
        assert_eq!(other_peer["active"].as_bool(), Some(false));
        assert_eq!(other_peer["count"].as_u64(), Some(0));
    }
}
