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

// Records lifted to rsclaw-types (crate-split); re-exported.
pub use rsclaw_types::{ExternalJobKind, ExternalJobOrigin, ExternalJobStatus, ExternalJobDelivery, ExternalJob, DEFAULT_TIMEOUT_SECS};


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






