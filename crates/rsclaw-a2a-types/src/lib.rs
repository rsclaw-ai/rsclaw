//! `rsclaw-a2a-types` — A2A protocol wire + event types (crate-split step-12
//! P1).
//!
//! Lifted from `src/a2a/{event,types}.rs` (verified zero internal crate deps —
//! pure serde/tokio/dashmap leaves). Pulling them into a leaf crate breaks the
//! `agent -> a2a` type edge: `AgentMessage.event_tx` now points here, not at
//! the a2a runtime module, so agent core can later extract independently.
pub mod durable_relay;
pub mod event;
pub mod types;

pub mod client;
