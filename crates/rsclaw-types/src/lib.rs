//! `rsclaw-types` — stable, cross-crate DTOs with no first-party dependencies.
//!
//! These types were historically embedded inside `agent`, `channel`, and
//! `gateway`. They are lifted here so lower crates (config, provider, store,
//! kb, channel) can depend on them without depending on the runtime knot.
//!
//! Policy: append-only / frozen. A type belongs here only if it has been
//! stable for months AND is referenced across crate boundaries. Anything with
//! behaviour or non-trivial deps (e.g. provider wire DTOs) stays in its
//! domain crate.

/// The four kinds of agent in the system.
///
/// (Distinct from `cap::AgentKind`, which enumerates external CLI drivers like
/// Claudecode/Codex — same name, different domain, different crate.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum AgentKind {
    /// The default entry point. Cannot be deleted. `default: true` in config.
    Main,
    /// User-created persistent agent. Saved to config file, survives restarts.
    Named,
    /// LLM-spawned temporary agent (`persistent: false`). Lives in memory, gone
    /// on restart.
    Sub,
    /// One-shot task agent. Automatically destroyed after completion.
    Task,
}

/// An image attachment sent by the user.
#[derive(Debug, Clone)]
pub struct ImageAttachment {
    /// Base64-encoded data URI or URL.
    pub data: String,
    /// MIME type (e.g. "image/png", "image/jpeg").
    pub mime_type: String,
    /// Best-effort original on-disk source path, when the image came from a
    /// path-referencing client (e.g. `[file:/abs/path]` from desktop, or a
    /// channel that downloaded to a known cache location). `None` for
    /// inline-only attachments (e.g. pasted base64).
    ///
    /// Surfaced in vision-failure fallback messages so the user can re-attach
    /// or the agent can retry with the same file via a different tool.
    pub source_path: Option<String>,
}

/// A file attachment sent by the user (raw bytes, not yet processed).
#[derive(Debug, Clone)]
pub struct FileAttachment {
    pub filename: String,
    pub data: Vec<u8>,
    pub mime_type: String,
}

/// A reply message ready to be sent to a channel.
#[derive(Debug, Clone, Default)]
pub struct OutboundMessage {
    /// Destination peer/group ID.
    pub target_id: String,
    /// Whether `target_id` is a group.
    pub is_group: bool,
    /// Text content.
    pub text: String,
    /// Optional reply-to message ID (platform-specific).
    pub reply_to: Option<String>,
    /// Image attachments (base64 data URIs).
    pub images: Vec<String>,
    /// File attachments: Vec<(filename, mime_type, file_path_or_url)>.
    /// Supported by channels that can send files (feishu, telegram, etc.).
    pub files: Vec<(String, String, String)>,
    /// Channel name to use for sending (e.g., "feishu", "telegram").
    /// Used by background tasks (opencode, claudecode) to route notifications.
    pub channel: Option<String>,
    /// Multi-account routing tag — which account in this channel received
    /// the inbound message that produced this reply. Set by inbound parsers
    /// that support multiple credentials (e.g. feishu accounts.<name>) so
    /// the outbound dispatcher can send via the same account's API token.
    /// `None` = single-account / bare `{channel}` lookup.
    pub account: Option<String>,
}

/// Sentinel prefix on `OutboundMessage.target_id` marking a batch fan-out: the
/// remainder is a comma-separated recipient id list that a batch-capable
/// channel (feishu `im/v1/batch_messages`, ≤200 ids/call) delivers in one API
/// call. Carried in `target_id` (rather than a new struct field) so the ~90
/// existing `OutboundMessage` literals stay untouched; the dispatcher still
/// hashes the whole string for worker routing. Only feishu decodes it; other
/// channels would attempt to send to the literal id and fail, so this is only
/// ever routed to feishu. Only individual user ids batch — not group/chat ids.
pub const OUTBOUND_BATCH_PREFIX: &str = "\u{1}batch:";

