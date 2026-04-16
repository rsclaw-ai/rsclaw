//! Agent management tool handlers — spawn, task, send, list, and consolidated dispatch.
//!
//! Split from `tools_misc.rs` for maintainability.  All methods live in
//! `impl AgentRuntime` via the split-impl pattern (same struct, different file).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use tracing::{info, warn};

use super::prompt_builder::build_base_system_prompt;
use super::registry::{AgentMessage, AgentReply};
use super::runtime::{persist_agent_to_config, AgentRuntime, RunContext, DEFAULT_TIMEOUT_SECONDS};

impl AgentRuntime {
    // -------------------------------------------------------------------
    // Agent-related tools
    // -------------------------------------------------------------------

    /// Build a full system prompt for a sub-agent by combining the shared base
    /// (date, platform, safety rules, agent loop guidance) with the role-specific
    /// description provided by the main agent.
    fn build_subagent_system_prompt(&self, role_desc: &str) -> String {
        let base_parts = build_base_system_prompt(&self.config.raw);
        let mut prompt = base_parts.join("\n\n");
        prompt.push_str("\n\n## Your Role\n");
        prompt.push_str(role_desc);
        prompt.push_str(
            "\n\n## Sub-Agent Guidelines\n\
             - You are a sub-agent working on a delegated task. Focus on the task and return results.\n\
             - Use the tools available to you. If a tool is not in your toolset, find an alternative.\n\
             - Be concise in your reply — the main agent will relay your output to the user.\n\
             - If the task is unclear or impossible, explain why instead of looping.",
        );
        prompt
    }

