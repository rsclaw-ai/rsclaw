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
        types::{
            A2aArtifact, A2aMessage, A2aPart, A2aTask, A2aTaskStatus, JsonRpcRequest,
            SendMessageParams, TaskState,
        },
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
    let (task_id, rx) = match req.method.as_str() {
        "SendStreamingMessage" => spawn_streaming_task(state.clone(), req.params).await,
        "SubscribeToTask" => {
            let tid = req
                .params
                .get("id")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .unwrap_or_default();
            // Empty id or unknown task: emit a synthetic Failed and close so
            // the SSE doesn't hang waiting on a channel that will never
            // receive a publish. Existing tasks still in flight have a live
            // sender, so subscribing returns events as expected.
            if tid.is_empty() || state.task_store.get(&tid).ok().flatten().is_none() {
                let rx = state.task_event_bus.subscribe(&tid);
                state.task_event_bus.publish(AgentEvent::Status {
                    task_id: tid.clone(),
                    context_id: String::new(),
                    state: TaskState::Failed,
                    message: Some(crate::a2a::event::text_message(if tid.is_empty() {
                        "SubscribeToTask: missing task id"
                    } else {
                        "SubscribeToTask: task not found"
                    })),
                    final_: true,
                });
                state.task_event_bus.close(&tid);
                (tid, rx)
            } else {
                let rx = state.task_event_bus.subscribe(&tid);
                (tid, rx)
            }
        }
        other => {
            // Unknown method on the SSE entry — emit Failed + close so the
            // client doesn't hang on an empty broadcast channel.
            let tid = Uuid::new_v4().to_string();
            let rx = state.task_event_bus.subscribe(&tid);
            state.task_event_bus.publish(AgentEvent::Status {
                task_id: tid.clone(),
                context_id: String::new(),
                state: TaskState::Failed,
                message: Some(crate::a2a::event::text_message(&format!(
                    "unsupported streaming method: {other}"
                ))),
                final_: true,
            });
            state.task_event_bus.close(&tid);
            (tid, rx)
        }
    };

    drop(task_id);
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

