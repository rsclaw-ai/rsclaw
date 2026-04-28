//! External-job persistence — keeps long-running provider tasks alive
//! across gateway restarts.
//!
//! When a tool like `tool_video` submits a generation job to an external
//! provider (Seedance / MiniMax / etc.) the agent gets back a provider
//! task ID and would normally block-poll until completion. That coupling
//! is what makes the user lose their video when the gateway is restarted
//! mid-poll: the tool returns an error, the agent reply is empty, and the
//! provider's render still finishes — but no one is listening anymore.
//!
//! This module records each in-flight provider job in redb so a dedicated
//! background worker (`ExternalJobsWorker`, separate file) can resume
//! polling after restart and deliver the result to the original session
//! through the standard channel notification path.
//!
//! Phase A scope (this file): pure types + persistence. The worker, the
//! provider polling adapters, and the tool-side enqueue path are added in
//! follow-up commits.
//!
//! Lifecycle: `Pending` → `Polling` → (`Done` | `Failed` | `TimedOut`).
//! Terminal rows are kept around briefly so the delivery side can
//! idempotently handle restart races, then GC'd by `cleanup_finished`.

use serde::{Deserialize, Serialize};

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

/// What a single provider poll returned. Worker uses this to decide
/// whether to keep polling, deliver the artifact, or surface a failure.
#[derive(Debug, Clone)]
pub enum PollOutcome {
    /// Provider says the job is still in progress — schedule another poll.
    Pending,
    /// Provider returned the artifact URL.
    Done(String),
    /// Provider reported a non-recoverable failure.
    Failed(String),
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

/// Default per-job timeout — 30 minutes covers Seedance / MiniMax / Kling
/// in the worst case while still bounding redb row lifetime.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30 * 60;

/// Default polling cadence used by `next_poll_delay_secs` for the first
/// dense window; afterwards the worker switches to `LATE_POLL_INTERVAL_SECS`.
const EARLY_POLL_INTERVAL_SECS: u64 = 10;
const LATE_POLL_INTERVAL_SECS: u64 = 60;
const EARLY_POLL_BUDGET: u32 = 12; // 12 polls × 10s = first 2 minutes

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