/// Names of all built-in agent tools. Lifted from agent/prompt_builder.rs
/// (crate-split) so rsclaw-provider can reference it without depending on the
/// runtime knot. Re-exported at
/// crate::agent::prompt_builder::BUILTIN_TOOL_NAMES.
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    "memory",
    "todo",
    "skill_use",
    "task",
    "task_finish",
    "read_file",
    "write_file",
    "edit_file",
    "send_file",
    "shell",
    "agent",
    "ask_user",
    "install_tool",
    "list_dir",
    "search_file",
    "search_content",
    "web_search",
    "web_fetch",
    "web_download",
    "web_browser",
    "computer_use",
    "image_gen",
    "video_gen",
    "avatar_gen",
    "mv_gen",
    "music_gen",
    "voice_gen",
    "pdf",
    "text_to_voice",
    "send_message",
    "cron",
    "session",
    "gateway",
    "cap",
    "cap_live",
    "cap_live_end",
    "cap_bind_sticky",
    "cap_unbind_sticky",
    "channel",
    "anycli",
    "pairing",
    "create_docx",
    "create_pdf",
    "create_xlsx",
    "create_pptx",
    "doc",
    // Context-recovery tools: static, byte-identical for every client of
    // this prefix version, so they belong in the cacheable builtin prefix.
    // Previously misclassified as user_tools, which under prefix_id mode
    // (dynamic_prefix omitted) meant they were NEVER sent to the model —
    // so the model could never call read_session_archive despite the
    // summary/system-prompt telling it to.
    "read_session_archive",
    "read_artifact",
    "knowledge_base",
    // A2A-only tool, but unconditionally registered in build_tool_list and
    // byte-identical across clients — same builtin class. Errors gracefully
    // if called on a non-A2A turn.
    "wait_input",
    // Self-service skill management (discover → install → use → remove).
    "skill_list",
    "skill_search",
    "skill_install",
    "skill_remove",
    // Plugin meta tools (the 4 dispatchers): byte-identical for every client
    // of this version, so they belong in the cacheable builtin prefix. Live
    // per-plugin tool schemas are NOT registered here — they're rendered as
    // text into user_system (KV-cache friendly) by `render_active_plugin_tools_text`.
    "plugin_list",
    "plugin_search",
    "plugin_describe",
    "plugin_invoke",
];

// ============================================================================
// Gateway persistence records (crate-split): lifted from gateway/{task_queue,
// external_jobs}.rs so rsclaw-store can depend on them without depending on the
// gateway runtime. Re-exported at their original paths.
// ============================================================================
use md5::{Digest as _, Md5};
use serde::{Deserialize, Serialize};

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
    /// Multi-account tag (e.g. feishu account name) so a queued task's reply
    /// is sent back via the same account that received it. None =
    /// single-account channel; the bare `{channel}` sender is used.
    #[serde(default)]
    pub account: Option<String>,
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
pub fn default_max_turns() -> u32 {
    0
}

/// Compute an MD5 hex digest of `text` (for content dedup, not security).
pub fn compute_hash(text: &str) -> String {
    let hash = Md5::digest(text.as_bytes());
    hex::encode(hash)
}
/// What kind of artifact the job will produce when it finishes. Used by the
/// worker's delivery step to pick the right channel formatting (e.g. video
/// vs image attachment).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExternalJobKind {
    /// Provider generated a video (URL → download → channel attachment).
    VideoGen,
    /// Provider generated an image (URL → download → channel attachment).
    ImageGen,
}

/// What initiated the job. Drives the delivery decision: agent-originated
/// jobs may carry a `reply_to`, cron-originated jobs are pushed as a fresh
/// notification, user-direct (REST) jobs return through their HTTP handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExternalJobOrigin {
    Agent,
    Cron,
    UserDirect,
}

/// Lifecycle of an external job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExternalJobStatus {
    /// Submitted to provider, never polled yet.
    Pending,
    /// Worker is actively polling (status persisted in case the worker
    /// crashes between poll and update — recovery treats it the same as
    /// `Pending`).
    Polling,
    /// Provider reported success and the artifact has been delivered.
    Done,
    /// Provider reported a non-recoverable error.
    Failed,
    /// Worker exhausted the timeout budget without a terminal status.
    TimedOut,
}

/// Routing context for delivering the artifact when polling completes.
/// Mirrors the fields of `OutboundMessage` that depend on the originating
/// message rather than the job result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalJobDelivery {
    pub channel: String,
    pub target_id: String,
    pub is_group: bool,
    pub reply_to: Option<String>,
    #[serde(default)]
    pub account: Option<String>,
}

