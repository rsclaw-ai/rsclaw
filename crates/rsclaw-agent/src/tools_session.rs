//! Session tool handlers — send, list, history, status, and consolidated
//! dispatch.
//!
//! Split from `tools_misc.rs` for maintainability.  All methods live in
//! `impl AgentRuntime` via the split-impl pattern (same struct, different
//! file).

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    registry::{AgentMessage, AgentReply},
    runtime::{AgentRuntime, DEFAULT_TIMEOUT_SECONDS, RunContext},
};

/// Agent id encoded in a session key (`agent:<id>:...`).
fn session_agent(key: &str) -> Option<&str> {
    key.strip_prefix("agent:")?.split(':').next()
}

/// True when `key` is the turn's own session or one of its children
/// (`<session>:send:...`, `<session>:task:...`).
fn is_own_or_child_session(ctx_session: &str, key: &str) -> bool {
    key == ctx_session
        || key
            .strip_prefix(ctx_session)
            .is_some_and(|rest| rest.starts_with(':'))
}

/// Access rule for reading / listing a session from a tool call:
/// - everyone: the current session and its child sessions;
/// - owners: any session of the SAME agent;
/// - owners, other agents: only when `explicit_agent` names that agent.
fn session_access_allowed(
    ctx: &RunContext,
    key: &str,
    explicit_agent: Option<&str>,
) -> bool {
    if is_own_or_child_session(&ctx.session_key, key) {
        return true;
    }
    if !ctx.turn_ctx.trust.is_owner() {
        return false;
    }
    match session_agent(key) {
        Some(a) if a == ctx.agent_id => true,
        Some(a) => explicit_agent == Some(a),
        None => false,
    }
}

impl AgentRuntime {
    async fn tool_sessions_send(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let message = args["message"]
            .as_str()
            .ok_or_else(|| anyhow!("sessions_send: `message` required"))?
            .to_owned();
        let agent_id = args["agentId"]
            .as_str()
            .or_else(|| args["agent_id"].as_str());
        let session_key = args["sessionKey"]
            .as_str()
            .or_else(|| args["session_key"].as_str());

        let registry = self
            .agents
            .as_ref()
            .ok_or_else(|| anyhow!("sessions_send: agent registry not available"))?;

        // Resolve target: agentId is required to avoid accidentally sending to self.
        let target_id = agent_id.ok_or_else(|| {
            anyhow!("sessions_send: `agentId` required (specify which agent to send to)")
        })?;
        let target = registry
            .get(target_id)
            .map_err(|_| anyhow!("sessions_send: agent `{target_id}` not found"))?;

        // An explicit session key must be a child of the current session, or
        // (owners only) one of the target agent's own sessions — never an
        // arbitrary user's conversation.
        if let Some(key) = session_key {
            let allowed = is_own_or_child_session(&ctx.session_key, key)
                || (ctx.turn_ctx.trust.is_owner() && session_agent(key) == Some(target_id));
            if !allowed {
                bail!(
                    "sessions_send: session `{key}` is not accessible from this conversation (use a child of the current session, or omit sessionKey)"
                );
            }
        }
        let child_session = session_key
            .map(|s| s.to_owned())
            .unwrap_or_else(|| format!("{}:send:{}", ctx.session_key, Uuid::new_v4()));

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<AgentReply>();
        let msg = AgentMessage {
            trust: ctx.turn_ctx.trust,
            session_key: child_session.clone(),
            text: message,
            channel: format!("sessions_send:{}", ctx.agent_id),
            peer_id: ctx.agent_id.clone(),
            chat_id: String::new(),
            reply_tx,
            task_id: None,
            context_id: None,
            event_tx: None,
            cancel_token: None,
            input_request_tx: None,
            extra_tools: vec![],
            images: vec![],
            files: vec![],
            account: None,
        };

        target
            .tx
            .send(msg)
            .await
            .map_err(|_| anyhow!("sessions_send: agent `{target_id}` inbox closed"))?;

        let timeout_secs = self
            .config
            .agents
            .defaults
            .timeout_seconds
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS as u32) as u64;

        let reply = tokio::time::timeout(Duration::from_secs(timeout_secs), reply_rx)
            .await
            .map_err(|_| anyhow!("sessions_send: timed out after {timeout_secs}s"))?
            .map_err(|_| anyhow!("sessions_send: reply channel dropped"))?;

