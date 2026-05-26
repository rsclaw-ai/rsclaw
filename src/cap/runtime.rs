//! `CapAgentManager` — owns one respawnable driver slot per
//! `AgentKind`. Each slot fronts an actor task that owns a
//! `Box<dyn cap_rs::Driver>`. External callers reach the actor via
//! the slot's `mpsc::Sender<ToolCapRequest>`; events flow back via
//! the request's embedded `oneshot::Sender<Reply>` and a shared
//! `broadcast::Sender<events::AgentEvent>`.

/// Which coding agent a `tool_cap` call dispatches to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AgentKind {
    Claudecode,
    Openclaude,
    Opencode,
    Codex,
}

impl AgentKind {
    pub(crate) fn from_str(s: &str) -> Option<Self> {
        match s {
            "claudecode" => Some(Self::Claudecode),
            "openclaude" => Some(Self::Openclaude),
            "opencode" => Some(Self::Opencode),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Claudecode => "claudecode",
            Self::Openclaude => "openclaude",
            Self::Opencode => "opencode",
            Self::Codex => "codex",
        }
    }
}

// `CapAgentManager` to be filled in Task 5.
pub(crate) struct CapAgentManager;