/// A provider job sitting in the persistent queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalJob {
    /// Internal UUID — primary key in the redb table.
    pub id: String,
    /// Originating session_key so the result can be appended to the right
    /// conversation history.
    pub session_key: String,
    /// Where to push the finished artifact.
    pub delivery: ExternalJobDelivery,
    /// What kicked off the job (agent / cron / direct).
    pub origin: ExternalJobOrigin,
    /// Provider name, matching the dispatch keys used by the worker
    /// (e.g. "seedance", "minimax").
    pub provider: String,
    /// Provider's own task identifier.
    pub external_task_id: String,
    /// What kind of artifact to expect.
    pub kind: ExternalJobKind,
    /// Original prompt — kept so the delivery message can give the user
    /// context ("Your video for: <prompt>") without re-fetching from
    /// chat history.
    pub prompt: String,
    /// Wall-clock submission time (Unix seconds).
    pub submitted_at: i64,
    /// Earliest wall-clock time at which the worker should poll again.
    pub next_poll_at: i64,
    /// How many times we've polled this job — drives the back-off.
    pub poll_count: u32,
    /// Hard deadline; once `now() > timeout_at` the worker marks the row
    /// `TimedOut` regardless of provider state.
    pub timeout_at: i64,
    /// Current lifecycle state.
    pub status: ExternalJobStatus,
    /// Last error message (transient or terminal).
    pub error: Option<String>,
    /// Result URL from the provider (set when Done).
    pub result_url: Option<String>,
    /// Local file path after download (set when Done — so reconnect-replay
    /// can re-deliver the saved artifact instead of re-downloading).
    pub result_path: Option<String>,
    /// Wall-clock time the artifact (or failure notice) was successfully
    /// pushed via `notification_tx`. While `None`, the worker treats the
    /// row as "needs delivery" and will retry each tick — protects against
    /// `notification_tx.send` failing on a full broadcast or transient
    /// channel-handler issue.
    #[serde(default)]
    pub delivered_at: Option<i64>,
    /// Number of delivery attempts so far. Drives the next-retry back-off
    /// in worker.rs and bounds the retry budget.
    #[serde(default)]
    pub delivery_attempts: u32,
}

/// What a single provider poll returned. Worker uses this to decide
/// whether to keep polling, deliver the artifact, or surface a failure.
///
/// Lifted from `gateway/external_jobs.rs` (crate-split) so `rsclaw-jobs`
/// can return it without depending on the gateway runtime. Re-exported at
/// `crate::gateway::external_jobs::PollOutcome`.
#[derive(Debug, Clone)]
pub enum PollOutcome {
    /// Provider says the job is still in progress — schedule another poll.
    Pending,
    /// Provider returned the artifact URL.
    Done(String),
    /// Provider reported a non-recoverable failure.
    Failed(String),
}

/// Default per-job timeout — 30 minutes covers Seedance / MiniMax / Kling
/// in the worst case while still bounding redb row lifetime.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30 * 60;

/// Default polling cadence used by `next_poll_delay_secs` for the first
/// dense window; afterwards the worker switches to `LATE_POLL_INTERVAL_SECS`.
pub const EARLY_POLL_INTERVAL_SECS: u64 = 10;

pub const LATE_POLL_INTERVAL_SECS: u64 = 60;

pub const EARLY_POLL_BUDGET: u32 = 12; // 12 polls × 10s = first 2 minutes

