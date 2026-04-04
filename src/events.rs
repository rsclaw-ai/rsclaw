//! Shared event types — used by agent runtimes and the SSE broadcast layer.
//!
//! Defined here (not in `server`) to avoid a circular dependency:
//!   agent → events ← server

use serde::Serialize;

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
}
