//! Async exec pool — runs long-running commands in background without blocking
//! the agent's main loop. Results are stored per-session for collection on
//! subsequent turns.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;

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

/// Global exec pool — managed as an Arc on AgentRuntime so all turns
/// share the same pool and can collect results.
pub struct ExecPool {
    /// Active tasks. Key is task_id.
    tasks: RwLock<HashMap<String, Instant>>,
    /// Completed results pending collection, keyed by session_key.
    pending_results: RwLock<HashMap<String, Vec<ExecResult>>>,
    /// Max concurrent exec tasks.
    #[allow(dead_code)]
    max_concurrent: usize,
}

impl ExecPool {
    /// Create a new pool with the given concurrency limit.
    pub fn new(max_concurrent: usize) -> Arc<Self> {
        Arc::new(Self {
            tasks: RwLock::new(HashMap::new()),
            pending_results: RwLock::new(HashMap::new()),
            max_concurrent,
        })
    }

    /// Register a task as running. Call this BEFORE spawning.
    pub async fn register_running(&self, task_id: String) {
        tracing::debug!(task_id = %task_id, "exec_pool: task registered as running");
        let mut tasks = self.tasks.write().await;
        tasks.insert(task_id, Instant::now());
    }

    /// Unregister a task (mark as no longer running). Call this AFTER completion.
    pub async fn unregister_running(&self, task_id: &str) {
        let mut tasks = self.tasks.write().await;
        tasks.remove(task_id);
        tracing::debug!(task_id = %task_id, "exec_pool: task unregistered (no longer running)");
    }

