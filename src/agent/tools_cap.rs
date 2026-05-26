//! `tool_cap` — single LLM-facing tool that dispatches to one of four
//! coding agents via `crate::cap::CapAgentManager`.
//!
//! The call returns IMMEDIATELY with `status: submitted` once the prompt
//! is queued. Live progress reaches the user's IM channel via
//! `notification_tx`; the final summary is reinjected into the agent's
//! own inbox on a `:cap-followup` sub-session so the LLM can act on the
//! result. Old `tool_acp*` did the same; see
//! `src/cap/runtime.rs::actor_loop` for the new pipeline.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use super::runtime::RunContext;
use crate::cap::{
    AgentKind, CapAgentManager,
    runtime::{InboxTarget, NotifTarget},
};

impl super::runtime::AgentRuntime {
    pub(crate) async fn tool_cap(&self, ctx: &RunContext, args: Value) -> Result<Value> {
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

        // Resolve language for IM notifications. Same logic as the old
        // tools_acp implementation — defaults to "en".
        let lang = self
            .config
            .raw
            .gateway
            .as_ref()
            .and_then(|g| g.language.as_deref())
            .map(crate::i18n::resolve_lang)
            .unwrap_or("en");

        // Build NotifTarget only when we have both the broadcast sender
        // AND a valid target_id (peer_id is the user/group identifier
        // outbound channels route on). Empty target_id means there's no
        // IM channel to push to (e.g. WS-only sessions).
        let notif = match (&self.notification_tx, ctx.peer_id.is_empty()) {
            (Some(tx), false) => Some(NotifTarget {
                tx: tx.clone(),
                target_id: ctx.peer_id.clone(),
                // ctx doesn't carry an is_group flag; default to false
                // (DMs are the common case). Group routing is a future
                // refinement when the upstream RunContext gains the flag.
                is_group: false,
                channel: ctx.channel.clone(),
                lang,
            }),
            _ => None,
        };

        // Build InboxTarget so the completion can be re-injected into
        // the agent's inbox. The `:cap-followup` sub-session keeps the
        // live user-visible session settled.
        let inbox = Some(InboxTarget {
            agent_tx: self.handle.tx.clone(),
            session_key: ctx.session_key.clone(),
            channel: ctx.channel.clone(),
            peer_id: ctx.peer_id.clone(),
            chat_id: ctx.chat_id.clone(),
        });

        tracing::info!(
            agent = agent_str,
            cwd = %cwd.display(),
            task_preview = %task.chars().take(80).collect::<String>(),
            has_notif = notif.is_some(),
            "tool_cap: dispatch (async)"
        );

        let submitted = manager
            .dispatch_async(kind, task.to_owned(), cwd, notif, inbox)
            .await?;

        // The LLM gets back "submitted" so it can ack the user and free
        // the turn. The actual result arrives via IM notification + a
        // followup AgentMessage on `:cap-followup`.
        Ok(json!({
            "agent": agent_str,
            "status": "submitted",
            "session_id": submitted.session_id,
            "output": crate::i18n::t_fmt(
                "acp_queued",
                lang,
                &[("name", kind.display_name())],
            ),
        }))
    }
}
