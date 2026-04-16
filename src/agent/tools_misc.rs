//! Miscellaneous tool handlers — agent management, messaging, sessions,
//! cron, gateway, pairing, doc, and consolidated dispatch.
//!
//! These are `impl AgentRuntime` methods extracted from `runtime.rs` for
//! maintainability. They compile as a split impl block.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use chrono::Utc;
use serde_json::{Value, json};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::prompt_builder::build_base_system_prompt;
use super::registry::{AgentMessage, AgentReply};
use super::runtime::{expand_tilde, persist_agent_to_config, AgentRuntime, RunContext, DEFAULT_TIMEOUT_SECONDS};

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

    // -------------------------------------------------------------------
    // Messaging / session tools
    // -------------------------------------------------------------------

    pub(crate) async fn tool_message(&self, args: Value) -> Result<Value> {
        let target = args["target"]
            .as_str()
            .ok_or_else(|| anyhow!("message: `target` required"))?;
        let text = args["text"]
            .as_str()
            .ok_or_else(|| anyhow!("message: `text` required"))?;
        let channel = args["channel"].as_str().unwrap_or("default");

        // Try to POST to the gateway's own message-send endpoint.
        let port = self.config.gateway.port;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{port}/api/v1/message/send"))
            .json(&json!({
                "channel": channel,
                "target": target,
                "text": text
            }))
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let body: Value = r.json().await.unwrap_or(json!({"ok": true}));
                Ok(json!({
                    "sent": true,
                    "channel": channel,
                    "target": target,
                    "response": body
                }))
            }
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                Err(anyhow!("message: gateway returned {status}: {body}"))
            }
            Err(e) => Err(anyhow!("message: failed to reach gateway: {e}")),
        }
    }

    pub(crate) async fn tool_cron(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow!("cron: `action` required"))?;

        let cron_dir = crate::config::loader::base_dir();
        let cron_path = cron_dir.join("cron").join("jobs.json");

        match action {
            "list" => {
                let jobs = read_cron_jobs(&cron_path).await;
                // Add 1-based index to each job for easier reference by LLMs
                let jobs_with_index: Vec<Value> = jobs
                    .iter()
                    .enumerate()
                    .map(|(i, j)| {
                        let mut indexed = j.clone();
                        indexed["_index"] = json!(i + 1);
                        indexed
                    })
                    .collect();
                Ok(
                    json!({"jobs": jobs_with_index, "hint": "Use index number (#1, #2, etc.) for removal to avoid ID truncation issues"}),
                )
            }
            "add" => {
                let schedule = args["schedule"]
                    .as_str()
                    .ok_or_else(|| anyhow!("cron add: `schedule` required"))?;
                let message = args["message"]
                    .as_str()
                    .ok_or_else(|| anyhow!("cron add: `message` required"))?;
                let name = args["name"].as_str();
                let tz = args["tz"].as_str();
                let agent_id = args["agent_id"].as_str().or(args["agentId"].as_str());

                let mut jobs = read_cron_jobs(&cron_path).await;

                let now_ms = Utc::now().timestamp_millis() as u64;
                let id = Uuid::new_v4().to_string();
                let mut job = json!({
                    "id": id,
                    "agentId": agent_id.unwrap_or("main"),
                    "enabled": true,
                    "createdAtMs": now_ms,
                    "updatedAtMs": now_ms,
                });
                // Schedule: use nested format if tz provided, flat otherwise.
                if let Some(tz_val) = tz {
                    job["schedule"] = json!({"kind": "cron", "expr": schedule, "tz": tz_val});
                } else {
                    job["schedule"] = json!({"kind": "cron", "expr": schedule});
                }
                // Payload in OpenClaw format.
                job["payload"] = json!({"kind": "systemEvent", "text": message});
                if let Some(n) = name {
                    job["name"] = json!(n);
                }

                jobs.push(job);
                write_cron_jobs(&cron_path, &jobs).await?;

                // Notify gateway to reload cron jobs
                let port = self.config.gateway.port;
                let client = reqwest::Client::new();
                if let Err(e) = client
                    .post(format!("http://127.0.0.1:{port}/api/v1/cron/reload"))
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                {
                    debug!(err = %e, "cron add: failed to notify gateway reload");
                }

                Ok(json!({"added": id, "schedule": schedule, "message": message}))
            }
            "remove" => {
                let mut jobs = read_cron_jobs(&cron_path).await;

                // Support both `id` and `index` parameters (prefer index for reliability)
                let removed_job = if let Some(index) = args["index"].as_u64() {
                    // 1-based index
                    let idx = index as usize;
                    if idx == 0 || idx > jobs.len() {
                        return Err(anyhow!(
                            "cron remove: invalid index {} (valid: 1-{})",
                            index,
                            jobs.len()
                        ));
                    }
                    let job = jobs.remove(idx - 1);
                    write_cron_jobs(&cron_path, &jobs).await?;
                    job
                } else if let Some(id) = args["id"].as_str() {
                    let before = jobs.len();
                    jobs.retain(|j| j["id"].as_str() != Some(id));
                    let removed = before - jobs.len();
                    if removed == 0 {
                        return Err(anyhow!("cron remove: job not found with id={}", id));
                    }
                    write_cron_jobs(&cron_path, &jobs).await?;
                    json!({"id": id, "count": removed})
                } else {
                    return Err(anyhow!(
                        "cron remove: `index` or `id` required (index is preferred)"
                    ));
                };

                // Notify gateway to reload cron jobs
                let port = self.config.gateway.port;
                let client = reqwest::Client::new();
                if let Err(e) = client
                    .post(format!("http://127.0.0.1:{port}/api/v1/cron/reload"))
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                {
                    debug!(err = %e, "cron remove: failed to notify gateway reload");
                }

                Ok(json!({"removed": removed_job}))
            }
            "enable" | "disable" => {
                let enabled = action == "enable";
                let mut jobs = read_cron_jobs(&cron_path).await;

                let idx = if let Some(index) = args["index"].as_u64() {
                    let idx = index as usize;
                    if idx == 0 || idx > jobs.len() {
                        return Err(anyhow!(
                            "cron {}: invalid index {} (valid: 1-{})",
                            action, index, jobs.len()
                        ));
                    }
                    idx - 1
                } else if let Some(id) = args["id"].as_str() {
                    match jobs.iter().position(|j| j["id"].as_str() == Some(id)) {
                        Some(pos) => pos,
                        None => return Err(anyhow!("cron {}: job not found with id={}", action, id)),
                    }
                } else {
                    return Err(anyhow!(
                        "cron {}: `index` or `id` required (index is preferred)",
                        action
                    ));
                };

                let id = jobs[idx]["id"].as_str().unwrap_or("?").to_string();
                jobs[idx]["enabled"] = json!(enabled);
                jobs[idx]["updatedAtMs"] = json!(Utc::now().timestamp_millis() as u64);
                write_cron_jobs(&cron_path, &jobs).await?;

                // Notify gateway to reload cron jobs
                let port = self.config.gateway.port;
                let client = reqwest::Client::new();
                if let Err(e) = client
                    .post(format!("http://127.0.0.1:{port}/api/v1/cron/reload"))
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                {
                    debug!(err = %e, "cron {}: failed to notify gateway reload", action);
                }

                Ok(json!({action: id}))
            }
            "edit" => {
                let mut jobs = read_cron_jobs(&cron_path).await;

                let idx = if let Some(index) = args["index"].as_u64() {
                    let idx = index as usize;
                    if idx == 0 || idx > jobs.len() {
                        return Err(anyhow!(
                            "cron edit: invalid index {} (valid: 1-{})",
                            index, jobs.len()
                        ));
                    }
                    idx - 1
                } else if let Some(id) = args["id"].as_str() {
                    match jobs.iter().position(|j| j["id"].as_str() == Some(id)) {
                        Some(pos) => pos,
                        None => return Err(anyhow!("cron edit: job not found with id={}", id)),
                    }
                } else {
                    return Err(anyhow!(
                        "cron edit: `index` or `id` required (index is preferred)"
                    ));
                };

                let id = jobs[idx]["id"].as_str().unwrap_or("?").to_string();
                if let Some(schedule) = args["schedule"].as_str() {
                    let tz = args["tz"].as_str();
                    if let Some(tz_val) = tz {
                        jobs[idx]["schedule"] = json!({"kind": "cron", "expr": schedule, "tz": tz_val});
                    } else {
                        jobs[idx]["schedule"] = json!({"kind": "cron", "expr": schedule});
                    }
                }
                if let Some(message) = args["message"].as_str() {
                    jobs[idx]["payload"] = json!({"kind": "systemEvent", "text": message});
                }
                if let Some(name) = args["name"].as_str() {
                    jobs[idx]["name"] = json!(name);
                }
                if let Some(agent_id) = args["agentId"].as_str().or(args["agent_id"].as_str()) {
                    jobs[idx]["agentId"] = json!(agent_id);
                }
                jobs[idx]["updatedAtMs"] = json!(Utc::now().timestamp_millis() as u64);
                write_cron_jobs(&cron_path, &jobs).await?;

                // Notify gateway to reload cron jobs
                let port = self.config.gateway.port;
                let client = reqwest::Client::new();
                if let Err(e) = client
                    .post(format!("http://127.0.0.1:{port}/api/v1/cron/reload"))
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                {
                    debug!(err = %e, "cron edit: failed to notify gateway reload");
                }

                Ok(json!({"edited": id}))
            }
            other => Err(anyhow!(
                "cron: unsupported action `{other}` (list, add, edit, remove, enable, disable)"
            )),
        }
    }

    // -------------------------------------------------------------------
    // Session tools
    // -------------------------------------------------------------------

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

    // -------------------------------------------------------------------
    // Gateway / pairing / doc tools
    // -------------------------------------------------------------------

    pub(crate) async fn tool_gateway(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow!("gateway: `action` required"))?;

        let port = self.config.gateway.port;
        let version = env!("CARGO_PKG_VERSION");

        match action {
            "status" | "health" => Ok(json!({
                "status": "running",
                "version": version,
                "port": port,
                "agents": self.agents.as_ref().map(|r| r.all().len()).unwrap_or(0),
            })),
            "version" => Ok(json!({
                "version": version,
                "name": "rsclaw",
            })),
            other => Err(anyhow!(
                "gateway: unsupported action `{other}` (status, health, version)"
            )),
        }
    }

    pub(crate) async fn tool_pairing(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow!("pairing: `action` required"))?;

        let port = self.config.gateway.port;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}/api/v1");
        let auth_token = self
            .config
            .gateway
            .auth_token
            .as_deref()
            .unwrap_or_default();

        let auth_header = if auth_token.is_empty() {
            String::new()
        } else {
            format!("Bearer {auth_token}")
        };

        match action {
            "list" => {
                let mut req = client.get(format!("{base}/channels/pairings"));
                if !auth_header.is_empty() {
                    req = req.header("Authorization", &auth_header);
                }
                let resp = req.send().await?;
                let data: Value = resp.json().await?;
                Ok(data)
            }
            "approve" => {
                let code = args["code"]
                    .as_str()
                    .ok_or_else(|| anyhow!("pairing approve: `code` required"))?;
                let mut req = client
                    .post(format!("{base}/channels/pair"))
                    .json(&json!({"code": code}));
                if !auth_header.is_empty() {
                    req = req.header("Authorization", &auth_header);
                }
                let resp = req.send().await?;
                let data: Value = resp.json().await?;
                Ok(data)
            }
            "revoke" => {
                let channel = args["channel"]
                    .as_str()
                    .ok_or_else(|| anyhow!("pairing revoke: `channel` required"))?;
                let peer_id = args["peerId"]
                    .as_str()
                    .ok_or_else(|| anyhow!("pairing revoke: `peerId` required"))?;
                let mut req = client
                    .post(format!("{base}/channels/unpair"))
                    .json(&json!({"channel": channel, "peerId": peer_id}));
                if !auth_header.is_empty() {
                    req = req.header("Authorization", &auth_header);
                }
                let resp = req.send().await?;
                let data: Value = resp.json().await?;
                Ok(data)
            }
            other => Err(anyhow!(
                "pairing: unsupported action `{other}` (list, approve, revoke)"
            )),
        }
    }

    pub(crate) async fn tool_doc(&self, args: Value) -> Result<Value> {
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| anyhow!("doc: `path` required"))?;

        let workspace = self
            .handle
            .config
            .workspace
            .as_deref()
            .or(self.config.agents.defaults.workspace.as_deref())
            .map(expand_tilde)
            .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));

        let pb = std::path::PathBuf::from(path_str);
        let full = if pb.is_absolute() { pb } else { workspace.join(path_str) };
        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        super::doc::handle(&args, &full).await
    }

    // -------------------------------------------------------------------
    // Consolidated tool handlers
    // -------------------------------------------------------------------

    pub(crate) async fn tool_memory_consolidated(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let action = args["action"].as_str().unwrap_or("search");
        match action {
            "search" => self.tool_memory_search(args).await,
            "get" => self.tool_memory_get(args).await,
            "put" => self.tool_memory_put(ctx, args).await,
            "delete" => self.tool_memory_delete(args).await,
            _ => bail!("memory: unknown action '{action}' (search, get, put, delete)"),
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

    pub(crate) async fn tool_channel_consolidated(&self, args: Value) -> Result<Value> {
        let channel_type = args["channel"].as_str().unwrap_or("unknown").to_owned();
        self.tool_channel_actions(&channel_type, args).await
    }

    pub(crate) async fn tool_channel_actions(&self, channel_type: &str, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow!("{channel_type}_actions: `action` required"))?;
        let chat_id = args["chatId"]
            .as_str()
            .or_else(|| args["chat_id"].as_str())
            .unwrap_or("");
        let text = args["text"].as_str().unwrap_or("");
        let message_id = args["messageId"]
            .as_str()
            .or_else(|| args["message_id"].as_str())
            .unwrap_or("");

        Ok(json!({
            "channel": channel_type,
            "action": action,
            "chatId": chat_id,
            "text": text,
            "messageId": message_id,
            "status": "stub",
            "note": format!(
                "{channel_type} action `{action}` received. \
                 Channel-specific API integration is not yet wired — \
                 use the `message` tool for basic send operations."
            )
        }))
    }
}

