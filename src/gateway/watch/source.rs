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
    pub async fn run(self, tx: mpsc::Sender<EventRecord>, stop: oneshot::Receiver<()>) {
        match self {
            SourceImpl::File(s) => run_file(s, tx, stop).await,
            SourceImpl::Shell(s) => run_shell(s, tx, stop).await,
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

async fn run_file(
    src: FileSource,
    tx: mpsc::Sender<EventRecord>,
    mut stop: oneshot::Receiver<()>,
) {
    use tokio::fs::File;
    use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader, SeekFrom};

    let open = File::open(&src.path).await;
    let file = match open {
        Ok(mut f) => {
            let _ = f.seek(SeekFrom::End(0)).await;
            f
        }
        Err(e) => {
            let _ = tx
                .send(EventRecord::lifecycle(
                    "_error",
                    serde_json::json!({ "msg": format!("open failed: {e}") }),
                    now_ms(),
                ))
                .await;
            return;
        }
    };
    let mut current_inode = inode_of(&file).await;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));

    loop {
        tokio::select! {
            _ = &mut stop => return,
            _ = interval.tick() => {}
        }

        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let stripped = line.trim_end_matches(&['\r', '\n'][..]).to_owned();
                    if tx.send(EventRecord::from_line(stripped, now_ms())).await.is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }

        if let Ok(metadata) = tokio::fs::metadata(&src.path).await {
            let now_size = metadata.len();
            let pos = reader.get_mut().stream_position().await.unwrap_or(0);
            let new_inode = inode_from_metadata(&metadata);
            let inode_changed = current_inode.is_some()
                && new_inode.is_some()
                && current_inode != new_inode;
            if inode_changed || now_size < pos {
                if let Ok(f) = File::open(&src.path).await {
                    current_inode = inode_from_metadata(&metadata);
                    reader = BufReader::new(f);
                }
            }
        }
    }
}

async fn run_shell(
    src: ShellSource,
    tx: mpsc::Sender<EventRecord>,
    mut stop: oneshot::Receiver<()>,
) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::Command;

    let (program, arg) = if cfg!(target_os = "windows") {
        ("powershell", "-Command")
    } else {
        ("sh", "-c")
    };
    let mut cmd = Command::new(program);
    cmd.arg(arg)
        .arg(&src.cmd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx
                .send(EventRecord::lifecycle(
                    "_error",
                    serde_json::json!({ "msg": format!("spawn failed: {e}") }),
                    now_ms(),
                ))
                .await;
            return;
        }
    };

    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let (line_tx, mut line_rx) = mpsc::channel::<String>(64);

    let lt1 = line_tx.clone();
    let r1 = tokio::spawn(async move {
        let mut r = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = r.next_line().await {
            if lt1.send(line).await.is_err() {
                break;
            }
        }
    });
    let r2 = tokio::spawn(async move {
        let mut r = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = r.next_line().await {
            if line_tx.send(line).await.is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            _ = &mut stop => {
                let _ = child.start_kill();
                break;
            }
            line = line_rx.recv() => {
                match line {
                    Some(l) => {
                        if tx.send(EventRecord::from_line(l, now_ms())).await.is_err() {
                            let _ = child.start_kill();
                            break;
                        }
                    }
                    None => {
                        let exit = child.wait().await.ok().and_then(|s| s.code());
                        let _ = tx
                            .send(EventRecord::lifecycle(
                                "_disconnect",
                                serde_json::json!({ "reason": "process_exited", "code": exit }),
                                now_ms(),
                            ))
                            .await;
                        break;
                    }
                }
            }
        }
    }

    let _ = r1.await;
    let _ = r2.await;
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

async fn inode_of(file: &tokio::fs::File) -> Option<u64> {
    let metadata = file.metadata().await.ok()?;
    inode_from_metadata(&metadata)
}

#[cfg(unix)]
fn inode_from_metadata(m: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(m.ino())
}

#[cfg(not(unix))]
fn inode_from_metadata(_m: &std::fs::Metadata) -> Option<u64> {
    None
}
