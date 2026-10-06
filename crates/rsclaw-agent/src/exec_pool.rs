//! Async exec pool — tracks long-running commands started in the background
//! by the `exec` tool so they don't block the agent's main loop. Results are
//! stored per-session for collection on subsequent turns, or polled by
//! `task_id`.
//!
//! Also hosts [`run_capped`], the shared child-process runner used by the
//! exec / search tools: bounded stdout/stderr capture, own process group,
//! and whole-tree kill on timeout.

use std::{collections::HashMap, sync::Arc, time::Instant};

use tokio::{io::AsyncReadExt, sync::RwLock};

/// Result of a completed exec command.
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub task_id: String,
    pub tool_call_id: String,
    pub command: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub started_at: Instant,
    pub completed_at: Instant,
}

/// A background task that is still running.
#[derive(Debug, Clone)]
struct RunningTask {
    session_key: String,
    #[allow(dead_code)]
    started_at: Instant,
}

/// Default cap on concurrently running background exec tasks
/// (`tools.exec.maxBackground`).
pub const DEFAULT_MAX_BACKGROUND: usize = 4;

/// Global exec pool — managed as an Arc on AgentRuntime so all turns
/// share the same pool and can collect results.
pub struct ExecPool {
    /// Active tasks. Key is task_id.
    tasks: RwLock<HashMap<String, RunningTask>>,
    /// Completed results pending collection, keyed by `session:<key>`.
    pending_results: RwLock<HashMap<String, Vec<ExecResult>>>,
    /// Max concurrent background tasks (0 = unlimited).
    max_concurrent: usize,
}

impl ExecPool {
    /// Create a new pool with the given concurrency limit (0 = unlimited).
    pub fn new(max_concurrent: usize) -> Arc<Self> {
        Arc::new(Self {
            tasks: RwLock::new(HashMap::new()),
            pending_results: RwLock::new(HashMap::new()),
            max_concurrent,
        })
    }

    /// Register a background task as running. Returns `false` (and does not
    /// register) when the pool is already at `max_concurrent`.
    pub async fn try_begin(&self, task_id: &str, session_key: &str) -> bool {
        let mut tasks = self.tasks.write().await;
        if self.max_concurrent > 0 && tasks.len() >= self.max_concurrent {
            return false;
        }
        tasks.insert(
            task_id.to_owned(),
            RunningTask {
                session_key: session_key.to_owned(),
                started_at: Instant::now(),
            },
        );
        true
    }

    /// Mark a task finished and queue its result for the owning session.
    pub async fn finish(self: &Arc<Self>, session_key: String, result: ExecResult) {
        self.tasks.write().await.remove(&result.task_id);
        self.add_pending_for_session(session_key, result).await;
    }

    /// Check if a task started by `session_key` is still running.
    pub async fn is_running(&self, session_key: &str, task_id: &str) -> bool {
        let tasks = self.tasks.read().await;
        tasks
            .get(task_id)
            .is_some_and(|t| t.session_key == session_key)
    }

    /// Collect (and remove) the completed result of `task_id`, provided it
    /// belongs to `session_key`. Collected results are not re-delivered at
    /// the start of the next turn.
    pub async fn try_collect_by_task(&self, session_key: &str, task_id: &str) -> Option<ExecResult> {
        let mut pending = self.pending_results.write().await;
        let key = format!("session:{session_key}");
        let results = pending.get_mut(&key)?;
        let pos = results.iter().position(|r| r.task_id == task_id)?;
        let result = results.remove(pos);
        if results.is_empty() {
            pending.remove(&key);
        }
        tracing::debug!(task_id = %task_id, "exec_pool: result collected by task_id");
        Some(result)
    }

    /// Collect all pending results for a session.
    pub async fn collect_pending_for_session(
        self: &Arc<Self>,
        session_key: &str,
    ) -> Vec<ExecResult> {
        let mut pending = self.pending_results.write().await;
        let key = format!("session:{session_key}");
        match pending.remove(&key) {
            Some(results) => {
                tracing::info!(
                    session_key = %session_key,
                    count = results.len(),
                    "exec_pool: collected results for session"
                );
                results
            }
            None => Vec::new(),
        }
    }

    /// Store a completed result in the pending queue for a session.
    pub async fn add_pending_for_session(
        self: &Arc<Self>,
        session_key: String,
        result: ExecResult,
    ) {
        tracing::info!(
            session_key = %session_key,
            task_id = %result.task_id,
            exit_code = ?result.exit_code,
            "exec_pool: adding pending result for session"
        );
        let mut pending = self.pending_results.write().await;
        pending
            .entry(format!("session:{session_key}"))
            .or_default()
            .push(result);
    }

    /// Get the number of currently running tasks.
    pub async fn running_count(&self) -> usize {
        self.tasks.read().await.len()
    }

    /// Get the number of pending results.
    pub async fn pending_count(&self) -> usize {
        let pending = self.pending_results.read().await;
        pending.values().map(|v| v.len()).sum()
    }
}

// ---------------------------------------------------------------------------
// Bounded child-process runner
// ---------------------------------------------------------------------------

/// Default per-stream capture cap for agent-run commands.
pub(crate) const EXEC_OUTPUT_CAP: usize = 1024 * 1024;

/// Output of a command run through [`run_capped`].
#[derive(Debug, Default)]
pub(crate) struct CappedOutput {
    /// Exit status; `None` when the command timed out and was killed.
    pub(crate) status: Option<std::process::ExitStatus>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr_truncated: bool,
    pub(crate) timed_out: bool,
}

