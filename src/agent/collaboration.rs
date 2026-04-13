//! Multi-agent collaboration modes (AGENTS.md §5 + §20).
//!
//! Three modes:
//! - `Sequential` — agents run one after another, each receives the previous
//!   agent's output as input.
//! - `Parallel` — agents run concurrently via `join_all`; results are collected
//!   into a `Vec<AgentReply>`.
//! - `Orchestrated` — a single "orchestrator" LLM drives the flow by calling
//!   `agent_<id>` tools (A2A dispatch).
//!
//! A2A tool naming convention: `agent_<id>` where `<id>` is the agent's
//! config ID, e.g. `agent_researcher` invokes the "researcher" agent.

use std::sync::Arc;

use anyhow::Result;
use futures::future::join_all;
use serde_json::Value;
use tokio::sync::oneshot;
use tracing::debug;

use super::registry::{AgentHandle, AgentMessage, AgentRegistry, AgentReply};

// ---------------------------------------------------------------------------
// CollabMode
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum CollabMode {
    /// Run agents in order; each receives the previous output.
    Sequential(Vec<String>),
    /// Run all agents concurrently; collect all results.
    Parallel(Vec<String>),
    /// Orchestrator-driven; the LLM decides which agents to call via tools.
    Orchestrated,
}

// ---------------------------------------------------------------------------
// Sequential
// ---------------------------------------------------------------------------

/// Run `agent_ids` sequentially, passing each output to the next agent.
///
/// `initial_input` — the first agent's user message.
/// Returns the final agent's reply.
pub async fn run_sequential(
    agent_ids: &[String],
    initial_input: &str,
    session_key: &str,
    channel: &str,
    peer_id: &str,
    registry: &AgentRegistry,
) -> Result<AgentReply> {
    if agent_ids.is_empty() {
        return Ok(AgentReply {
            text: String::new(),
            is_empty: true,
            tool_calls: None,
            images: vec![],
            files: vec![],
            pending_analysis: None,
            was_preparse: false,
        });
    }

    let mut current_text = initial_input.to_owned();

    for id in agent_ids {
        let handle = registry.get(id)?;
        let reply = invoke_agent(&handle, &current_text, session_key, channel, peer_id).await?;
        debug!(agent = %id, chars = reply.text.len(), "sequential step complete");
        current_text = reply.text;
    }

    Ok(AgentReply {
        is_empty: current_text.is_empty(),
        text: current_text,
        tool_calls: None,
        images: vec![],
        files: vec![],
        pending_analysis: None,
        was_preparse: false,
    })
}

// ---------------------------------------------------------------------------
// Parallel
// ---------------------------------------------------------------------------

/// Run all agents concurrently with the same input.
/// Returns a `Vec<AgentReply>` in the same order as `agent_ids`.
pub async fn run_parallel(
    agent_ids: &[String],
    input: &str,
    session_key: &str,
    channel: &str,
    peer_id: &str,
    registry: &AgentRegistry,
) -> Result<Vec<AgentReply>> {
    if agent_ids.is_empty() {
        return Ok(Vec::new());
    }

    let futures: Vec<_> = agent_ids
        .iter()
        .map(|id| {
            let handle = registry.get(id);
            let input = input.to_owned();
            let sk = session_key.to_owned();
            let ch = channel.to_owned();
            let pid = peer_id.to_owned();
            async move {
                let handle = handle?;
                invoke_agent(&handle, &input, &sk, &ch, &pid).await
            }
        })
        .collect();

    join_all(futures).await.into_iter().collect()
}

// ---------------------------------------------------------------------------
// Orchestrated  —  A2A tool dispatch
// ---------------------------------------------------------------------------

/// A2A (Agent-to-Agent) tool dispatcher used inside `AgentRuntime`.
///
/// When the orchestrator LLM emits a tool call named `agent_<id>`,
/// this function routes the call to the target agent and returns its reply
/// as a JSON `Value`.
///
/// Input schema expected by an `agent_<id>` tool:
/// ```json
/// { "message": "the sub-task description" }
/// ```
/// Output is the text reply from the sub-agent as a JSON string.
pub async fn dispatch_a2a(
    agent_id: &str,
    args: Value,
    session_key: &str,
    channel: &str,
    peer_id: &str,
    registry: &AgentRegistry,
) -> Result<Value> {
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_else(|| args.to_string().as_str().to_owned().leak()); // fallback: full JSON

    // Use a child session key so A2A calls don't pollute the parent session.
    let child_session_key = format!("{session_key}:a2a:{agent_id}");

    let handle = registry.get(agent_id)?;
    debug!(
        orchestrator_session = %session_key,
        sub_agent = %agent_id,
        "A2A dispatch"
    );

    let reply = invoke_agent(&handle, message, &child_session_key, channel, peer_id).await?;

    Ok(Value::String(reply.text))
}

