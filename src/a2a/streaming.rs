//! SSE handlers for A2A v1.0 streaming methods.
//!
//! Two methods land here:
//!   - SendStreamingMessage : spawn a new agent task, stream its events back
//!   - SubscribeToTask      : tap into an existing task's event broadcast
//!
//! Each event is wrapped in a JSON-RPC frame `{"jsonrpc","id","result":<wire>}`
//! and emitted as one SSE `data:` line.

use std::convert::Infallible;

use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::{Stream, StreamExt};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    a2a::{
        event::AgentEvent,
        types::{A2aPart, JsonRpcRequest, SendMessageParams},
    },
    agent::{AgentMessage, AgentReply},
    server::AppState,
};

/// Entry point. Called by the gateway dispatcher when the JSON-RPC method is
/// `SendStreamingMessage` or `SubscribeToTask`.
pub async fn handle_streaming_rpc(
    state: AppState,
    req: JsonRpcRequest,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let req_id = req.id.clone();
    let task_id = match req.method.as_str() {
        "SendStreamingMessage" => spawn_streaming_task(state.clone(), req.params).await,
        "SubscribeToTask" => req
            .params
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or_default(),
        _ => String::new(),
    };

    let rx = state.task_event_bus.subscribe(&task_id);
    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        let req_id = req_id.clone();
        async move {
            match result {
                Ok(ev) => {
                    let payload = json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "result": ev.to_wire_event(),
                    });
                    Some(Ok::<_, Infallible>(
                        Event::default().json_data(payload).unwrap_or_default(),
                    ))
                }
                Err(BroadcastStreamRecvError::Lagged(n)) => {
                    warn!(lagged = n, "SSE consumer lagged");
                    None
                }
            }
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::new())
}

/// Spawn an agent task and return the assigned task_id. Events flow back via
/// the per-task broadcast bus.
async fn spawn_streaming_task(state: AppState, params: Value) -> String {
    let params: SendMessageParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            warn!(err = %e, "SendStreamingMessage: invalid params");
            return Uuid::new_v4().to_string();
        }
    };

    let task_id = params
        .message
        .task_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let session_key = params
        .message
        .context_id
        .clone()
        .unwrap_or_else(|| format!("a2a:{}", Uuid::new_v4()));

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

    let agent_id = params
        .metadata
        .as_ref()
        .and_then(|m| m.get("agentId").and_then(|v| v.as_str()).map(str::to_owned));

    let handle = match agent_id {
        Some(aid) => state.agents.get(&aid),
        None => state.agents.default_agent(),
    };
    let Ok(handle) = handle else {
        warn!("SendStreamingMessage: no agent available");
        return task_id;
    };

    // Reply oneshot — kept alive but the streaming path doesn't await it
    // (status events on the bus carry final state).
    let (reply_tx, _reply_rx) = oneshot::channel::<AgentReply>();

    // mpsc channel from runtime → bus bridge.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    let bus = state.task_event_bus.clone();
    let bus_task_id = task_id.clone();
    tokio::spawn(async move {
        while let Some(ev) = event_rx.recv().await {
            bus.publish(ev);
        }
        bus.close(&bus_task_id);
    });

    let cancel_token = tokio_util::sync::CancellationToken::new();
    state
        .task_cancels
        .insert(task_id.clone(), cancel_token.clone());

    let msg = AgentMessage {
        session_key,
        text,
        channel: "a2a".to_owned(),
        peer_id: "a2a-client".to_owned(),
        chat_id: String::new(),
        reply_tx,
        event_tx: Some(event_tx),
        cancel_token: Some(cancel_token),
        input_request_tx: None,
        extra_tools: vec![],
        images: vec![],
        files: vec![],
        account: None,
    };

    if let Err(e) = handle.tx.send(msg).await {
        warn!(err = ?e, "agent inbox closed");
    } else {
        info!(task_id = %task_id, "A2A SendStreamingMessage spawned");
    }
    task_id
}
