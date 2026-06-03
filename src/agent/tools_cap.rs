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
    AgentKind, CapAgentManager, CapLiveManager,
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

    /// `cap_live` — interactive synchronous call into a warm cap driver.
    /// Opens a new session when `session_id` is missing/empty; otherwise
    /// re-uses the existing session. Waits for the driver's full reply
    /// before returning so the LLM can chain follow-ups in the same turn.
    pub(crate) async fn tool_cap_live(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let agent_str = args["agent"]
            .as_str()
            .ok_or_else(|| anyhow!("tool_cap_live: `agent` required"))?;
        let kind = AgentKind::from_str(agent_str)
            .ok_or_else(|| anyhow!("tool_cap_live: unknown agent `{agent_str}`"))?;
        let task = args["task"]
            .as_str()
            .ok_or_else(|| anyhow!("tool_cap_live: `task` required"))?;
        if task.trim().is_empty() {
            return Err(anyhow!(
                "tool_cap_live: `task` is empty — pass the prompt text the agent should \
                 act on. If you are continuing a session, the new turn's instruction \
                 belongs in `task`; do not send a blank message."
            ));
        }
        let session_id = args["session_id"].as_str().map(|s| s.to_owned());
        let cwd = args["cwd"]
            .as_str()
            .map(|s| std::path::PathBuf::from(crate::agent::runtime::expand_tilde(s)))
            .unwrap_or_else(|| self.default_workspace());

        let manager: &CapLiveManager = self
            .cap_live_manager
            .as_ref()
            .ok_or_else(|| anyhow!("tool_cap_live: CapLiveManager not initialised"))?;

        let lang = self
            .config
            .raw
            .gateway
            .as_ref()
            .and_then(|g| g.language.as_deref())
            .map(crate::i18n::resolve_lang)
            .unwrap_or("en");

        // Optional IM notification (same shape as cap task mode — surfaces
        // the driver's inner tool-call progress + completion summary live
        // to the user). `cap-followup` filtering does NOT apply here
        // because this is a synchronous LLM-mediated call, not a passive
        // re-injection.
        let notif = match (&self.notification_tx, ctx.peer_id.is_empty()) {
            (Some(tx), false) => Some(NotifTarget {
                tx: tx.clone(),
                target_id: ctx.peer_id.clone(),
                is_group: false,
                channel: ctx.channel.clone(),
                lang,
            }),
            _ => None,
        };

        tracing::info!(
            agent = agent_str,
            session_id = ?session_id,
            cwd = %cwd.display(),
            task_preview = %task.chars().take(80).collect::<String>(),
            "tool_cap_live: dispatch (sync)"
        );

        let result = manager
            .dispatch_sync(kind, session_id, task.to_owned(), cwd, notif)
            .await?;

        Ok(json!({
            "agent": result.agent_kind.as_str(),
            "session_id": result.session_id,
            "output": result.output,
        }))
    }

    /// `cap_live_end` — release a live cap session and tear down its driver.
    pub(crate) async fn tool_cap_live_end(&self, _ctx: &RunContext, args: Value) -> Result<Value> {
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| anyhow!("tool_cap_live_end: `session_id` required"))?;
        let manager: &CapLiveManager = self
            .cap_live_manager
            .as_ref()
            .ok_or_else(|| anyhow!("tool_cap_live_end: CapLiveManager not initialised"))?;
        manager.end_session(session_id).await?;
        Ok(json!({ "session_id": session_id, "status": "closed" }))
    }

    /// `cap_bind_sticky` — natural-language equivalent of the `/cap <agent>`
    /// slash command. The LLM calls this when the user expresses intent to
    /// hand the conversation off to a coding subagent ("接下来让 claudecode
    /// 来", "switch to codex for the next few turns"). Opens a new cap_live
    /// session and binds it to the current IM session_key so subsequent
    /// user messages bypass the main LLM entirely.
    ///
    /// Returns the freshly minted `session_id` for traceability, but the
    /// LLM should NOT need to pass it back anywhere — sticky direct mode
    /// is consumed at the runtime layer (see
    /// `AgentRuntime::run_turn` → sticky bypass branch). Use this
    /// alongside `cap_unbind_sticky` to wind it down.
    pub(crate) async fn tool_cap_bind_sticky(
        &self,
        ctx: &RunContext,
        args: Value,
    ) -> Result<Value> {
        let agent_str = args["agent"]
            .as_str()
            .ok_or_else(|| anyhow!("tool_cap_bind_sticky: `agent` required"))?;
        let kind = AgentKind::from_str(agent_str)
            .ok_or_else(|| anyhow!("tool_cap_bind_sticky: unknown agent `{agent_str}`"))?;
        let cwd = args["cwd"]
            .as_str()
            .map(|s| std::path::PathBuf::from(crate::agent::runtime::expand_tilde(s)))
            .unwrap_or_else(|| self.default_workspace());

        let manager: &CapLiveManager = self
            .cap_live_manager
            .as_ref()
            .ok_or_else(|| anyhow!("tool_cap_bind_sticky: CapLiveManager not initialised"))?;

        if ctx.session_key.is_empty() {
            return Err(anyhow!(
                "tool_cap_bind_sticky: empty session_key — sticky binding only meaningful inside \
                 a real IM/WS session"
            ));
        }

        let session_id = manager.open_session(kind, cwd).await?;
        manager
            .bind_sticky(ctx.session_key.clone(), session_id.clone(), kind)
            .await;

        tracing::info!(
            target: "cap",
            session_id = %session_id,
            agent = kind.as_str(),
            im_session_key = %ctx.session_key,
            "cap_live sticky bind via LLM tool"
        );

        let lang = self
            .config
            .raw
            .gateway
            .as_ref()
            .and_then(|g| g.language.as_deref())
            .map(crate::i18n::resolve_lang)
            .unwrap_or("en");
        Ok(json!({
            "agent": kind.as_str(),
            "session_id": session_id,
            "status": "bound",
            "output": crate::i18n::t_fmt(
                "cap_bound",
                lang,
                &[
                    ("agent", kind.display_name()),
                    ("sid", &session_id[..8.min(session_id.len())]),
                ],
            ),
        }))
    }

    /// `cap_unbind_sticky` — natural-language `/cap-exit`. The LLM calls
    /// this when the user signals "back to normal" / "stop using
    /// claudecode" / "release". Releases the sticky binding on the
    /// current IM session AND tears down the underlying live driver.
    /// No-op (returns `status: "not_bound"`) if nothing was bound — the
    /// LLM can call it defensively.
    pub(crate) async fn tool_cap_unbind_sticky(
        &self,
        ctx: &RunContext,
        _args: Value,
    ) -> Result<Value> {
        let manager: &CapLiveManager = self
            .cap_live_manager
            .as_ref()
            .ok_or_else(|| anyhow!("tool_cap_unbind_sticky: CapLiveManager not initialised"))?;
        let lang = self
            .config
            .raw
            .gateway
            .as_ref()
            .and_then(|g| g.language.as_deref())
            .map(crate::i18n::resolve_lang)
            .unwrap_or("en");
        let Some((sid, kind)) = manager.unbind_sticky(&ctx.session_key).await else {
            return Ok(json!({
                "status": "not_bound",
                "output": crate::i18n::t("cap_no_active", lang),
            }));
        };
        let _ = manager.end_session(&sid).await;
        tracing::info!(
            target: "cap",
            session_id = %sid,
            agent = kind.as_str(),
            im_session_key = %ctx.session_key,
            "cap_live sticky unbind via LLM tool"
        );
        Ok(json!({
            "agent": kind.as_str(),
            "session_id": sid,
            "status": "released",
            "output": crate::i18n::t_fmt(
                "cap_session_closed",
                lang,
                &[("agent", kind.display_name())],
            ),
        }))
    }
}