    async fn tool_agent_spawn(&self, args: Value) -> Result<Value> {
        let spawner = self
            .spawner
            .as_ref()
            .ok_or_else(|| anyhow!("agent_spawn: spawner not available"))?;

        let id = args["id"]
            .as_str()
            .ok_or_else(|| anyhow!("agent_spawn: `id` required"))?
            .to_owned();
        let model = args["model"].as_str()
            .filter(|s| !s.is_empty() && *s != "default")
            .unwrap_or(&self.resolve_model_name())
            .to_owned();
        let system = args["system"]
            .as_str()
            .ok_or_else(|| anyhow!("agent_spawn: `system` required"))?
            .to_owned();
        let toolset_str = args["toolset"]
            .as_str()
            .unwrap_or("standard")
            .to_owned();
        let channels: Option<Vec<String>> = args["channels"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_owned())).collect());

        use crate::config::schema::{AgentEntry, ModelConfig};

        let entry = AgentEntry {
            id: id.clone(),
            default: Some(false),
            workspace: Some(crate::config::loader::path_to_forward_slash(
                &crate::config::loader::base_dir().join(format!("workspace-{id}")),
            )),
            model: Some(ModelConfig {
                primary: Some(model),
                fallbacks: None,
                image: None,
                image_fallbacks: None,
                thinking: None,
                tools_enabled: None,
                toolset: Some(toolset_str.clone()),
                tools: None,
                context_tokens: None,
                max_tokens: None,
            }),
            lane: None,
            lane_concurrency: None,
            group_chat: None,
            channels: channels.clone(),
            name: None,
            agent_dir: None,
            system: None,
            commands: None,
            allowed_commands: None,
            opencode: None,
            claudecode: None,
        };

        spawner.spawn_agent(entry.clone())?;

        // Write full system prompt (base + role) as SOUL.md in the new agent's workspace.
        let ws_path = crate::config::loader::base_dir().join(format!("workspace-{id}"));
        if let Err(e) = tokio::fs::create_dir_all(&ws_path).await {
            warn!("agent_spawn: failed to create workspace for {id}: {e:#}");
        }
        let soul_path = ws_path.join("SOUL.md");
        let full_prompt = self.build_subagent_system_prompt(&system);
        if let Err(e) = tokio::fs::write(&soul_path, format!("# Agent: {id}\n\n{full_prompt}\n")).await {
            warn!("agent_spawn: failed to write SOUL.md for {id}: {e:#}");
        }

        // Persist to config file by default (user-created agents survive restart).
        // Pass persistent=false only for temporary task-delegation agents.
        let persistent = args["persistent"].as_bool().unwrap_or(true);
        if persistent {
            if let Err(e) = persist_agent_to_config(&entry).await {
                warn!("agent_spawn: failed to persist to config: {e:#}");
            }
        }

        let needs_restart = persistent && channels.is_some();
        Ok(json!({
            "spawned": id,
            "model": args["model"],
            "persistent": persistent,
            "channels": channels,
            "needs_restart": needs_restart,
            "status": if needs_restart { "saved — restart gateway to bind channels" } else { "ready" }
        }))
    }

    /// One-shot task agent: spawn -> send message -> return immediately.
    ///
    /// The task runs in the background. When the sub-agent completes, the
    /// result is stored in `pending_task_results` and injected into the
    /// main agent's session on the next turn. This ensures the main agent
    /// is NEVER blocked by sub-agent work.
    async fn tool_agent_task(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let spawner = self
            .spawner
            .as_ref()
            .ok_or_else(|| anyhow!("agent_task: spawner not available"))?;

        let model = args["model"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.resolve_model_name())
            .to_owned();

        let system = args["system"]
            .as_str()
            .ok_or_else(|| anyhow!("agent_task: `system` required"))?
            .to_owned();

        let message = args["message"]
            .as_str()
            .ok_or_else(|| anyhow!("agent_task: `message` required"))?
            .to_owned();

        let toolset_str = args["toolset"]
            .as_str()
            .unwrap_or("standard")
            .to_owned();

        let short_id = &uuid::Uuid::new_v4().to_string()[..8];
        let id = format!("task-{short_id}");
        let base = crate::config::loader::base_dir();
        let parent_ws = self.handle.config.workspace
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| base.join("workspace"));
        let ws_path = parent_ws.join(format!("task-{short_id}"));
        use crate::config::schema::{AgentEntry, ModelConfig};
        let entry = AgentEntry {
            id: id.clone(),
            default: Some(false),
            workspace: Some(crate::config::loader::path_to_forward_slash(&ws_path)),
            model: Some(ModelConfig {
                primary: Some(model),
                fallbacks: None,
                image: None,
                image_fallbacks: None,
                thinking: None,
                tools_enabled: None,
                toolset: Some(toolset_str.clone()),
                tools: None,
                context_tokens: None,
                max_tokens: None,
            }),
            lane: None,
            lane_concurrency: None,
            group_chat: None,
            channels: None,
            name: None,
            agent_dir: None,
            system: None,
            commands: None,
            allowed_commands: None,
            opencode: None,
            claudecode: None,
        };

        spawner.spawn_agent(entry)?;

        // Write full system prompt (base + role) as SOUL.md.
        if let Err(e) = tokio::fs::create_dir_all(&ws_path).await {
            warn!("agent_task: failed to create workspace for {id}: {e:#}");
        }
        let full_prompt = self.build_subagent_system_prompt(&system);
        if let Err(e) = tokio::fs::write(ws_path.join("SOUL.md"), format!("# Agent: {id}\n\n{full_prompt}\n")).await {
            warn!("agent_task: failed to write SOUL.md for {id}: {e:#}");
        }

        // Send message to the task agent.
        let registry = self
            .agents
            .as_ref()
            .ok_or_else(|| anyhow!("agent_task: agent registry not available"))?;
        let target = registry.get(&id)?;
        let task_session = format!("{}:task:{short_id}", ctx.session_key);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<AgentReply>();
        let msg = AgentMessage {
            session_key: task_session,
            text: message.clone(),
            channel: format!("task:{}", ctx.agent_id),
            peer_id: ctx.agent_id.clone(),
            chat_id: String::new(),
            reply_tx,
            extra_tools: vec![],
            images: vec![],
            files: vec![],
        };
        target.tx.send(msg).await.map_err(|_| anyhow!("agent_task: agent inbox closed"))?;

        // Spawn background worker to wait for reply and store result.
        // Main agent returns IMMEDIATELY — never blocked.
        let pending = Arc::clone(&self.pending_task_results);
        let session_key = ctx.session_key.clone();
        let task_id = id.clone();
        let agents = self.agents.as_ref().map(Arc::clone);
        let timeout_secs = self
            .config
            .agents
            .defaults
            .timeout_seconds
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS as u32) as u64;
        let task_timeout = timeout_secs.min(300); // up to 5 min for background tasks

        tokio::spawn(async move {
            let result_text = match tokio::time::timeout(
                Duration::from_secs(task_timeout),
                reply_rx,
            ).await {
                Ok(Ok(reply)) => reply.text,
                Ok(Err(_)) => "[task agent channel closed unexpectedly]".to_owned(),
                Err(_) => format!("[task {task_id} timed out after {task_timeout}s]"),
            };

            // Store result for main agent to pick up.
            if let Ok(mut guard) = pending.lock() {
                guard.push((task_id.clone(), session_key, result_text));
            }

            // Cleanup: remove agent from registry, delete workspace.
            if let Some(reg) = agents {
                reg.remove_handle(&task_id);
            }
            let _ = tokio::fs::remove_dir_all(&ws_path).await;
            info!(task = %task_id, "async task agent completed and cleaned up");
        });

        Ok(json!({
            "task_id": id,
            "status": "dispatched",
            "toolset": toolset_str,
            "message": message,
            "note": "Task is running in the background. Results will be available on your next turn. You can continue with other work."
        }))
    }

    /// Send a message to a persistent (spawned) sub-agent. Non-blocking.
    async fn tool_agent_send(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let target_id = args["id"]
            .as_str()
            .ok_or_else(|| anyhow!("agent_send: `id` required"))?
            .to_owned();
        let message = args["message"]
            .as_str()
            .ok_or_else(|| anyhow!("agent_send: `message` required"))?
            .to_owned();

        let registry = self
            .agents
            .as_ref()
            .ok_or_else(|| anyhow!("agent_send: agent registry not available"))?;
        let target = registry.get(&target_id)?;

        let short_id = &uuid::Uuid::new_v4().to_string()[..8];
        let send_session = format!("{}:send:{short_id}", ctx.session_key);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<AgentReply>();
        let msg = AgentMessage {
            session_key: send_session,
            text: message.clone(),
            channel: format!("send:{}", ctx.agent_id),
            peer_id: ctx.agent_id.clone(),
            chat_id: String::new(),
            reply_tx,
            extra_tools: vec![],
            images: vec![],
            files: vec![],
        };
        target.tx.send(msg).await.map_err(|_| anyhow!("agent_send: agent '{target_id}' inbox closed"))?;

        // Background: wait for reply and store in pending results.
        let pending = Arc::clone(&self.pending_task_results);
        let session_key = ctx.session_key.clone();
        let timeout_secs = self
            .config
            .agents
            .defaults
            .timeout_seconds
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS as u32) as u64;
        let send_timeout = timeout_secs.min(300);
        let send_id = format!("send-{target_id}-{short_id}");
        let send_id_bg = send_id.clone();
        let target_id_bg = target_id.clone();

        tokio::spawn(async move {
            let result_text = match tokio::time::timeout(
                Duration::from_secs(send_timeout),
                reply_rx,
            ).await {
                Ok(Ok(reply)) => reply.text,
                Ok(Err(_)) => format!("[agent {target_id_bg} channel closed]"),
                Err(_) => format!("[agent {target_id_bg} timed out after {send_timeout}s]"),
            };
            if let Ok(mut guard) = pending.lock() {
                guard.push((send_id_bg, session_key, result_text));
            }
        });

        Ok(json!({
            "send_id": send_id,
            "target": target_id,
            "status": "sent",
            "note": "Message sent to agent. Reply will be available on your next turn."
        }))
    }

    async fn tool_agent_list(&self) -> Result<Value> {
        let agents = match &self.agents {
            Some(reg) => reg
                .all()
                .iter()
                .map(|h| {
                    json!({
                        "id": h.id,
                        "model": h.config.model.as_ref()
                            .and_then(|m| m.primary.as_deref())
                            .unwrap_or("unknown"),
                    })
                })
                .collect::<Vec<_>>(),
            None => vec![],
        };
        Ok(json!({"agents": agents}))
    }

    pub(crate) async fn tool_agent_consolidated(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let action = args["action"].as_str().unwrap_or("list");
        match action {
            "spawn" => self.tool_agent_spawn(args).await,
            "task" => self.tool_agent_task(ctx, args).await,
            "send" => self.tool_agent_send(ctx, args).await,
            "list" => self.tool_agent_list().await,
            "kill" => {
                let id = args["id"]
                    .as_str()
                    .ok_or_else(|| anyhow!("agent kill: `id` required"))?;
                Ok(json!({
                    "action": "kill",
                    "id": id,
                    "note": "agent termination not yet implemented; agent will stop on next idle timeout"
                }))
            }
            _ => bail!("agent: unknown action '{action}' (spawn, task, list, kill)"),
        }
    }
}
