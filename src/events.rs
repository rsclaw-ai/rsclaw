//! Shared event types — used by agent runtimes and the SSE broadcast layer.
//!
//! Defined here (not in `server`) to avoid a circular dependency:
//!   agent → events ← server
//!
//! TODO: Current limitations and improvement plan:
//! - AgentEvent is a flat struct; richer event types (tool calls, errors,
//!   usage updates) require either new structs or an enum-based approach.
//! - The broadcast channel drops events when subscribers lag; consider a
//!   bounded replay buffer or per-subscriber mpsc for guaranteed delivery.
//! - No event filtering: every subscriber receives every agent's events.
//!   Add topic-based or session-based filtering when load requires it.

use serde::Serialize;

/// Why the gateway is asking the user to restart.
///
/// Multi-source: the config file watcher, the BGE auto-downloader, and any
/// future installer (plugin / model / migration) all publish into the same
/// `restart_request_tx` broadcast channel.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RestartReason {
    /// User edited `~/.rsclaw/rsclaw.json5` (file watcher trigger).
    /// `sections` is best-effort; empty if diff was not computed.
    ConfigChanged {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        sections: Vec<String>,
    },
    /// A model finished downloading in the background — restart loads it.
    ModelDownloaded { name: String },
    /// A model download failed; not strictly a restart trigger, but the UI
    /// shows it in the same banner channel.
    ModelDownloadFailed { name: String, error: String },
}

/// How urgently the gateway recommends restarting.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestartUrgency {
    /// New behavior takes effect after restart, but the gateway works without it.
    Recommended,
    /// The gateway is already in a degraded state; restart is required to recover.
    Required,
}

/// Published by any source that wants the user to restart the gateway.
/// Latched in `AppState::pending_restart` so late-connecting UIs see it.
#[derive(Debug, Clone, Serialize)]
pub struct RestartRequest {
    /// Wall-clock time the request was generated, milliseconds since epoch.
    pub at_ms: u64,
    pub reason: RestartReason,
    pub urgency: RestartUrgency,
    /// Pre-translated, human-readable message for the banner.
    pub message: String,
}

impl RestartRequest {
    /// Construct a new request stamped with `now`.
    pub fn new(reason: RestartReason, urgency: RestartUrgency, message: String) -> Self {
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            at_ms,
            reason,
            urgency,
            message,
        }
    }
}

/// Emitted by `AgentRuntime` and broadcast to SSE subscribers via the
/// `AppState::event_bus` channel.
#[derive(Debug, Clone, Serialize)]
pub struct AgentEvent {
    pub session_id: String,
    pub agent_id: String,
    /// Incremental text delta.  Empty when `done = true`.
    pub delta: String,
    /// `true` on the final "turn complete" event.
    pub done: bool,
    /// File attachments produced this turn: (filename, mime_type, local_path_or_url).
    /// Non-empty only on the final `done = true` event.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<(String, String, String)>,
    /// Image attachments (base64 data URIs or local paths).
    /// Non-empty only on the final `done = true` event.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    /// Tool call log for this turn: (name, args_json, output_text).
    /// Non-empty only on the final `done = true` event.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_log: Vec<(String, String, String)>,
}