impl CappedOutput {
    /// Exit code, when the process exited normally.
    pub(crate) fn code(&self) -> Option<i32> {
        self.status.and_then(|s| s.code())
    }

    /// Lossy UTF-8 stdout with a truncation note appended when capped.
    pub(crate) fn stdout_text(&self) -> String {
        with_trunc_note(&self.stdout, self.stdout_truncated)
    }

    /// Lossy UTF-8 stderr with a truncation note appended when capped.
    pub(crate) fn stderr_text(&self) -> String {
        with_trunc_note(&self.stderr, self.stderr_truncated)
    }
}

fn with_trunc_note(bytes: &[u8], truncated: bool) -> String {
    let mut s = String::from_utf8_lossy(bytes).into_owned();
    if truncated {
        s.push_str(&format!(
            "\n[output truncated at {} bytes — redirect to a file and read it in parts]",
            bytes.len()
        ));
    }
    s
}

/// Put the child in its own process group (Unix) so a timeout can kill the
/// whole tree, not just the direct child (`sh -c` / PowerShell).
pub(crate) fn isolate_process_group(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _unused = cmd;
    }
}

/// Kill the process tree rooted at `pid` (best effort). On Unix the child
/// must have been started with [`isolate_process_group`], making its pgid
/// equal to its pid.
pub(crate) fn kill_process_tree(pid: u32) {
    #[cfg(unix)]
    {
        let res = std::process::Command::new("kill")
            .args(["-s", "KILL", "--", &format!("-{pid}")])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if let Err(e) = res {
            tracing::warn!(pid, error = %e, "kill_process_tree: kill failed");
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let res = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if let Err(e) = res {
            tracing::warn!(pid, error = %e, "kill_process_tree: taskkill failed");
        }
    }
}

/// Read `reader` to EOF keeping at most `cap` bytes. Keeps draining past the
/// cap so the child never blocks on a full pipe.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    reader: Option<R>,
    cap: usize,
) -> (Vec<u8>, bool) {
    let Some(mut r) = reader else {
        return (Vec::new(), false);
    };
    let mut out = Vec::new();
    let mut truncated = false;
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match r.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let room = cap.saturating_sub(out.len());
                if n > room {
                    truncated = true;
                }
                out.extend_from_slice(&buf[..n.min(room)]);
            }
            Err(e) => {
                tracing::debug!(error = %e, "read_capped: pipe read error");
                break;
            }
        }
    }
    (out, truncated)
}

/// Spawn `cmd` and wait for it with a wall-clock `timeout`, capturing at
/// most `cap` bytes of stdout and of stderr (stdio configuration set by the
/// caller is overridden to piped; pass `capture_stderr = false` to discard
/// stderr). The child runs in its own process group; on timeout the whole
/// tree is killed. `kill_on_drop` is set so a cancelled caller also reaps it.
pub(crate) async fn run_capped(
    mut cmd: tokio::process::Command,
    timeout: std::time::Duration,
    cap: usize,
    capture_stderr: bool,
) -> std::io::Result<CappedOutput> {
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(if capture_stderr {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .kill_on_drop(true);
    isolate_process_group(&mut cmd);
    let mut child = cmd.spawn()?;
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let waited = tokio::time::timeout(timeout, async {
        tokio::join!(
            read_capped(stdout, cap),
            read_capped(stderr, cap),
            child.wait()
        )
    })
    .await;

    match waited {
        Ok(((out, out_trunc), (err, err_trunc), status)) => Ok(CappedOutput {
            status: Some(status?),
            stdout: out,
            stderr: err,
            stdout_truncated: out_trunc,
            stderr_truncated: err_trunc,
            timed_out: false,
        }),
        Err(_) => {
            if let Some(pid) = pid {
                kill_process_tree(pid);
            }
            if let Err(e) = child.start_kill() {
                tracing::debug!(error = %e, "run_capped: start_kill after timeout");
            }
            if tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
                .await
                .is_err()
            {
                tracing::warn!(?pid, "run_capped: child did not exit after kill");
            }
            Ok(CappedOutput {
                timed_out: true,
                ..Default::default()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exec_pool_poll_by_task_is_session_scoped() {
        let pool = ExecPool::new(1);
        assert!(pool.try_begin("t1", "s1").await);
        assert!(!pool.try_begin("t2", "s1").await, "limit enforced");
        assert!(pool.is_running("s1", "t1").await);
        assert!(!pool.is_running("s2", "t1").await);
        let now = Instant::now();
        pool.finish(
            "s1".to_owned(),
            ExecResult {
                task_id: "t1".to_owned(),
                tool_call_id: String::new(),
                command: "true".to_owned(),
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                started_at: now,
                completed_at: now,
            },
        )
        .await;
        assert!(!pool.is_running("s1", "t1").await);
        assert!(pool.try_collect_by_task("s2", "t1").await.is_none());
        assert!(pool.try_collect_by_task("s1", "t1").await.is_some());
        assert!(pool.collect_pending_for_session("s1").await.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exec_pool_run_capped_truncates_and_times_out() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "yes | head -c 100000"]);
        let out = run_capped(cmd, std::time::Duration::from_secs(10), 1000, true)
            .await
            .expect("spawn");
        assert_eq!(out.stdout.len(), 1000);
        assert!(out.stdout_truncated);

        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "sleep 30 & sleep 30"]);
        let started = Instant::now();
        let out = run_capped(cmd, std::time::Duration::from_millis(300), 1000, true)
            .await
            .expect("spawn");
        assert!(out.timed_out);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }
}
