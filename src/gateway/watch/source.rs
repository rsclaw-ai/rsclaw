//! EventSource impls (file / sse / shell).
//!
//! Three sources share the same `EventRecord` output shape and a single
//! `SourceImpl::run(tx, stop)` entry point. Each source's body is
//! implemented in its own submodule (added in Tasks 8/9/10–14).

use serde::Serialize;
use std::path::PathBuf;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::gateway::watch::parser::SourceKind;

/// Unified event record emitted by every EventSource.
#[derive(Debug, Clone, Serialize)]
pub struct EventRecord {
    /// Event type. `"line"` for shell / file, the SSE `event:` field for SSE
    /// (default `"message"`), and `_disconnect` / `_timeout` / `_error` for
    /// lifecycle signals.
    pub event: String,
    pub data: serde_json::Value,
    /// Raw text form (used by `--grep`). Always present for shell/file; None
    /// for SSE when the only representation is the parsed JSON.
    pub raw: Option<String>,
    /// SSE `id:` field. Used for Last-Event-ID resume on reconnect.
    pub event_id: Option<String>,
    pub ts_ms: u64,
}

impl EventRecord {
    pub fn from_line(line: String, now_ms: u64) -> Self {
        Self {
            event: "line".into(),
            data: serde_json::Value::String(line.clone()),
            raw: Some(line),
            event_id: None,
            ts_ms: now_ms,
        }
    }

    pub fn lifecycle(kind: &str, reason: serde_json::Value, now_ms: u64) -> Self {
        Self {
            event: kind.into(),
            data: reason,
            raw: None,
            event_id: None,
            ts_ms: now_ms,
        }
    }
}

#[derive(Debug, Error)]
pub enum WatchStartError {
    #[error("limit reached ({current}/{max})")]
    LimitReached { current: usize, max: usize },
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("invalid regex: {0}")]
    InvalidRegex(String),
    #[error("invalid jq expression: {0}")]
    InvalidJq(String),
    #[error("unresolved env var: {0}")]
    UnresolvedEnv(String),
    #[error("shell exited immediately (code={0:?})")]
    SourceFailedImmediately(Option<i32>),
    #[error("internal error: {0}")]
    Internal(String),
}

/// Concrete source implementations are sum-typed instead of `Box<dyn Trait>`
/// to avoid `async-trait` and keep the call-site `select!` on `Future` types
/// concrete. Each variant implements its own `run` inline (or in a helper
/// submodule for the larger ones).
pub enum SourceImpl {
    File(FileSource),
    Shell(ShellSource),
    Sse(SseSource),
}

pub struct FileSource {
    pub path: PathBuf,
}

pub struct ShellSource {
    pub cmd: String,
}

pub struct SseSource {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

impl SourceImpl {
    /// Drive the source. Send each emitted event to `tx`; exit on either
    /// `stop` signal or natural EOF / fatal error.
    ///
    /// **Source-specific implementations live in `Task 8/9/10–14`.**
    pub async fn run(self, _tx: mpsc::Sender<EventRecord>, _stop: oneshot::Receiver<()>) {
        // Tasks 8/9/10–14 fill in match arms.
        match self {
            SourceImpl::File(_) => unimplemented!("Task 8: FileSource"),
            SourceImpl::Shell(_) => unimplemented!("Task 9: ShellSource"),
            SourceImpl::Sse(_) => unimplemented!("Task 10–14: SseSource"),
        }
    }

    pub fn kind(&self) -> SourceKind {
        match self {
            SourceImpl::File(_) => SourceKind::File,
            SourceImpl::Shell(_) => SourceKind::Shell,
            SourceImpl::Sse(_) => SourceKind::Sse,
        }
    }
}