        Ok(json!({
            "session_key": child_session,
            "agent_id": target_id,
            "reply": reply.text
        }))
    }

    /// Active message count for a session (what history would return). The
    /// stored `SessionMeta::message_count` is a monotonic seq allocator that
    /// does not drop after compaction, so it is only a fallback.
    fn active_message_count(
        &self,
        session_key: &str,
        meta: Option<&rsclaw_store::redb_store::SessionMeta>,
    ) -> u64 {
        match self.store.db.count_active_messages(session_key) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(session = %session_key, error = %e, "active message count failed");
                meta.map(|m| m.message_count).unwrap_or(0)
            }
        }
    }

    async fn tool_sessions_list(&self, ctx: &RunContext, args: &Value) -> Result<Value> {
        let explicit_agent = args["agentId"]
            .as_str()
            .or_else(|| args["agent_id"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(a) = explicit_agent
            && a != ctx.agent_id
            && !ctx.turn_ctx.trust.is_owner()
        {
            bail!("session list: other agents' sessions are not accessible");
        }
        let sessions = self.store.db.list_sessions()?;
        let list: Vec<Value> = sessions
            .iter()
            .filter(|key| match explicit_agent {
                // Explicit agent: that agent's sessions (owners) — access
                // re-checked below.
                Some(a) => session_agent(key) == Some(a),
                None => true,
            })
            .filter(|key| session_access_allowed(ctx, key, explicit_agent))
            .filter(|key| {
                // Without an explicit agent id, never list other agents.
                explicit_agent.is_some()
                    || is_own_or_child_session(&ctx.session_key, key)
                    || session_agent(key) == Some(ctx.agent_id.as_str())
            })
            .filter_map(|key| {
                let meta = self.store.db.get_session_meta(key).ok().flatten();
                Some(json!({
                    "session_key": key,
                    "message_count": self.active_message_count(key, meta.as_ref()),
                    "last_active": meta.as_ref().map(|m| m.last_active).unwrap_or(0),
                    "created_at": meta.as_ref().map(|m| m.created_at).unwrap_or(0),
                }))
            })
            .collect();
        Ok(json!({"sessions": list, "count": list.len()}))
    }

    async fn tool_sessions_history(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let session_key = args["sessionKey"]
            .as_str()
            .or_else(|| args["session_key"].as_str())
            .map(str::trim)
            .unwrap_or(ctx.session_key.as_str());
        let explicit_agent = args["agentId"]
            .as_str()
            .or_else(|| args["agent_id"].as_str())
            .map(str::trim);
        if !session_access_allowed(ctx, session_key, explicit_agent) {
            bail!(
                "sessions_history: session `{session_key}` is not accessible from this conversation"
            );
        }
        let limit = args["limit"].as_u64().unwrap_or(50) as usize;

        let messages: Vec<_> = self
            .store
            .db
            .load_messages(session_key)?
            .into_iter()
            .map(rsclaw_provider::redact_rsclaw_hidden_value)
            .collect();
        let total = messages.len();
        let truncated: Vec<&Value> = messages.iter().rev().take(limit).collect();

        Ok(json!({
            "session_key": session_key,
            "messages": truncated,
            "total": total,
            "returned": truncated.len()
        }))
    }

    async fn tool_session_status(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let session_key = args["sessionKey"]
            .as_str()
            .or_else(|| args["session_key"].as_str())
            .map(str::trim)
            .unwrap_or(&ctx.session_key);
        let explicit_agent = args["agentId"]
            .as_str()
            .or_else(|| args["agent_id"].as_str())
            .map(str::trim);
        if !session_access_allowed(ctx, session_key, explicit_agent) {
            bail!("session status: session `{session_key}` is not accessible from this conversation");
        }

        let meta = self.store.db.get_session_meta(session_key)?;

        match meta {
            Some(m) => Ok(json!({
                "session_key": session_key,
                "message_count": self.active_message_count(session_key, Some(&m)),
                "last_active": m.last_active,
                "created_at": m.created_at,
                "active": true
            })),
            None => Ok(json!({
                "session_key": session_key,
                "active": false,
                "note": "session not found or no metadata"
            })),
        }
    }

    pub(crate) async fn tool_session_consolidated(
        &self,
        ctx: &RunContext,
        args: Value,
    ) -> Result<Value> {
        // .trim() — v1 block protocol can shard tool_call JSON such that
        // string values land with leading/trailing whitespace; see
        // tool_memory_consolidated for the diagnosis.
        let action = args["action"].as_str().unwrap_or("list").trim();
        match action {
            "send" => self.tool_sessions_send(ctx, args).await,
            "list" => self.tool_sessions_list(ctx, &args).await,
            "history" => self.tool_sessions_history(ctx, args).await,
            "status" => self.tool_session_status(ctx, args).await,
            _ => bail!("session: unknown action '{action}' (send, list, history, status)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{is_own_or_child_session, session_agent};

    #[test]
    fn session_scope_helpers() {
        assert_eq!(session_agent("agent:main:telegram:direct:1"), Some("main"));
        assert_eq!(session_agent("other"), None);
        let own = "agent:main:telegram:direct:1";
        assert!(is_own_or_child_session(own, own));
        assert!(is_own_or_child_session(own, "agent:main:telegram:direct:1:send:x"));
        assert!(!is_own_or_child_session(own, "agent:main:telegram:direct:12"));
        assert!(!is_own_or_child_session(own, "agent:main:telegram:direct:2"));
    }
}