    /// Spawn a command in the background. The result will be stored
    /// in `pending_results` keyed by session_key and can be retrieved
    /// via `collect_pending_for_session()`.
    pub async fn spawn(
        self: &Arc<Self>,
        task_id: String,
        command: String,
        cwd: PathBuf,
        timeout_secs: u64,
    ) {
        let started_at = Instant::now();

        // Store the task entry (indicates running)
        {
            let mut tasks = self.tasks.write().await;
            tasks.insert(task_id.clone(), started_at);
        }

        // Spawn the background runner
        let pool = Arc::clone(self);
        let tid = task_id.clone();
        let cmd = command;
        let cw = cwd;

        tokio::spawn(async move {
            let completed_at = Instant::now();

            // Determine shell based on platform
            let (shell, shell_args) = if cfg!(target_os = "windows") {
                ("powershell", vec!["-NoProfile", "-Command"])
            } else {
                ("sh", vec!["-c"])
            };

            // Run the command with timeout
            // - kill_on_drop ensures process is killed if future is dropped during timeout
            // - stdin null prevents interactive prompts from blocking (e.g. PowerShell waiting for input)
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                tokio::process::Command::new(shell)
                    .args(&shell_args)
                    .arg(&cmd)
                    .current_dir(&cw)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true)
                    .output(),
            )
            .await;

            let (exit_code, stdout, stderr) = match result {
                Ok(Ok(output)) => {
                    let exit_code = output.status.code();
                    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                    (exit_code, stdout, stderr)
                }
                Ok(Err(e)) => {
                    tracing::error!(task_id = %tid, "exec background spawn failed: {}", e);
                    (None, String::new(), format!("spawn error: {}", e))
                }
                Err(_) => {
                    tracing::warn!(task_id = %tid, timeout_secs, "exec background timed out");
                    (None, String::new(), format!("timed out after {} seconds", timeout_secs))
                }
            };

            tracing::info!(
                task_id = %tid,
                exit_code = ?exit_code,
                stdout_len = stdout.len(),
                stderr_len = stderr.len(),
                "exec background completed"
            );

            // Remove from running tasks
            let mut tasks = pool.tasks.write().await;
            tasks.remove(&tid);
            drop(tasks);

            // Store result - will be collected by session_key in tool_exec
            // Note: spawn() is not currently used; tool_exec builds ExecResult directly
            // with full tool_call_id and command fields.
            let exec_result = ExecResult {
                task_id: tid.clone(),
                tool_call_id: String::new(), // placeholder, not used
                command: String::new(),      // placeholder, not used
                exit_code,
                stdout,
                stderr,
                started_at,
                completed_at,
            };

            // Store with a placeholder key; tool_exec will re-store with session_key
            pool.add_pending_for_task(&tid, exec_result).await;
        });

        tracing::info!(task_id = %task_id, "exec background spawned");
    }

    /// Add a pending result for a task (for polling by task_id).
    pub async fn add_pending_for_task(&self, task_id: &str, result: ExecResult) {
        tracing::info!(
            task_id = %task_id,
            exit_code = ?result.exit_code,
            "exec_pool: adding pending result for task (polling)"
        );
        let mut pending = self.pending_results.write().await;
        pending
            .entry(format!("task:{}", task_id))
            .or_insert_with(Vec::new)
            .push(result);
    }

    /// Check if a task is still running.
    pub async fn is_running(&self, task_id: &str) -> bool {
        let tasks = self.tasks.read().await;
        let is_running = tasks.contains_key(task_id);
        tracing::debug!(
            task_id = %task_id,
            is_running = is_running,
            running_count = tasks.len(),
            "exec_pool: is_running check"
        );
        is_running
    }

    /// Collect a completed result for a task by task_id.
    pub async fn try_collect_by_task(&self, task_id: &str) -> Option<ExecResult> {
        tracing::info!(task_id = %task_id, "exec_pool: trying to collect result by task_id");
        let mut pending = self.pending_results.write().await;
        let key = format!("task:{}", task_id);
        if let Some(mut results) = pending.remove(&key) {
            let result = results.pop();
            tracing::info!(
                task_id = %task_id,
                found = result.is_some(),
                remaining_in_list = results.len(),
                "exec_pool: result collected from task key"
            );
            result
        } else {
            // Also try session key format (results stored from runtime.rs spawn)
            tracing::debug!(
                task_id = %task_id,
                pending_keys = ?pending.keys().collect::<Vec<_>>(),
                "exec_pool: task key not found, showing all pending keys"
            );
            None
        }
    }

    /// Collect all pending results for a session.
    pub async fn collect_pending_for_session(
        self: &Arc<Self>,
        session_key: &str,
    ) -> Vec<ExecResult> {
        tracing::info!(
            session_key = %session_key,
            "exec_pool: collecting pending results for session"
        );
        let mut pending = self.pending_results.write().await;
        let key = format!("session:{}", session_key);

        tracing::debug!(
            session_key = %session_key,
            key = %key,
            all_keys = ?pending.keys().collect::<Vec<_>>(),
            "exec_pool: checking pending_results keys"
        );

        if let Some(results) = pending.remove(&key) {
            tracing::info!(
                session_key = %session_key,
                count = results.len(),
                task_ids = ?results.iter().map(|r| &r.task_id).collect::<Vec<_>>(),
                "exec_pool: collected results for session"
            );
            results
        } else {
            tracing::debug!(
                session_key = %session_key,
                "exec_pool: no results found for session key"
            );
            Vec::new()
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
            tool_call_id = %result.tool_call_id,
            exit_code = ?result.exit_code,
            "exec_pool: adding pending result for session"
        );
        let mut pending = self.pending_results.write().await;
        let key = format!("session:{}", session_key);
        let entry = pending.entry(key).or_insert_with(Vec::new);
        let prev_len = entry.len();
        entry.push(result);
        tracing::debug!(
            session_key = %session_key,
            prev_len = prev_len,
            new_len = entry.len(),
            "exec_pool: result added to pending queue"
        );
    }

    /// Get the number of currently running tasks.
    pub async fn running_count(&self) -> usize {
        let tasks = self.tasks.read().await;
        tasks.len()
    }

    /// Get the number of pending results.
    pub async fn pending_count(&self) -> usize {
        let pending = self.pending_results.read().await;
        pending.values().map(|v| v.len()).sum()
    }
}