impl ExternalJob {
    /// Build a freshly-submitted job ready to be enqueued.
    pub fn new_submitted(
        session_key: impl Into<String>,
        delivery: ExternalJobDelivery,
        origin: ExternalJobOrigin,
        provider: impl Into<String>,
        external_task_id: impl Into<String>,
        kind: ExternalJobKind,
        prompt: impl Into<String>,
    ) -> Self {
        let now = chrono::Utc::now().timestamp();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            session_key: session_key.into(),
            delivery,
            origin,
            provider: provider.into(),
            external_task_id: external_task_id.into(),
            kind,
            prompt: prompt.into(),
            submitted_at: now,
            next_poll_at: now + EARLY_POLL_INTERVAL_SECS as i64,
            poll_count: 0,
            timeout_at: now + DEFAULT_TIMEOUT_SECS as i64,
            status: ExternalJobStatus::Pending,
            error: None,
            result_url: None,
            result_path: None,
            delivered_at: None,
            delivery_attempts: 0,
        }
    }

    /// True when the job is in a terminal state (`Done` / `Failed` /
    /// `TimedOut`) but the result has not yet been pushed via the
    /// notification channel. Used by the worker's delivery-retry sweep.
    pub fn needs_delivery(&self) -> bool {
        matches!(
            self.status,
            ExternalJobStatus::Done | ExternalJobStatus::Failed | ExternalJobStatus::TimedOut
        ) && self.delivered_at.is_none()
    }

    /// Max delivery retries before we give up. After this the job is
    /// considered abandoned (logged) and GC-eligible regardless of
    /// `delivered_at` so the table doesn't grow forever on a permanently
    /// broken channel sink.
    pub const MAX_DELIVERY_ATTEMPTS: u32 = 60; // ~30 min at 30s back-off

    /// Compute the next polling delay using a coarse two-step back-off:
    /// dense (10 s) for the first ~2 minutes — when most jobs finish — then
    /// sparse (60 s) for the long tail.
    pub fn next_poll_delay_secs(&self) -> u64 {
        if self.poll_count < EARLY_POLL_BUDGET {
            EARLY_POLL_INTERVAL_SECS
        } else {
            LATE_POLL_INTERVAL_SECS
        }
    }

    /// Whether the worker should treat this job as exhausted.
    pub fn is_timed_out(&self) -> bool {
        chrono::Utc::now().timestamp() > self.timeout_at
    }
}
// ============================================================================
// Notification sink (crate-split): lifted from cap/notification.rs so channel
// and other crates can implement/route notifications without depending on cap.
// ============================================================================
use futures::future::BoxFuture as _NotifBoxFuture;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
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
    fn send(&self, notification: &Notification) -> _NotifBoxFuture<'_, anyhow::Result<()>>;
}

pub mod turn_metrics;

// ============================================================================
// BriefingSink (crate-split): trait inversion so trusted runtime integrations
// can submit to the gateway task queue + push outbound WITHOUT depending on the
// gateway runtime.
// ============================================================================
pub trait BriefingSink: Send + Sync {
    /// Submit a briefing prompt to the agent task queue.
    /// Returns (task_id, merged_into_existing).
    fn submit_briefing(
        &self,
        session_key: &str,
        text: &str,
        channel: &str,
        peer_id: &str,
        chat_id: &str,
        is_group: bool,
        priority: Priority,
    ) -> anyhow::Result<(String, bool)>;

    /// Push an outbound message directly to a channel sender.
    fn push_outbound(
        &self,
        channel: &str,
        account: Option<&str>,
        msg: OutboundMessage,
    ) -> Result<(), String>;
}

/// Outcome of an agent turn (crate-split: lifted from agent/registry.rs so
/// rsclaw-a2a can consume it without depending on the agent runtime).
/// How a turn finished, surfaced from the gateway worker so A2A consumers
/// can emit the right terminal state. Non-A2A channels (WebSocket, channels/*)
/// ignore this — they show `reply.text` to the user regardless.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ReplyOutcome {
    #[default]
    Ok,
    /// Turn errored — `text` is the rendered error message.
    Error,
    /// Turn was cancelled via the A2A cancel_token. The cancel handler has
    /// already published the terminal `Canceled` status event; A2A consumers
    /// must not emit another terminal event.
    Canceled,
}
// Structured task outcome (crate-split: lifted from gateway/task_queue.rs).
/// Self-reported outcome an agent fills in via the `task_finish` tool when
/// it believes its task is finished (or deliberately abandoned).
///
/// Provides structured signal that replaces the fragile string-matching path
/// in `classify_outcome`. Also serialised into `A2aTask.metadata.outcome`
/// for protocol-compliant A2A v1.0 reporting.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct StructuredOutcome {
    /// How complete the work is. Drives the orchestrator's continue/stop call.
    pub completion: Completion,

    /// The agent's own recommendation for what to do next. Orchestrator
    /// treats this as a strong hint but applies its own decision matrix.
    pub recommend: Recommend,

    /// Did the agent actually run tests/build/lint/curl that confirmed the
    /// claimed work? `false` is honest; `true` without `verification_log`
    /// is downgraded to `false` by the orchestrator.
    #[serde(default)]
    pub verified: bool,

    /// Evidence backing `verified=true`. Short excerpt of command + output.
    /// Required when `verified=true`; absence downgrades to `verified=false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_log: Option<String>,

    /// Concrete things the agent did. Each entry should map to an observable
    /// artifact (file changed, command run, message sent). Empty list with
    /// `completion=Full` is suspicious — orchestrator may downgrade.
    #[serde(default)]
    pub accomplished: Vec<String>,

    /// Things the agent deliberately skipped, paired with the reason.
    /// Distinct from `blocked_on` — these are choices, not obstacles.
    #[serde(default)]
    pub skipped: Vec<SkipEntry>,

    /// Unresolved blockers. Non-empty implies `completion != Full`.
    /// When `recommend = NeedsHuman`, these are surfaced to the user verbatim.
    #[serde(default)]
    pub blocked_on: Vec<String>,

    /// Assumptions the agent made when the spec was ambiguous. Non-empty
    /// with `completion < Full` triggers orchestrator to confirm with user.
    #[serde(default)]
    pub assumptions: Vec<String>,

    /// Suggested next tasks. Each entry is a self-contained task description,
    /// not a TODO note. Auto-spawned when `recommend = Continue`.
    #[serde(default)]
    pub follow_up_tasks: Vec<String>,

    /// Optional one-paragraph prose summary for the channel reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// How complete the agent's work is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Completion {
    /// Everything the user asked for, done and (ideally) verified.
    Full,
    /// Meaningful subset done. Coverage implied by `accomplished` length.
    Partial,
    /// Almost nothing achieved. Agent should explain in `blocked_on`.
    Minimal,
    /// Agent attempted, work was wrong/rolled back, nothing landed.
    Failed,
}

