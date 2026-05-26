//! `tool_cap` — single LLM-facing tool that dispatches to one of four
//! coding agents via `crate::cap::CapAgentManager`.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::cap::{AgentKind, CapAgentManager};

impl super::runtime::AgentRuntime {
    pub(crate) async fn tool_cap(&self, args: Value) -> Result<Value> {
        let agent_str = args["agent"]
            .as_str()
            .ok_or_else(|| anyhow!("tool_cap: `agent` required"))?;
        let kind = AgentKind::from_str(agent_str)
            .ok_or_else(|| anyhow!("tool_cap: unknown agent `{agent_str}`"))?;
        let task = args["task"]
            .as_str()
            .ok_or_else(|| anyhow!("tool_cap: `task` required"))?;
        let cwd = args["cwd"]
            .as_str()
            .map(|s| std::path::PathBuf::from(crate::agent::runtime::expand_tilde(s)))
            .unwrap_or_else(|| self.default_workspace());

        let manager: &CapAgentManager = self
            .cap_manager
            .as_ref()
            .ok_or_else(|| anyhow!("tool_cap: CapAgentManager not initialised"))?;

        tracing::info!(
            agent = agent_str,
            cwd = %cwd.display(),
            task_preview = %task.chars().take(80).collect::<String>(),
            "tool_cap: dispatch"
        );
        let reply = manager.dispatch(kind, task.to_owned(), cwd).await?;
        Ok(json!({ "agent": agent_str, "reply": reply.text }))
    }
}
