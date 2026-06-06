//! cap-protocol coding-agent driver glue.
//!
//! Replaces `src/acp/`. All four coding agents (claudecode,
//! openclaude, opencode, codex) drive through `cap-rs`. The
//! LLM-facing tool surface is the single `tool_cap` in
//! `crate::agent::tools_cap`.

pub(crate) mod bridge;
pub(crate) mod identity;
pub(crate) mod live;
pub(crate) mod notification;
pub(crate) mod permission;
pub(crate) mod runtime;

pub(crate) use live::CapLiveManager;
pub(crate) use runtime::{AgentKind, CapAgentManager};

/// Process-wide handle to the single `CapLiveManager`. Set once by
/// `gateway/startup.rs` after construction; read by preparse so the
/// `/cap` and `/cap-exit` slash commands can register/unregister sticky
/// IM-session bindings without each channel having to plumb the manager
/// through its own state.
///
/// `OnceLock` so this is both lock-free at read time and safe to set
/// before any channel inbound code runs (startup orders the `set` call
/// before `spawn_channel_tasks`). Tests that don't go through the
/// gateway leave it unset — `get()` returns `None`, slash commands
/// reply with "cap_live not initialised".
pub(crate) static GLOBAL_CAP_LIVE: std::sync::OnceLock<std::sync::Arc<CapLiveManager>> =
    std::sync::OnceLock::new();

/// Idempotent setter. Subsequent calls (e.g. on hot-restart paths) are
/// silently ignored — keeping the first manager wired up is correct
/// because all existing sticky bindings already point at it.
pub(crate) fn set_global_cap_live(m: std::sync::Arc<CapLiveManager>) {
    let _ = GLOBAL_CAP_LIVE.set(m);
}
