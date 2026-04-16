//! Session tool handlers — send, list, history, status, and consolidated dispatch.
//!
//! Split from `tools_misc.rs` for maintainability.  All methods live in
//! `impl AgentRuntime` via the split-impl pattern (same struct, different file).

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use uuid::Uuid;

use super::registry::{AgentMessage, AgentReply};
use super::runtime::{AgentRuntime, RunContext, DEFAULT_TIMEOUT_SECONDS};

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

        // Resolve target: if agentId given, send to that agent; otherwise use
        // session_key to find an agent.
        let target_id = agent_id.unwrap_or(&ctx.agent_id);
        let target = registry
            .get(target_id)
            .map_err(|_| anyhow!("sessions_send: agent `{target_id}` not found"))?;

        let child_session = session_key
            .map(|s| s.to_owned())
            .unwrap_or_else(|| format!("{}:send:{}", ctx.session_key, Uuid::new_v4()));

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<AgentReply>();
        let msg = AgentMessage {
            session_key: child_session.clone(),
            text: message,
            channel: format!("sessions_send:{}", ctx.agent_id),
            peer_id: ctx.agent_id.clone(),
            chat_id: String::new(),
            reply_tx,
            extra_tools: vec![],
            images: vec![],
            files: vec![],
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

    async fn tool_sessions_list(&self) -> Result<Value> {
        let sessions = self.store.db.list_sessions()?;
        let list: Vec<Value> = sessions
            .iter()
            .filter_map(|key| {
                let meta = self.store.db.get_session_meta(key).ok().flatten();
                Some(json!({
                    "session_key": key,
                    "message_count": meta.as_ref().map(|m| m.message_count).unwrap_or(0),
                    "last_active": meta.as_ref().map(|m| m.last_active).unwrap_or(0),
                    "created_at": meta.as_ref().map(|m| m.created_at).unwrap_or(0),
                }))
            })
            .collect();
        Ok(json!({"sessions": list, "count": list.len()}))
    }

    async fn tool_sessions_history(&self, args: Value) -> Result<Value> {
        let session_key = args["sessionKey"]
            .as_str()
            .or_else(|| args["session_key"].as_str())
            .ok_or_else(|| anyhow!("sessions_history: `sessionKey` required"))?;
        let limit = args["limit"].as_u64().unwrap_or(50) as usize;

        let messages = self.store.db.load_messages(session_key)?;
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
            .unwrap_or(&ctx.session_key);

        let meta = self.store.db.get_session_meta(session_key)?;

        match meta {
            Some(m) => Ok(json!({
                "session_key": session_key,
                "message_count": m.message_count,
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

    pub(crate) async fn tool_session_consolidated(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let action = args["action"].as_str().unwrap_or("list");
        match action {
            "send" => self.tool_sessions_send(ctx, args).await,
            "list" => self.tool_sessions_list().await,
            "history" => self.tool_sessions_history(args).await,
            "status" => self.tool_session_status(ctx, args).await,
            _ => bail!("session: unknown action '{action}' (send, list, history, status)"),
        }
    }
}