// ---------------------------------------------------------------------------
// Cron helpers (file-based job storage)
// ---------------------------------------------------------------------------

/// Read cron jobs from the OpenClaw-compatible jobs.json file.
/// Handles both bare array `[...]` and wrapped `{"version":1,"jobs":[...]}` formats.
async fn read_cron_jobs(path: &std::path::Path) -> Vec<Value> {
    let data = tokio::fs::read_to_string(path)
        .await
        .unwrap_or_else(|_| "[]".to_owned());
    // Try wrapped format first.
    if let Ok(wrapper) = serde_json::from_str::<Value>(&data) {
        if let Some(jobs) = wrapper.get("jobs").and_then(|v| v.as_array()) {
            return jobs.clone();
        }
        // Fall through to try as bare array.
        if let Some(arr) = wrapper.as_array() {
            return arr.clone();
        }
    }
    Vec::new()
}

/// Write cron jobs in OpenClaw-compatible format: {"version":1,"jobs":[...]}.
async fn write_cron_jobs(path: &std::path::Path, jobs: &[Value]) -> Result<()> {
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let wrapper = json!({"version": 1, "jobs": jobs});
    tokio::fs::write(path, serde_json::to_string_pretty(&wrapper)?)
        .await
        .map_err(|e| anyhow!("cron: failed to write jobs: {e}"))?;
    Ok(())
}
