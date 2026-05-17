//! A2A v1.0 server handlers.
//!
//! Implements:
//!   GET  /.well-known/agent.json  — Agent Card discovery
//!   POST /api/v1/a2a              — JSON-RPC 2.0 method dispatch
//!
//! Streaming methods (SendStreamingMessage / SubscribeToTask) are served by
//! `crate::a2a::streaming` instead — the dispatcher in `src/server/mod.rs`
//! routes by method name.

use axum::{Json, extract::State, response::IntoResponse};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tracing::info;
use uuid::Uuid;

use crate::{
    a2a::types::{
        A2aArtifact, A2aMessage, A2aPart, AgentCapabilities, AgentCard, AgentExtension,
        AgentInterface, AgentProvider, AgentSkill, JsonRpcRequest, JsonRpcResponse,
        SendMessageParams,
    },
    agent::{AgentMessage, AgentReply},
    config::schema::BindMode,
    server::AppState,
};

pub const PROTOCOL_VERSION: &str = "1.0";

pub const V1_METHODS: &[&str] = &[
    "SendMessage",
    "SendStreamingMessage",
    "GetTask",
    "ListTasks",
    "CancelTask",
    "SubscribeToTask",
    "CreateTaskPushNotificationConfig",
    "GetTaskPushNotificationConfig",
    "ListTaskPushNotificationConfigs",
    "DeleteTaskPushNotificationConfig",
    "GetExtendedAgentCard",
];

pub fn known_method(name: &str) -> bool {
    V1_METHODS.contains(&name)
}

// ---------------------------------------------------------------------------
// Agent Card — GET /.well-known/agent.json
// ---------------------------------------------------------------------------

pub async fn agent_card_handler(State(state): State<AppState>) -> impl IntoResponse {
    Json(build_agent_card(&state, false))
}

pub fn build_agent_card(state: &AppState, _extended: bool) -> AgentCard {
    let host = match state.config.gateway.bind {
        BindMode::Loopback => "127.0.0.1",
        BindMode::All | BindMode::Lan | BindMode::Auto | BindMode::Custom | BindMode::Tailnet => {
            "0.0.0.0"
        }
    };
    let port = state.config.gateway.port;
    let base_url = format!("http://{host}:{port}/api/v1/a2a");

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

    AgentCard {
        protocol_version: PROTOCOL_VERSION.to_owned(),
        name: "rsclaw".to_owned(),
        description: Some("AI Agent Engine Compatible with OpenClaw".to_owned()),
        url: base_url.clone(),
        provider: Some(AgentProvider {
            organization: "rsclaw".to_owned(),
            url: Some("https://github.com/oopos/rsclaw".to_owned()),
        }),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        capabilities: AgentCapabilities {
            streaming: true,
            push_notifications: true,
            extended_agent_card: true,
        },
        security_schemes: Some(json!({
            "bearer": { "type": "http", "scheme": "bearer" },
            "apiKey": { "type": "apiKey", "in": "header", "name": "X-API-Key" }
        })),
        security: Some(vec![json!({ "bearer": [] }), json!({ "apiKey": [] })]),
        default_input_modes: vec![
            "text/plain".to_owned(),
            "application/octet-stream".to_owned(),
        ],
        default_output_modes: vec!["text/plain".to_owned()],
        skills,
        extensions: Vec::<AgentExtension>::new(),
        signatures: vec![],
        interfaces: vec![AgentInterface {
            url: base_url,
            transport: "JSONRPC".to_owned(),
        }],
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC dispatch — POST /api/v1/a2a (non-streaming methods)
// ---------------------------------------------------------------------------

pub async fn a2a_rpc_handler(
    State(state): State<AppState>,
    Json(req): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    a2a_rpc_handler_inner(state, req).await
}

pub async fn a2a_rpc_handler_inner(state: AppState, req: JsonRpcRequest) -> Json<JsonRpcResponse> {
    let id = req.id.clone();
    match req.method.as_str() {
        "SendMessage" => handle_send_message(state, id, req.params).await,
        "GetExtendedAgentCard" => Json(JsonRpcResponse::ok(
            id,
            serde_json::to_value(build_agent_card(&state, true)).unwrap_or(Value::Null),
        )),
        "SendStreamingMessage" | "SubscribeToTask" => Json(JsonRpcResponse::err(
            id,
            -32601,
            "use Accept: text/event-stream for streaming methods",
        )),
        "GetTask"
        | "ListTasks"
        | "CancelTask"
        | "CreateTaskPushNotificationConfig"
        | "GetTaskPushNotificationConfig"
        | "ListTaskPushNotificationConfigs"
        | "DeleteTaskPushNotificationConfig" => Json(JsonRpcResponse::err(
            id,
            -32601,
            format!("method not implemented yet: {}", req.method),
        )),
        other => Json(JsonRpcResponse::err(
            id,
            -32601,
            format!("method not found: {other}"),
        )),
    }
}

async fn handle_send_message(
    state: AppState,
    id: Value,
    params: Value,
) -> Json<JsonRpcResponse> {
    let params: SendMessageParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            return Json(JsonRpcResponse::err(
                id,
                -32602,
                format!("invalid params: {e}"),
            ));
        }
    };

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

    let agent_id = params
        .metadata
        .as_ref()
        .and_then(|m| m.get("agentId").and_then(|v| v.as_str()).map(str::to_owned));

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
                return Json(JsonRpcResponse::err(id, -32001, format!("no default agent: {e}")));
            }
        }
    };

    let session_key = params
        .message
        .context_id
        .clone()
        .unwrap_or_else(|| format!("a2a:{}", Uuid::new_v4()));

    let task_id = params
        .message
        .task_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());

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
        account: None,
    };

    if handle.tx.send(msg).await.is_err() {
        return Json(JsonRpcResponse::err(id, -32603, "agent inbox closed"));
    }

    let timeout_secs = state.config.agents.defaults.timeout_seconds.unwrap_or(600) as u64;

    let reply =
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), reply_rx).await {
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

    let history_msg = A2aMessage {
        message_id: params.message.message_id.clone(),
        role: params.message.role.clone(),
        parts: params.message.parts.clone(),
        context_id: Some(session_key.clone()),
        task_id: Some(task_id.clone()),
        metadata: params.message.metadata.clone(),
    };

    let artifact = A2aArtifact {
        artifact_id: Uuid::new_v4().to_string(),
        parts: vec![A2aPart::Text { text: reply.text }],
        name: None,
        description: None,
        metadata: None,
    };

    let result = json!({
        "id": task_id,
        "contextId": session_key,
        "status": { "state": "TASK_STATE_COMPLETED" },
        "artifacts": [artifact],
        "history": [history_msg],
    });

    info!(task_id, agent = %handle.id, "A2A SendMessage completed");
    Json(JsonRpcResponse::ok(id, result))
}
