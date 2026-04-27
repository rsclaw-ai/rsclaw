//! Agent subsystem.
//!
//! Modules:
//!   - `registry`       — AgentHandle + AgentRegistry + channel routing
//!   - `runtime`        — Agent loop (LLM ↔ tool execution cycle)
//!   - `workspace`      — WorkspaceContext (AGENTS.md / SOUL.md / etc.)
//!   - `loop_detection` — Sliding-window loop detector
//!   - `collaboration`  — Sequential / Parallel / Orchestrated collab modes

pub mod bootstrap;
pub mod evolution;
pub mod btw;
pub mod collaboration;
pub mod compaction;
pub mod context_mgr;
pub mod doc;
pub mod exec_pool;
pub mod loop_detection;
pub mod memory;
pub mod permission;
pub mod platform;
pub mod preparse;
pub mod prompt_builder;
pub mod query_planner;
pub mod registry;
pub mod runtime;
pub mod security;
pub mod spawner;
pub mod tool_call_repair;
pub mod tools_acp;
pub mod tools_agent;
pub mod tools_builder;
pub mod tools_computer;
pub mod tools_cron;
pub mod tools_file;
pub mod tools_image;
pub mod tools_misc;
pub mod tools_session;
pub mod tools_video;
pub mod tools_web;
pub mod web_parsers;
pub mod workspace;

pub use bootstrap::{seed_tools, seed_workspace, seed_workspace_with_lang};
pub use collaboration::CollabMode;
pub use loop_detection::LoopDetector;
pub use memory::{MemoryDoc, MemoryStore};
pub use permission::{
    add_pending_permission, get_pending_permission, remove_pending_permission, resolve_permission,
};
pub use registry::{
    AgentHandle, AgentKind, AgentMessage, AgentRegistry, AgentReply,
    FileAttachment, ImageAttachment, PendingAnalysis, SessionTokens,
};
pub use runtime::{AgentRuntime, LiveStatus};
pub use spawner::AgentSpawner;
pub use workspace::{SessionType, WorkspaceContext};
