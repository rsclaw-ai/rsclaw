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

use serde::Deserialize;

use crate::{
    a2a::{
        errors as a2a_errors,
        types::{
            A2aArtifact, A2aMessage, A2aPart, A2aTask, A2aTaskStatus, AgentCapabilities,
            AgentCard, AgentExtension, AgentInterface, AgentProvider, AgentSkill,
            JsonRpcRequest, JsonRpcResponse, PushNotificationConfig, SendMessageParams,
            TaskState,
        },
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

/// Top-level dispatcher. Streaming methods route to `crate::a2a::streaming`
/// and return an SSE stream; everything else returns a JSON-RPC response.
pub async fn a2a_dispatch(
    State(state): State<AppState>,
    Json(req): Json<JsonRpcRequest>,
) -> axum::response::Response {
    match req.method.as_str() {
        "SendStreamingMessage" | "SubscribeToTask" => {
            crate::a2a::streaming::handle_streaming_rpc(state, req)
                .await
                .into_response()
        }
        _ => a2a_rpc_handler_inner(state, req).await.into_response(),
    }
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
        "GetTask" => handle_get_task(state, id, req.params).await,
        "ListTasks" => handle_list_tasks(state, id, req.params).await,
        "CancelTask" => handle_cancel_task(state, id, req.params).await,
        "CreateTaskPushNotificationConfig" => {
            handle_create_push_config(state, id, req.params).await
        }
        "GetTaskPushNotificationConfig" => handle_get_push_config(state, id, req.params).await,
        "ListTaskPushNotificationConfigs" => {
            handle_list_push_configs(state, id, req.params).await
        }
        "DeleteTaskPushNotificationConfig" => {
            handle_delete_push_config(state, id, req.params).await
        }
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

    // Resume path: if the client is sending follow-up input to a paused task,
    // route it to the suspended runtime and return the task's current state.
    if let Some((_, suspended)) = state.suspended_tasks.remove(&task_id) {
        let _ = suspended.resume_tx.send(text);
        let task = state
            .task_store
            .get(&task_id)
            .ok()
            .flatten()
            .unwrap_or_else(|| A2aTask {
                id: task_id.clone(),
                context_id: Some(session_key.clone()),
                status: A2aTaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: Some(chrono::Utc::now().to_rfc3339()),
                },
                history: vec![],
                artifacts: vec![],
                metadata: None,
            });
        return Json(JsonRpcResponse::ok(
            id,
            serde_json::to_value(task).unwrap_or(Value::Null),
        ));
    }

    // Persist the initial task (Submitted).
    let initial_history = A2aMessage {
        message_id: params.message.message_id.clone(),
        role: params.message.role.clone(),
        parts: params.message.parts.clone(),
        context_id: Some(session_key.clone()),
        task_id: Some(task_id.clone()),
        metadata: params.message.metadata.clone(),
    };
    let initial_task = A2aTask {
        id: task_id.clone(),
        context_id: Some(session_key.clone()),
        status: A2aTaskStatus {
            state: TaskState::Submitted,
            message: None,
            timestamp: Some(chrono::Utc::now().to_rfc3339()),
        },
        history: vec![initial_history],
        artifacts: vec![],
        metadata: None,
    };
    if let Err(e) = state.task_store.put(&initial_task) {
        info!(err = %e, "failed to persist initial task");
    }

    // Register a cancellation token so CancelTask can stop the in-flight turn.
    let cancel_token = tokio_util::sync::CancellationToken::new();
    state
        .task_cancels
        .insert(task_id.clone(), cancel_token.clone());

    // Push notification fan-out — same as the streaming path. Without this,
    // synchronous SendMessage tasks emit events to the bus but no push
    // webhooks ever fire.
    state.push_dispatcher.clone().watch(task_id.clone());

    // Publish Submitted → Working status events on the bus so any SSE
    // subscriber (or push webhook listener) can observe progress.
    state.task_event_bus.publish(crate::a2a::event::AgentEvent::Status {
        task_id: task_id.clone(),
        context_id: session_key.clone(),
        state: TaskState::Submitted,
        message: None,
        final_: false,
    });
    state.task_event_bus.publish(crate::a2a::event::AgentEvent::Status {
        task_id: task_id.clone(),
        context_id: session_key.clone(),
        state: TaskState::Working,
        message: None,
        final_: false,
    });

    // Wire the INPUT_REQUIRED resume channel. If the runtime ever requests
    // additional input mid-turn, an entry lands in `state.suspended_tasks`;
    // the client's next SendMessage with the same taskId hits the
    // resume-path at the top of this handler.
    let (ireq_tx, mut ireq_rx) =
        tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<String>>(4);
    {
        let suspended = state.suspended_tasks.clone();
        let sus_task_id = task_id.clone();
        let sus_ctx = session_key.clone();
        tokio::spawn(async move {
            while let Some(resume_tx) = ireq_rx.recv().await {
                suspended.insert(
                    sus_task_id.clone(),
                    crate::a2a::event::SuspendedTask {
                        task_id: sus_task_id.clone(),
                        context_id: sus_ctx.clone(),
                        resume_tx,
                    },
                );
            }
        });
    }

    let (reply_tx, reply_rx) = oneshot::channel::<AgentReply>();
    let msg = AgentMessage {
        session_key: session_key.clone(),
        text,
        channel: "a2a".to_owned(),
        peer_id: "a2a-client".to_owned(),
        chat_id: String::new(),
        reply_tx,
        event_tx: None,
        cancel_token: Some(cancel_token),
        input_request_tx: Some(ireq_tx),
        extra_tools: vec![],
        images: vec![],
        files: vec![],
        account: None,
    };

    if handle.tx.send(msg).await.is_err() {
        finalize_failed_task(&state, &task_id, &session_key);
        return Json(JsonRpcResponse::err(id, -32603, "agent inbox closed"));
    }

    let timeout_secs = state.config.agents.defaults.timeout_seconds.unwrap_or(600) as u64;

    let reply =
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), reply_rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => {
                finalize_failed_task(&state, &task_id, &session_key);
                return Json(JsonRpcResponse::err(id, -32603, "reply channel dropped"));
            }
            Err(_) => {
                finalize_failed_task(&state, &task_id, &session_key);
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

    let _ = state.task_store.append_artifact(&task_id, artifact.clone());
    let _ = state.task_store.set_status(&task_id, TaskState::Completed);
    state.task_cancels.remove(&task_id);

    // Publish artifact + final Completed status, then close the bus.
    state.task_event_bus.publish(crate::a2a::event::AgentEvent::Artifact {
        task_id: task_id.clone(),
        context_id: session_key.clone(),
        artifact_id: artifact.artifact_id.clone(),
        parts: artifact.parts.clone(),
        append: false,
        last_chunk: true,
    });
    state.task_event_bus.publish(crate::a2a::event::AgentEvent::Status {
        task_id: task_id.clone(),
        context_id: session_key.clone(),
        state: TaskState::Completed,
        message: None,
        final_: true,
    });
    state.task_event_bus.close(&task_id);

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

// ---------------------------------------------------------------------------
// GetTask / ListTasks / CancelTask
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetTaskParams {
    id: String,
    #[serde(default)]
    history_length: Option<usize>,
}

async fn handle_get_task(state: AppState, id: Value, params: Value) -> Json<JsonRpcResponse> {
    let params: GetTaskParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            return Json(JsonRpcResponse::err_struct(
                id,
                a2a_errors::invalid_argument(format!("invalid params: {e}"), "params"),
            ));
        }
    };
    match state.task_store.get(&params.id) {
        Ok(Some(mut task)) => {
            if let Some(n) = params.history_length
                && task.history.len() > n
            {
                let skip = task.history.len() - n;
                task.history = task.history.split_off(skip);
            }
            Json(JsonRpcResponse::ok(
                id,
                serde_json::to_value(task).unwrap_or(Value::Null),
            ))
        }
        Ok(None) => Json(JsonRpcResponse::err_struct(
            id,
            a2a_errors::not_found(format!("tasks/{}", params.id)),
        )),
        Err(e) => Json(JsonRpcResponse::err_struct(
            id,
            a2a_errors::internal(format!("store error: {e}")),
        )),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListTasksParams {
    #[serde(default)]
    page_size: Option<usize>,
    #[serde(default)]
    page_token: Option<String>,
}

async fn handle_list_tasks(state: AppState, id: Value, params: Value) -> Json<JsonRpcResponse> {
    let params: ListTasksParams = serde_json::from_value(params).unwrap_or(ListTasksParams {
        page_size: None,
        page_token: None,
    });
    let offset: usize = params
        .page_token
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let limit = params.page_size.unwrap_or(50).min(500);
    match state.task_store.list(offset, limit) {
        Ok(tasks) => {
            let next_token = if tasks.len() == limit {
                Some((offset + limit).to_string())
            } else {
                None
            };
            Json(JsonRpcResponse::ok(
                id,
                json!({ "tasks": tasks, "nextPageToken": next_token }),
            ))
        }
        Err(e) => Json(JsonRpcResponse::err_struct(
            id,
            a2a_errors::internal(format!("store error: {e}")),
        )),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CancelTaskParams {
    id: String,
}

async fn handle_cancel_task(
    state: AppState,
    id: Value,
    params: Value,
) -> Json<JsonRpcResponse> {
    let params: CancelTaskParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            return Json(JsonRpcResponse::err_struct(
                id,
                a2a_errors::invalid_argument(format!("invalid params: {e}"), "params"),
            ));
        }
    };
    match state.task_cancels.remove(&params.id) {
        Some((_, token)) => {
            token.cancel();
            let _ = state.task_store.set_status(&params.id, TaskState::Canceled);
            // Publish a terminal Canceled status so any SSE subscriber sees it.
            let ctx = state
                .task_store
                .get(&params.id)
                .ok()
                .flatten()
                .and_then(|t| t.context_id)
                .unwrap_or_default();
            state.task_event_bus.publish(crate::a2a::event::AgentEvent::Status {
                task_id: params.id.clone(),
                context_id: ctx,
                state: TaskState::Canceled,
                message: None,
                final_: true,
            });
            state.task_event_bus.close(&params.id);
            match state.task_store.get(&params.id) {
                Ok(Some(task)) => Json(JsonRpcResponse::ok(
                    id,
                    serde_json::to_value(task).unwrap_or(Value::Null),
                )),
                _ => Json(JsonRpcResponse::ok(
                    id,
                    json!({ "id": params.id, "status": { "state": "TASK_STATE_CANCELED" } }),
                )),
            }
        }
        None => match state.task_store.get(&params.id) {
            Ok(Some(t)) if t.status.state.is_terminal() => Json(JsonRpcResponse::err_struct(
                id,
                a2a_errors::precondition_failed(format!(
                    "task already terminal: {:?}",
                    t.status.state
                )),
            )),
            Ok(Some(_)) => Json(JsonRpcResponse::err_struct(
                id,
                a2a_errors::precondition_failed(
                    "task running but no cancel token (gateway restart?)".to_owned(),
                ),
            )),
            Ok(None) => Json(JsonRpcResponse::err_struct(
                id,
                a2a_errors::not_found(format!("tasks/{}", params.id)),
            )),
            Err(e) => Json(JsonRpcResponse::err_struct(
                id,
                a2a_errors::internal(format!("store error: {e}")),
            )),
        },
    }
}

// ---------------------------------------------------------------------------
// Push notification CRUD
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreatePushConfigParams {
    task_id: String,
    push_notification_config: PushNotificationConfig,
}

async fn handle_create_push_config(
    state: AppState,
    id: Value,
    params: Value,
) -> Json<JsonRpcResponse> {
    let mut params: CreatePushConfigParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            return Json(JsonRpcResponse::err_struct(
                id,
                a2a_errors::invalid_argument(format!("invalid params: {e}"), "params"),
            ));
        }
    };
    params.push_notification_config.task_id = params.task_id.clone();
    if params.push_notification_config.id.is_empty() {
        params.push_notification_config.id = Uuid::new_v4().to_string();
    }
    match state.task_store.put_push_config(&params.push_notification_config) {
        Ok(_) => Json(JsonRpcResponse::ok(
            id,
            serde_json::to_value(&params.push_notification_config).unwrap_or(Value::Null),
        )),
        Err(e) => Json(JsonRpcResponse::err_struct(
            id,
            a2a_errors::internal(format!("store error: {e}")),
        )),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetPushConfigParams {
    task_id: String,
    push_notification_config_id: String,
}

async fn handle_get_push_config(
    state: AppState,
    id: Value,
    params: Value,
) -> Json<JsonRpcResponse> {
    let params: GetPushConfigParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            return Json(JsonRpcResponse::err_struct(
                id,
                a2a_errors::invalid_argument(format!("invalid params: {e}"), "params"),
            ));
        }
    };
    match state
        .task_store
        .get_push_config(&params.task_id, &params.push_notification_config_id)
    {
        Ok(Some(c)) => Json(JsonRpcResponse::ok(
            id,
            serde_json::to_value(c).unwrap_or(Value::Null),
        )),
        Ok(None) => Json(JsonRpcResponse::err_struct(
            id,
            a2a_errors::not_found(format!(
                "tasks/{}/pushNotificationConfigs/{}",
                params.task_id, params.push_notification_config_id
            )),
        )),
        Err(e) => Json(JsonRpcResponse::err_struct(
            id,
            a2a_errors::internal(format!("store error: {e}")),
        )),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListPushConfigsParams {
    task_id: String,
}

async fn handle_list_push_configs(
    state: AppState,
    id: Value,
    params: Value,
) -> Json<JsonRpcResponse> {
    let params: ListPushConfigsParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            return Json(JsonRpcResponse::err_struct(
                id,
                a2a_errors::invalid_argument(format!("invalid params: {e}"), "params"),
            ));
        }
    };
    match state.task_store.list_push_configs(&params.task_id) {
        Ok(configs) => Json(JsonRpcResponse::ok(id, json!({ "configs": configs }))),
        Err(e) => Json(JsonRpcResponse::err_struct(
            id,
            a2a_errors::internal(format!("store error: {e}")),
        )),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeletePushConfigParams {
    task_id: String,
    push_notification_config_id: String,
}

async fn handle_delete_push_config(
    state: AppState,
    id: Value,
    params: Value,
) -> Json<JsonRpcResponse> {
    let params: DeletePushConfigParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            return Json(JsonRpcResponse::err_struct(
                id,
                a2a_errors::invalid_argument(format!("invalid params: {e}"), "params"),
            ));
        }
    };
    match state
        .task_store
        .delete_push_config(&params.task_id, &params.push_notification_config_id)
    {
        Ok(true) => Json(JsonRpcResponse::ok(id, json!({ "deleted": true }))),
        Ok(false) => Json(JsonRpcResponse::err_struct(
            id,
            a2a_errors::not_found(format!(
                "tasks/{}/pushNotificationConfigs/{}",
                params.task_id, params.push_notification_config_id
            )),
        )),
        Err(e) => Json(JsonRpcResponse::err_struct(
            id,
            a2a_errors::internal(format!("store error: {e}")),
        )),
    }
}

/// Mark a task as Failed and clean up its in-memory state.
/// Used by every early-error return in `handle_send_message` so a task
/// that never reached `Completed` doesn't linger as `Working` in the store
/// (GetTask/ListTasks would surface it as stuck) and so the cancel token
/// + broadcast channel don't leak.
fn finalize_failed_task(state: &AppState, task_id: &str, context_id: &str) {
    let _ = state.task_store.set_status(task_id, TaskState::Failed);
    state.task_cancels.remove(task_id);
    state
        .task_event_bus
        .publish(crate::a2a::event::AgentEvent::Status {
            task_id: task_id.to_owned(),
            context_id: context_id.to_owned(),
            state: TaskState::Failed,
            message: None,
            final_: true,
        });
    state.task_event_bus.close(task_id);
}
