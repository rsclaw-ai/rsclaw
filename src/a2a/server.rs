//! A2A server handlers.
//!
//! Implements:
//!   GET  /.well-known/agent.json  — Agent Card discovery (Level 1)
//!   POST /api/v1/a2a              — JSON-RPC 2.0 task receiver (Level 2)

use axum::{Json, extract::State, response::IntoResponse};
use serde_json::json;
use tokio::sync::oneshot;
use tracing::info;
use uuid::Uuid;

use crate::{
    a2a::{
        A2aPart, AgentCapabilities, AgentCard, AgentSkill, JsonRpcRequest, JsonRpcResponse,
        TaskSendParams,
    },
    agent::{AgentMessage, AgentReply},
    config::schema::BindMode,
    server::AppState,
};

// ---------------------------------------------------------------------------
// Agent Card — GET /.well-known/agent.json
// ---------------------------------------------------------------------------

/// Returns an Agent Card describing this gateway's agents and capabilities.
/// Clients use this for discovery before sending A2A tasks.
pub async fn agent_card_handler(State(state): State<AppState>) -> impl IntoResponse {
    let host = match state.config.gateway.bind {
        BindMode::Loopback => "127.0.0.1",
        BindMode::All | BindMode::Lan | BindMode::Auto | BindMode::Custom | BindMode::Tailnet => {
            "0.0.0.0"
        }
    };
    let base_url = format!("http://{}:{}/api/v1/a2a", host, state.config.gateway.port,);

    let skills: Vec<AgentSkill> = state
        .agents
        .all()
        .into_iter()
        .map(|h| AgentSkill {
            id: h.id.clone(),
            name: h.id.clone(),
            description: None,
            input_modes: vec!["text/plain".to_owned()],
            output_modes: vec!["text/plain".to_owned()],
        })
        .collect();

    let card = AgentCard {
        protocol_version: "0.3".to_owned(),
        name: "rsclaw".to_owned(),
        description: Some("OpenClaw-compatible multi-agent AI gateway".to_owned()),
        url: base_url,
        capabilities: AgentCapabilities {
            streaming: true,
            push_notifications: false,
        },
        default_input_modes: vec!["text/plain".to_owned()],
        default_output_modes: vec!["text/plain".to_owned()],
        skills,
    };

    Json(card)
}

// ---------------------------------------------------------------------------
// JSON-RPC task receiver — POST /api/v1/a2a
// ---------------------------------------------------------------------------

/// JSON-RPC 2.0 endpoint. Accepts `tasks/send` method.
/// Dispatches to the target agent and returns the reply.
pub async fn a2a_rpc_handler(
    State(state): State<AppState>,
    Json(req): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    let id = req.id.clone();

    match req.method.as_str() {
        "tasks/send" => {
            let params: TaskSendParams = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    return Json(JsonRpcResponse::err(
                        id,
                        -32602,
                        format!("invalid params: {e}"),
                    ));
                }
            };

            // Extract text from message parts.
            let text = params
                .message
                .parts
                .iter()
                .find_map(|p| {
                    if let A2aPart::Text { text } = p {
                        Some(text.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            if text.is_empty() {
                return Json(JsonRpcResponse::err(id, -32602, "no text part in message"));
            }

            // Resolve target agent (from metadata.agentId or the registry default).
            let agent_id = params
                .metadata
                .as_ref()
                .and_then(|m| m["agentId"].as_str().map(str::to_owned));

            let handle = if let Some(ref aid) = agent_id {
                match state.agents.get(aid) {
                    Ok(h) => h,
                    Err(_) => {
                        return Json(JsonRpcResponse::err(
                            id,
                            -32001,
                            format!("agent not found: {aid}"),
                        ));
                    }
                }
            } else {
                match state.agents.default_agent() {
                    Ok(h) => h,
                    Err(e) => {
                        return Json(JsonRpcResponse::err(
                            id,
                            -32001,
                            format!("no default agent: {e}"),
                        ));
                    }
                }
            };

            let session_key = params
                .session_id
                .unwrap_or_else(|| format!("a2a:{}", Uuid::new_v4()));

            let (reply_tx, reply_rx) = oneshot::channel::<AgentReply>();
            let msg = AgentMessage {
                session_key: session_key.clone(),
                text,
                channel: "a2a".to_owned(),
                peer_id: "a2a-client".to_owned(),
                chat_id: String::new(),
                reply_tx,
                extra_tools: vec![],
                images: vec![],
                files: vec![],
            };

            if handle.tx.send(msg).await.is_err() {
                return Json(JsonRpcResponse::err(id, -32603, "agent inbox closed"));
            }

            let timeout_secs = state.config.agents.defaults.timeout_seconds.unwrap_or(600) as u64;

            let reply =
                match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), reply_rx)
                    .await
                {
                    Ok(Ok(r)) => r,
                    Ok(Err(_)) => {
                        return Json(JsonRpcResponse::err(id, -32603, "reply channel dropped"));
                    }
                    Err(_) => {
                        return Json(JsonRpcResponse::err(
                            id,
                            -32000,
                            format!("agent timed out after {timeout_secs}s"),
                        ));
                    }
                };

            let task_id = params.id;
            let result = json!({
                "id": task_id,
                "sessionId": session_key,
                "status": { "state": "completed" },
                "artifacts": [{
                    "parts": [{ "type": "text", "text": reply.text }]
                }]
            });

            info!(task_id, agent = %handle.id, "A2A task completed");
            Json(JsonRpcResponse::ok(id, result))
        }

        "tasks/get" => {
            // Not supported — gateway operates in stateless mode.
            Json(JsonRpcResponse::err(
                id,
                -32601,
                "tasks/get not supported (stateless mode)",
            ))
        }

        "tasks/cancel" => Json(JsonRpcResponse::err(
            id,
            -32601,
            "tasks/cancel not supported",
        )),

        other => Json(JsonRpcResponse::err(
            id,
            -32601,
            format!("method not found: {other}"),
        )),
    }
}
