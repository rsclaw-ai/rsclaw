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
