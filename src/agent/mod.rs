//! Agent subsystem.
//!
//! Modules:
//!   - `registry`       — AgentHandle + AgentRegistry + channel routing
//!   - `runtime`        — Agent loop (LLM ↔ tool execution cycle)
//!   - `workspace`      — WorkspaceContext (AGENTS.md / SOUL.md / etc.)
//!   - `loop_detection` — Sliding-window loop detector
//!   - `collaboration`  — Sequential / Parallel / Orchestrated collab modes

pub mod bootstrap;
pub mod doc;
pub mod btw;
pub mod collaboration;
pub mod loop_detection;
pub mod memory;
pub mod permission;
pub mod preparse;
pub mod registry;
pub mod runtime;
pub mod spawner;
pub mod tool_call_repair;
pub mod workspace;

pub use bootstrap::{seed_tools, seed_workspace, seed_workspace_with_lang};
pub use collaboration::CollabMode;
pub use loop_detection::LoopDetector;
pub use memory::{MemoryDoc, MemoryStore};
pub use permission::{
    add_pending_permission, get_pending_permission, remove_pending_permission, resolve_permission,
};
pub use registry::{
    AgentHandle, AgentMessage, AgentRegistry, AgentReply, FileAttachment, ImageAttachment,
    PendingAnalysis,
};
pub use runtime::{AgentRuntime, LiveStatus};
pub use spawner::AgentSpawner;
pub use workspace::{SessionType, WorkspaceContext};