/// Recommended next action for the orchestrator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recommend {
    /// Done, deliver to user, close the task.
    Ship,
    /// Spawn the next task automatically using `follow_up_tasks`.
    Continue,
    /// Stop the auto-continue loop; surface `blocked_on` to the user.
    NeedsHuman,
    /// Same task, fresh attempt. Agent thinks a retry with the same prompt
    /// might succeed (transient failure, flaky env).
    Retry,
    /// Agent gave up; orchestrator should not retry without changed inputs.
    Abandon,
}
// Pending-outcome staging map (crate-split: lifted from gateway/task_queue.rs
// so rsclaw-a2a can drain staged outcomes without depending on the gateway).
use std::collections::HashMap as _PoHashMap;
static PENDING_OUTCOMES: std::sync::OnceLock<
    std::sync::Mutex<_PoHashMap<String, StructuredOutcome>>,
> = std::sync::OnceLock::new();

fn pending_outcomes_map() -> &'static std::sync::Mutex<_PoHashMap<String, StructuredOutcome>> {
    PENDING_OUTCOMES.get_or_init(|| std::sync::Mutex::new(_PoHashMap::new()))
}

/// Stage a structured outcome produced by the `task_finish` tool.
pub fn stage_pending_outcome(session_key: &str, outcome: StructuredOutcome) {
    if let Ok(mut map) = pending_outcomes_map().lock() {
        map.insert(session_key.to_owned(), outcome);
    }
}

/// Remove and return the staged outcome for `session_key`, if any.
pub fn drain_pending_outcome(session_key: &str) -> Option<StructuredOutcome> {
    pending_outcomes_map().lock().ok()?.remove(session_key)
}

// SkipEntry (crate-split: lifted with StructuredOutcome).
/// A deliberate skip, paired with its reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkipEntry {
    /// What was skipped.
    pub what: String,
    /// Why it was skipped.
    pub why: String,
}
/// Default max turns for /task mode (crate-split: lifted from
/// gateway/task_queue.rs).
pub const TASK_DEFAULT_MAX_TURNS: u32 = 10;
/// Default TTL for /task mode (1 hour).
pub const TASK_DEFAULT_TTL_SECS: u64 = 3600;

// ============================================================================
// TaskQueueHost (crate-split step-12 P3): trait inversion so rsclaw-agent can
// submit background tasks WITHOUT depending on the gateway task-queue runtime.
// Root's gateway implements this and injects it at startup.
// ============================================================================
pub trait TaskQueueHost: Send + Sync {
    /// Submit a background task to the queue. Returns (task_id,
    /// merged_into_existing).
    fn submit_task(
        &self,
        session_key: &str,
        message: QueuedMessage,
        priority: Priority,
        max_turns: u32,
        ttl_secs: u64,
    ) -> anyhow::Result<(String, bool)>;
}

static TASK_QUEUE_HOST: std::sync::OnceLock<std::sync::Arc<dyn TaskQueueHost>> =
    std::sync::OnceLock::new();

pub fn set_task_queue_host(host: std::sync::Arc<dyn TaskQueueHost>) {
    let _ = TASK_QUEUE_HOST.set(host);
}

pub fn task_queue_host() -> Option<std::sync::Arc<dyn TaskQueueHost>> {
    TASK_QUEUE_HOST.get().cloned()
}
