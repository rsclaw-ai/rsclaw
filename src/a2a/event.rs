//! Streaming agent events — bridge from runtime to SSE / push consumers.

use std::sync::Arc;

use dashmap::DashMap;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use super::types::{A2aMessage, A2aPart, TaskState};

/// An event emitted by the agent runtime as a task makes progress.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    Status {
        task_id: String,
        context_id: String,
        state: TaskState,
        message: Option<A2aMessage>,
        /// `true` only on the terminal status event.
        final_: bool,
    },
    Artifact {
        task_id: String,
        context_id: String,
        artifact_id: String,
        parts: Vec<A2aPart>,
        /// If `true`, append these parts to a prior artifact with the same id.
        append: bool,
        last_chunk: bool,
    },
    /// Agent is requesting additional input (TASK_STATE_INPUT_REQUIRED).
    InputRequired {
        task_id: String,
        context_id: String,
        prompt: A2aMessage,
    },
}

impl AgentEvent {
    pub fn task_id(&self) -> &str {
        match self {
            Self::Status { task_id, .. }
            | Self::Artifact { task_id, .. }
            | Self::InputRequired { task_id, .. } => task_id,
        }
    }

    /// Serialize to the v1.0 wire event JSON payload that goes inside the
    /// JSON-RPC `result` field of a streaming response.
    pub fn to_wire_event(&self) -> Value {
        match self {
            Self::Status {
                task_id,
                context_id,
                state,
                message,
                final_,
            } => {
                let mut status = json!({ "state": state });
                if let Some(m) = message {
                    status["message"] = serde_json::to_value(m).unwrap_or(Value::Null);
                }
                json!({
                    "kind": "status-update",
                    "taskId": task_id,
                    "contextId": context_id,
                    "status": status,
                    "final": final_,
                })
            }
            Self::Artifact {
                task_id,
                context_id,
                artifact_id,
                parts,
                append,
                last_chunk,
            } => json!({
                "kind": "artifact-update",
                "taskId": task_id,
                "contextId": context_id,
                "artifact": {
                    "artifactId": artifact_id,
                    "parts": parts,
                },
                "append": append,
                "lastChunk": last_chunk,
            }),
            Self::InputRequired {
                task_id,
                context_id,
                prompt,
            } => json!({
                "kind": "status-update",
                "taskId": task_id,
                "contextId": context_id,
                "status": {
                    "state": TaskState::InputRequired,
                    "message": prompt,
                },
                "final": false,
            }),
        }
    }
}

/// Fan-out bus that broadcasts agent events per task_id.
/// Used by SSE handlers (SubscribeToTask) and the push notification dispatcher.
#[derive(Clone, Default)]
pub struct TaskEventBus {
    inner: Arc<DashMap<String, broadcast::Sender<AgentEvent>>>,
}

impl TaskEventBus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn channel(&self, task_id: &str) -> broadcast::Sender<AgentEvent> {
        self.inner
            .entry(task_id.to_owned())
            .or_insert_with(|| broadcast::channel(128).0)
            .clone()
    }

    pub fn subscribe(&self, task_id: &str) -> broadcast::Receiver<AgentEvent> {
        self.channel(task_id).subscribe()
    }

    /// Publish an event. Returns subscriber count (0 if no subscribers — fine).
    pub fn publish(&self, event: AgentEvent) -> usize {
        let tx = self.channel(event.task_id());
        tx.send(event).unwrap_or(0)
    }

    /// Drop the channel for a terminal task (call after the final status event).
    pub fn close(&self, task_id: &str) {
        self.inner.remove(task_id);
    }
}

/// State for an A2A task currently paused on TASK_STATE_INPUT_REQUIRED.
/// Registered when the runtime emits `AgentEvent::InputRequired`; consumed when
/// the client sends another `SendMessage` with the matching `taskId`.
#[derive(Debug)]
pub struct SuspendedTask {
    pub task_id: String,
    pub context_id: String,
    /// Oneshot sender for the resume input. Runtime holds the receiver.
    pub resume_tx: tokio::sync::oneshot::Sender<String>,
}