/// Spawn an agent task and return `(task_id, subscriber)`. The subscriber is
/// taken BEFORE any events are published to the bus so the SSE consumer
/// observes the full Submitted → Working → Completed sequence.
async fn spawn_streaming_task(
    state: AppState,
    params: Value,
) -> (String, tokio::sync::broadcast::Receiver<AgentEvent>) {
    let params: SendMessageParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(e) => {
            warn!(err = %e, "SendStreamingMessage: invalid params");
            // Without a terminal Failed + bus close the SSE stream stays
            // open with no events and the client hangs until its read
            // timeout. Emit a synthetic Failed carrying the parse error
            // and close the bus so BroadcastStream sees Closed and ends.
            let tid = Uuid::new_v4().to_string();
            let rx = state.task_event_bus.subscribe(&tid);
            state.task_event_bus.publish(AgentEvent::Status {
                task_id: tid.clone(),
                context_id: String::new(),
                state: TaskState::Failed,
                message: Some(crate::a2a::event::text_message(&format!(
                    "invalid params: {e}"
                ))),
                final_: true,
            });
            state.task_event_bus.close(&tid);
            return (tid, rx);
        }
    };

    let task_id = params
        .message
        .task_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    // CRITICAL: subscribe BEFORE any publish so the SSE consumer sees
    // Submitted/Working frames. broadcast channels don't replay history.
    let early_rx = state.task_event_bus.subscribe(&task_id);

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
        // Publish Failed AND close the bus so the SSE stream actually
        // terminates — without the close the subscriber sees Failed but
        // BroadcastStream stays subscribed forever waiting for the next
        // event that will never come.
        state.task_event_bus.publish(AgentEvent::Status {
            task_id: task_id.clone(),
            context_id: session_key.clone(),
            state: TaskState::Failed,
            message: Some(crate::a2a::event::text_message(
                "no agent available for this task",
            )),
            final_: true,
        });
        state.task_event_bus.close(&task_id);
        return (task_id, early_rx);
    };

    // Reply oneshot — we await this in a spawned task so we can publish
    // Completed/Failed and the final Artifact when the agent's turn finishes.
    let (reply_tx, reply_rx) = oneshot::channel::<AgentReply>();

    // mpsc channel from runtime → bus bridge. Does NOT close the bus on
    // drop — the bus stays open until the reply-watcher (below) publishes
    // the terminal status event. Otherwise the bus would close as soon as
    // the agent destructured AgentMessage (dropping event_tx) and any later
    // Completed publish would land on a fresh channel with no subscribers.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    let bus_for_bridge = state.task_event_bus.clone();
    tokio::spawn(async move {
        while let Some(ev) = event_rx.recv().await {
            bus_for_bridge.publish(ev);
        }
    });

    let cancel_token = tokio_util::sync::CancellationToken::new();
    state
        .task_cancels
        .insert(task_id.clone(), cancel_token.clone());

    // Persist initial task state (Submitted).
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
        warn!(err = %e, "failed to persist initial streaming task");
    }

    // Mirror events into the persistent store as they arrive.
    let persist_store = state.task_store.clone();
    let persist_task_id = task_id.clone();
    let mut persist_rx = state.task_event_bus.subscribe(&task_id);
    tokio::spawn(async move {
        while let Ok(ev) = persist_rx.recv().await {
            match ev {
                AgentEvent::Artifact {
                    artifact_id,
                    parts,
                    ..
                } => {
                    let _ = persist_store.append_artifact(
                        &persist_task_id,
                        A2aArtifact {
                            artifact_id,
                            parts,
                            name: None,
                            description: None,
                            metadata: None,
                        },
                    );
                }
                AgentEvent::Status { state, final_, .. } => {
                    let _ = persist_store.set_status(&persist_task_id, state);
                    if final_ {
                        break;
                    }
                }
                AgentEvent::InputRequired { .. } => {
                    let _ = persist_store
                        .set_status(&persist_task_id, TaskState::InputRequired);
                }
                AgentEvent::AuthRequired { .. } => {
                    let _ = persist_store
                        .set_status(&persist_task_id, TaskState::AuthRequired);
                }
            }
        }
    });

    // Push notification fan-out.
    state.push_dispatcher.clone().watch(task_id.clone());

    // Publish Submitted → Working status events so SSE subscribers see progress.
    state.task_event_bus.publish(AgentEvent::Status {
        task_id: task_id.clone(),
        context_id: session_key.clone(),
        state: TaskState::Submitted,
        message: None,
        final_: false,
    });
    state.task_event_bus.publish(AgentEvent::Status {
        task_id: task_id.clone(),
        context_id: session_key.clone(),
        state: TaskState::Working,
        message: None,
        final_: false,
    });

    // Spawn a watcher that, when the agent's reply arrives, publishes the
    // final artifact + Completed status (or Failed if the channel dropped).
    let bus_for_reply = state.task_event_bus.clone();
    let task_id_for_reply = task_id.clone();
    let ctx_id_for_reply = session_key.clone();
    let cancels_for_reply = state.task_cancels.clone();
    tokio::spawn(async move {
        match reply_rx.await {
            Ok(reply) => match reply.outcome {
                crate::agent::registry::ReplyOutcome::Ok => {
                    let artifact_id = uuid::Uuid::new_v4().to_string();
                    bus_for_reply.publish(AgentEvent::Artifact {
                        task_id: task_id_for_reply.clone(),
                        context_id: ctx_id_for_reply.clone(),
                        artifact_id,
                        parts: vec![A2aPart::Text { text: reply.text }],
                        append: false,
                        last_chunk: true,
                    });
                    bus_for_reply.publish(AgentEvent::Status {
                        task_id: task_id_for_reply.clone(),
                        context_id: ctx_id_for_reply,
                        state: TaskState::Completed,
                        message: None,
                        final_: true,
                    });
                }
                crate::agent::registry::ReplyOutcome::Error => {
                    // Surface the error text in the terminal Failed message so
                    // SSE/push subscribers see the cause; no Artifact (Artifact
                    // implies usable output).
                    bus_for_reply.publish(AgentEvent::Status {
                        task_id: task_id_for_reply.clone(),
                        context_id: ctx_id_for_reply,
                        state: TaskState::Failed,
                        message: Some(crate::a2a::event::text_message(&reply.text)),
                        final_: true,
                    });
                }
                crate::agent::registry::ReplyOutcome::Canceled => {
                    // CancelTask dispatcher already published the terminal
                    // Canceled event and closed the bus. Don't republish.
                }
            },
            Err(_) => {
                bus_for_reply.publish(AgentEvent::Status {
                    task_id: task_id_for_reply.clone(),
                    context_id: ctx_id_for_reply,
                    state: TaskState::Failed,
                    message: None,
                    final_: true,
                });
            }
        }
        cancels_for_reply.remove(&task_id_for_reply);
        // Now safe to drop the broadcast channel — terminal event delivered.
        bus_for_reply.close(&task_id_for_reply);
    });

    // Wire the INPUT_REQUIRED resume channel.
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

    let msg = AgentMessage {
        session_key: session_key.clone(),
        text,
        channel: "a2a".to_owned(),
        peer_id: "a2a-client".to_owned(),
        chat_id: String::new(),
        reply_tx,
        task_id: Some(task_id.clone()),
        context_id: Some(session_key),
        event_tx: Some(event_tx),
        cancel_token: Some(cancel_token),
        input_request_tx: Some(ireq_tx),
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
    (task_id, early_rx)
}