/// Build the `ToolDef` list for all agents in the registry,
/// so the orchestrator LLM can call them by name.
pub fn build_a2a_tool_defs(
    registry: &AgentRegistry,
    orchestrator_id: &str,
) -> Vec<crate::provider::ToolDef> {
    registry
        .all()
        .into_iter()
        .filter(|h| h.id != orchestrator_id) // don't expose self
        .map(|h| {
            let desc = format!(
                "Invoke sub-agent '{}'. Provide a 'message' parameter with the task.",
                h.id
            );
            crate::provider::ToolDef {
                name: format!("agent_{}", h.id),
                description: desc,
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message": {
                            "type": "string",
                            "description": "The task or question for the sub-agent"
                        }
                    },
                    "required": ["message"]
                }),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Shared helper
// ---------------------------------------------------------------------------

/// Send a message to one agent and await its reply.
pub(crate) async fn invoke_agent(
    handle: &Arc<AgentHandle>,
    text: &str,
    session_key: &str,
    channel: &str,
    peer_id: &str,
) -> Result<AgentReply> {
    let (reply_tx, reply_rx) = oneshot::channel();

    handle
        .tx
        .send(AgentMessage {
            session_key: session_key.to_owned(),
            text: text.to_owned(),
            channel: channel.to_owned(),
            peer_id: peer_id.to_owned(),
            chat_id: String::new(),
            reply_tx,
            extra_tools: vec![],
            images: vec![],
            files: vec![],
        })
        .await
        .map_err(|_| anyhow::anyhow!("agent `{}` mailbox closed", handle.id))?;

    reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("agent `{}` dropped reply sender", handle.id))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {

    use super::*;
    use crate::{
        agent::registry::AgentRegistry,
        config::{
            runtime::{
                AgentsRuntime, ChannelRuntime, ExtRuntime, GatewayRuntime, ModelRuntime,
                OpsRuntime, RuntimeConfig,
            },
            schema::{AgentEntry, BindMode, GatewayMode, ReloadMode, SessionConfig},
        },
    };

    fn make_registry_with_echo(ids: &[&str]) -> AgentRegistry {
        // Build a minimal RuntimeConfig with stub agents.
        let agents: Vec<AgentEntry> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| AgentEntry {
                id: id.to_string(),
                name: None,
                default: if i == 0 { Some(true) } else { None },
                workspace: None,
                model: None,
                lane: None,
                lane_concurrency: None,
                group_chat: None,
                channels: None,
                commands: None,
                allowed_commands: None,
                opencode: None,
                claudecode: None,
                agent_dir: None,
                system: None,
            })
            .collect();

        let cfg = RuntimeConfig {
            gateway: GatewayRuntime {
                port: 18888,
                mode: GatewayMode::Local,
                bind: BindMode::Loopback,
                bind_address: None,
                reload: ReloadMode::Hybrid,
                auth_token: None,
                allow_tailscale: false,
                channel_health_check_minutes: 5,
                channel_stale_event_threshold_minutes: 30,
                channel_max_restarts_per_hour: 10,
                auth_token_configured: false,
                auth_token_is_plaintext: false,
                user_agent: None,
            },
            agents: AgentsRuntime {
                defaults: Default::default(),
                list: agents,
                bindings: vec![],
                external: vec![],
            },
            channel: ChannelRuntime {
                channels: Default::default(),
                session: SessionConfig {
                    dm_scope: None,
                    thread_bindings: None,
                    reset: None,
                    identity_links: None,
                    maintenance: None,
                },
            },
            model: ModelRuntime {
                models: None,
                auth: None,
            },
            ext: ExtRuntime {
                tools: None,
                skills: None,
                plugins: None,
            },
            ops: OpsRuntime {
                cron: None,
                hooks: None,
                sandbox: None,
                logging: None,
                secrets: None,
            },
            raw: Default::default(),
        };

        AgentRegistry::from_config(&cfg)
    }

    #[test]
    fn collab_mode_variants_exist() {
        let _s = CollabMode::Sequential(vec!["a".into(), "b".into()]);
        let _p = CollabMode::Parallel(vec!["a".into()]);
        let _o = CollabMode::Orchestrated;
    }

    #[test]
    fn build_a2a_tools_excludes_self() {
        let reg = make_registry_with_echo(&["main", "researcher", "writer"]);
        let tools = build_a2a_tool_defs(&reg, "main");
        // main excluded; researcher + writer included
        assert_eq!(tools.len(), 2);
        let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"agent_researcher"));
        assert!(names.contains(&"agent_writer"));
        assert!(!names.contains(&"agent_main"));
    }

    #[test]
    fn a2a_tool_schema_has_message_param() {
        let reg = make_registry_with_echo(&["main", "sub"]);
        let tools = build_a2a_tool_defs(&reg, "main");
        let props = &tools[0].parameters["properties"];
        assert!(props.get("message").is_some());
    }
}
