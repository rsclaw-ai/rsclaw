//! cap-protocol coding-agent driver glue.
//!
//! Replaces `src/acp/`. All four coding agents (claudecode,
//! openclaude, opencode, codex) drive through `cap-rs`. The
//! LLM-facing tool surface is the single `tool_cap` in
//! `crate::agent::tools_cap`.

pub(crate) mod bridge;
pub(crate) mod live;
pub(crate) mod notification;
pub(crate) mod permission;
pub(crate) mod runtime;

pub(crate) use live::CapLiveManager;
pub(crate) use runtime::{AgentKind, CapAgentManager};
