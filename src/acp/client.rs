//! ACP (Agent Client Protocol) client implementation
//!
//! Key insight from debugging:
//! - tokio's Lines iterator can miss wake-ups when used in a shared task
//! - Solution: dedicated subprocess task with manual polling
//!
//! Full ACP spec: https://agentclientprotocol.com

use std::{collections::HashMap, process::Stdio, sync::Arc};

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, broadcast, mpsc},
    time::{Duration, timeout},
};

use crate::acp::{methods, notification::*, types::*};

pub const ACP_TIMEOUT: Duration = Duration::from_secs(60);
pub const LONG_TIMEOUT: Duration = Duration::from_secs(300);
/// Default timeout for initialization/session creation - these can take longer as the agent
/// may need to load MCP servers, check environment, etc.
pub const DEFAULT_INIT_TIMEOUT: Duration = Duration::from_secs(600); // 10 minutes

// ---------------------------------------------------------------------------
// Callback Handlers (Agent → Client)
// ---------------------------------------------------------------------------

/// Handler for Agent -> Client requests (permissions, fs, terminal).
/// Uses BoxFuture for dyn-safety (used as `dyn AcpCallbackHandler`).
pub trait AcpCallbackHandler: Send + Sync {
    /// Handle permission request from agent
    fn handle_request_permission(
        &self,
        session_id: &SessionId,
        tool_call_id: &str,
        options: Vec<PermissionOption>,
    ) -> BoxFuture<'_, RequestPermissionOutcome>;

    /// Handle file read request from agent
    fn handle_read_text_file(&self, session_id: &SessionId, path: &str) -> BoxFuture<'_, Result<String>>;

    /// Handle file write request from agent
    fn handle_write_text_file(
        &self,
        session_id: &SessionId,
        path: &str,
        contents: &str,
    ) -> BoxFuture<'_, Result<()>>;

    /// Handle terminal create request from agent
    fn handle_terminal_create(
        &self,
        session_id: &SessionId,
        command: Option<&str>,
        args: Option<Vec<String>>,
    ) -> BoxFuture<'_, Result<String>>;

    /// Handle terminal output request from agent
    fn handle_terminal_output(
        &self,
        session_id: &SessionId,
        terminal_id: &str,
    ) -> BoxFuture<'_, Result<TerminalOutputResponse>>;

    /// Handle terminal kill request from agent
    fn handle_terminal_kill(&self, session_id: &SessionId, terminal_id: &str) -> BoxFuture<'_, Result<()>>;

    /// Handle terminal release request from agent
    fn handle_terminal_release(
        &self,
        session_id: &SessionId,
        terminal_id: &str,
    ) -> BoxFuture<'_, Result<()>>;

    /// Handle terminal wait for exit request from agent
    fn handle_terminal_wait_for_exit(
        &self,
        session_id: &SessionId,
        terminal_id: &str,
    ) -> BoxFuture<'_, Result<Option<i32>>>;
}

/// Default callback handler that auto-approves everything.
// TODO(H-21): DefaultAcpHandler and DefaultAcpHandlerWithTerminal share nearly
// identical handle_request_permission / handle_read_text_file / handle_write_text_file
// implementations.  Extract a shared helper or blanket impl to reduce duplication.
pub struct DefaultAcpHandler;

impl AcpCallbackHandler for DefaultAcpHandler {
    fn handle_request_permission(
        &self,
        _session_id: &SessionId,
        _tool_call_id: &str,
        options: Vec<PermissionOption>,
    ) -> BoxFuture<'_, RequestPermissionOutcome> {
        Box::pin(async move {
            // Log all options for debugging
            tracing::debug!(
                options = ?options.iter().map(|o| (&o.option_id, &o.kind)).collect::<Vec<_>>(),
                "handle_request_permission: received options"
            );

            // Auto-approve any non-reject option (more lenient for different agent implementations)
            for opt in options {
                // Match any "allow" type option
                if matches!(
                    opt.kind,
                    PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
                ) {
                    tracing::debug!(
                        option_id = %opt.option_id,
                        kind = ?opt.kind,
                        "handle_request_permission: auto-approving"
                    );
                    return RequestPermissionOutcome::Selected {
                        option_id: opt.option_id,
                    };
                }
                // Also approve if option_id contains "allow" or "accept" (fallback for non-standard formats)
                if opt.option_id.contains("allow") || opt.option_id.contains("accept") {
                    tracing::debug!(
                        option_id = %opt.option_id,
                        "handle_request_permission: auto-approving by option_id pattern"
                    );
                    return RequestPermissionOutcome::Selected {
                        option_id: opt.option_id,
                    };
                }
            }
            tracing::warn!("handle_request_permission: no matching allow option found, cancelling");
            RequestPermissionOutcome::Cancelled
        })
    }

    fn handle_read_text_file(&self, _session_id: &SessionId, path: &str) -> BoxFuture<'_, Result<String>> {
        let path = path.to_owned();
        Box::pin(async move {
            tokio::fs::read_to_string(&path)
                .await
                .context("Failed to read file")
        })
    }

    fn handle_write_text_file(
        &self,
        _session_id: &SessionId,
        path: &str,
        contents: &str,
    ) -> BoxFuture<'_, Result<()>> {
        let path = path.to_owned();
        let contents = contents.to_owned();
        Box::pin(async move {
            tokio::fs::write(&path, &contents)
                .await
                .context("Failed to write file")
        })
    }

    fn handle_terminal_create(
        &self,
        _session_id: &SessionId,
        _command: Option<&str>,
        _args: Option<Vec<String>>,
    ) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move {
            Err(anyhow::anyhow!(
                "Terminal operations not implemented in DefaultAcpHandler. Implement custom AcpCallbackHandler to enable terminal support."
            ))
        })
    }

    fn handle_terminal_output(
        &self,
        _session_id: &SessionId,
        _terminal_id: &str,
    ) -> BoxFuture<'_, Result<TerminalOutputResponse>> {
        Box::pin(async move {
            Err(anyhow::anyhow!(
                "Terminal operations not implemented in DefaultAcpHandler"
            ))
        })
    }

    fn handle_terminal_kill(
        &self,
        _session_id: &SessionId,
        _terminal_id: &str,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            Err(anyhow::anyhow!(
                "Terminal operations not implemented in DefaultAcpHandler"
            ))
        })
    }

    fn handle_terminal_release(
        &self,
        _session_id: &SessionId,
        _terminal_id: &str,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            Err(anyhow::anyhow!(
                "Terminal operations not implemented in DefaultAcpHandler"
            ))
        })
    }

    fn handle_terminal_wait_for_exit(
        &self,
        _session_id: &SessionId,
        _terminal_id: &str,
    ) -> BoxFuture<'_, Result<Option<i32>>> {
        Box::pin(async move {
            Err(anyhow::anyhow!(
                "Terminal operations not implemented in DefaultAcpHandler"
            ))
        })
    }
}

/// Callback handler with terminal support
pub struct DefaultAcpHandlerWithTerminal {
    state: Arc<Mutex<AcpState>>,
}

impl DefaultAcpHandlerWithTerminal {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(AcpState {
                next_id: 1,
                session_id: None,
                pending_requests: HashMap::new(),
                handler: Arc::new(DefaultAcpHandler),
                capabilities: None,
                agent_info: None,
                config_options: Vec::new(),
                models: None,
                terminals: HashMap::new(),
            })),
        }
    }
}

impl AcpCallbackHandler for DefaultAcpHandlerWithTerminal {
    fn handle_request_permission(
        &self,
        _session_id: &SessionId,
        _tool_call_id: &str,
        options: Vec<PermissionOption>,
    ) -> BoxFuture<'_, RequestPermissionOutcome> {
        Box::pin(async move {
            // Log all options for debugging
            tracing::debug!(
                options = ?options.iter().map(|o| (&o.option_id, &o.kind)).collect::<Vec<_>>(),
                "handle_request_permission: received options"
            );

            // Auto-approve any non-reject option (more lenient for different agent implementations)
            for opt in options {
                // Match any "allow" type option
                if matches!(
                    opt.kind,
                    PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
                ) {
                    tracing::debug!(
                        option_id = %opt.option_id,
                        kind = ?opt.kind,
                        "handle_request_permission: auto-approving"
                    );
                    return RequestPermissionOutcome::Selected {
                        option_id: opt.option_id,
                    };
                }
                // Also approve if option_id contains "allow" or "accept" (fallback for non-standard formats)
                if opt.option_id.contains("allow") || opt.option_id.contains("accept") {
                    tracing::debug!(
                        option_id = %opt.option_id,
                        "handle_request_permission: auto-approving by option_id pattern"
                    );
                    return RequestPermissionOutcome::Selected {
                        option_id: opt.option_id,
                    };
                }
            }
            tracing::warn!("handle_request_permission: no matching allow option found, cancelling");
            RequestPermissionOutcome::Cancelled
        })
    }

    fn handle_read_text_file(&self, _session_id: &SessionId, path: &str) -> BoxFuture<'_, Result<String>> {
        let path = path.to_owned();
        Box::pin(async move {
            tokio::fs::read_to_string(&path)
                .await
                .context("Failed to read file")
        })
    }

    fn handle_write_text_file(
        &self,
        _session_id: &SessionId,
        path: &str,
        contents: &str,
    ) -> BoxFuture<'_, Result<()>> {
        let path = path.to_owned();
        let contents = contents.to_owned();
        Box::pin(async move {
            tokio::fs::write(&path, &contents)
                .await
                .context("Failed to write file")
        })
    }

    fn handle_terminal_create(
        &self,
        _session_id: &SessionId,
        command: Option<&str>,
        _args: Option<Vec<String>>,
    ) -> BoxFuture<'_, Result<String>> {
        let command = command.map(|s| s.to_owned());
        Box::pin(async move {
            let default_shell = if cfg!(target_os = "windows") { "powershell.exe" } else { "sh" };
            let shell = command.as_deref().unwrap_or(default_shell);
            let mut cmd = Command::new(shell);
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x08000000;
                cmd.creation_flags(CREATE_NO_WINDOW);
            }
            let child = cmd.spawn()
                .context("Failed to spawn terminal process")?;
            let terminal_id = format!("terminal-{}", uuid::Uuid::new_v4());

            let mut state = self.state.lock().await;
            state.terminals.insert(terminal_id.clone(), child);

            tracing::info!(terminal_id = %terminal_id, "terminal created");
            Ok(terminal_id)
        })
    }

    fn handle_terminal_output(
        &self,
        _session_id: &SessionId,
        terminal_id: &str,
    ) -> BoxFuture<'_, Result<TerminalOutputResponse>> {
        let terminal_id = terminal_id.to_owned();
        Box::pin(async move {
            let mut state = self.state.lock().await;

            let child = state
                .terminals
                .get_mut(&terminal_id)
                .ok_or_else(|| anyhow::anyhow!("terminal not found: {}", terminal_id))?;

            let mut stdout_buf = String::new();
            let mut stderr_buf = String::new();

            if let Some(stdout) = child.stdout.as_mut() {
                let mut reader = BufReader::new(stdout);
                reader.read_line(&mut stdout_buf).await.ok();
            }

            if let Some(stderr) = child.stderr.as_mut() {
                let mut reader = BufReader::new(stderr);
                reader.read_line(&mut stderr_buf).await.ok();
            }

            let exit = match child.try_wait()? {
                Some(status) => status.code(),
                None => None,
            };

            tracing::debug!(terminal_id = %terminal_id, "terminal output read");

            Ok(TerminalOutputResponse {
                exit,
                stdout: stdout_buf,
                stderr: stderr_buf,
            })
        })
    }

    fn handle_terminal_kill(&self, _session_id: &SessionId, terminal_id: &str) -> BoxFuture<'_, Result<()>> {
        let terminal_id = terminal_id.to_owned();
        Box::pin(async move {
            let mut state = self.state.lock().await;

            if let Some(mut child) = state.terminals.remove(&terminal_id) {
                child.kill().await.ok();
                tracing::info!(terminal_id = %terminal_id, "terminal killed");
            }

            Ok(())
        })
    }

    fn handle_terminal_release(
        &self,
        _session_id: &SessionId,
        terminal_id: &str,
    ) -> BoxFuture<'_, Result<()>> {
        let terminal_id = terminal_id.to_owned();
        Box::pin(async move {
            let mut state = self.state.lock().await;

            if let Some(mut child) = state.terminals.remove(&terminal_id) {
                child.wait().await.ok();
                tracing::info!(terminal_id = %terminal_id, "terminal released");
            }

            Ok(())
        })
    }

    fn handle_terminal_wait_for_exit(
        &self,
        _session_id: &SessionId,
        terminal_id: &str,
    ) -> BoxFuture<'_, Result<Option<i32>>> {
        let terminal_id = terminal_id.to_owned();
        Box::pin(async move {
            let mut state = self.state.lock().await;

            if let Some(child) = state.terminals.get_mut(&terminal_id) {
                let status = child.wait().await.context("Failed to wait for terminal")?;
                let code = status.code();
                tracing::info!(terminal_id = %terminal_id, exit_code = ?code, "terminal exited");
                return Ok(code);
            }

            Ok(None)
        })
    }
}

// ---------------------------------------------------------------------------
// Session Update Events
// ---------------------------------------------------------------------------

/// Session update event from agent
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Agent message chunk
    AgentMessageChunk { content: String },
    /// Agent thought chunk
    AgentThoughtChunk { content: String },
    /// Tool call started
    ToolCallStarted {
        tool_call_id: String,
        title: Option<String>,
        kind: ToolKind,
    },
    /// Tool call in progress
    ToolCallInProgress { tool_call_id: String },
    /// Tool call completed
    ToolCallCompleted {
        tool_call_id: String,
        result: Option<String>,
    },
    /// Tool call failed
    ToolCallFailed { tool_call_id: String, error: String },
    /// Mode changed
    ModeChanged { mode_id: String },
    /// Config option updated
    ConfigOptionUpdated { options: Vec<SessionConfigOption> },
    /// Session info updated
    SessionInfoUpdated {
        title: Option<String>,
        updated_at: Option<String>,
    },
    /// Usage updated
    UsageUpdated { used: u32, size: u32 },
    /// Available commands updated
    AvailableCommandsUpdated { commands: Vec<AvailableCommand> },
}

// ---------------------------------------------------------------------------
// Internal State
// ---------------------------------------------------------------------------

/// Shared state for the ACP client.
#[allow(dead_code)]
struct AcpState {
    next_id: i64,
    session_id: Option<SessionId>,
    pending_requests: HashMap<i64, mpsc::Sender<Result<serde_json::Value>>>,
    handler: Arc<dyn AcpCallbackHandler>,
    capabilities: Option<AgentCapabilities>,
    agent_info: Option<Implementation>,
    config_options: Vec<SessionConfigOption>,
    models: Option<SessionModels>,
    /// Active terminal processes
    terminals: HashMap<String, Child>,
}

/// Commands for subprocess task
#[derive(Debug)]
enum SubprocessCmd {
    SendRequest {
        request: String,
        response_tx: mpsc::Sender<Result<serde_json::Value>>,
    },
    Shutdown,
}

/// Internal message from subprocess to client
#[derive(Debug)]
#[allow(dead_code)]
enum SubprocessEvent {
    Response {
        id: i64,
        result: Result<serde_json::Value>,
    },
    SessionUpdate {
        session_id: SessionId,
        event: SessionEvent,
    },
    AgentRequest {
        request: serde_json::Value,
    },
}

// ---------------------------------------------------------------------------
// ACP Client
// ---------------------------------------------------------------------------

/// ACP client for communicating with ACP-compatible agents.
#[derive(Clone)]
pub struct AcpClient {
    cmd_tx: Arc<Mutex<Option<mpsc::Sender<SubprocessCmd>>>>,
    state: Arc<Mutex<AcpState>>,
    collected_content: Arc<Mutex<String>>,
    event_tx: broadcast::Sender<SessionEvent>,
    notification_manager: Arc<Mutex<NotificationManager>>,
    /// Timeout for initialization and session creation operations.
    /// Can be configured via OpenCodeConfig/ClaudeCodeConfig/CodexConfig.
    init_timeout: Duration,
}

impl AcpClient {
    pub async fn spawn(command: &str, args: &[&str]) -> Result<Self> {
        Self::spawn_with_timeout(
            command,
            args,
            Arc::new(DefaultAcpHandler),
            Arc::new(Mutex::new(NotificationManager::new())),
            None,
        )
        .await
    }

    /// Spawn with custom init timeout (seconds).
    /// If timeout_secs is None, uses DEFAULT_INIT_TIMEOUT (600s).
    pub async fn spawn_with_timeout(
        command: &str,
        args: &[&str],
        handler: Arc<dyn AcpCallbackHandler>,
        notification_manager: Arc<Mutex<NotificationManager>>,
        init_timeout_secs: Option<u64>,
    ) -> Result<Self> {
        let init_timeout = init_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_INIT_TIMEOUT);

        // First, check if the command exists (for better error messages)
        let command_path = which::which(command)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| command.to_string());

        // Try to spawn the process first to catch errors early
        let test_child = std::process::Command::new(&command_path)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();

        match test_child {
            Ok(mut child) => {
                // Process started successfully, kill it and spawn the real one
                let _ = child.kill();
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Failed to spawn ACP subprocess '{}': {}. Please ensure the command exists and is executable.",
                    command_path,
                    e
                ));
            }
        }

        let command_owned = command_path.clone();
        let args_owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();

        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let cmd_tx_clone = cmd_tx.clone();
        let collected = Arc::new(Mutex::new(String::new()));
        let collected_clone = collected.clone();
        let handler_clone = handler.clone();
        let notification_manager_clone = notification_manager.clone();

        let (event_tx, _) = broadcast::channel(256);
        let event_tx_clone = event_tx.clone();

        tokio::spawn(async move {
            let _ = run_subprocess(
                &command_owned,
                &args_owned,
                cmd_rx,
                collected_clone,
                handler_clone,
                event_tx_clone,
                notification_manager_clone,
            ).await;
        });

        // Brief startup delay to allow subprocess to initialize
        tokio::time::sleep(Duration::from_millis(100)).await;

        Ok(Self {
            cmd_tx: Arc::new(Mutex::new(Some(cmd_tx_clone))),
            state: Arc::new(Mutex::new(AcpState {
                next_id: 1,
                session_id: None,
                pending_requests: HashMap::new(),
                handler,
                capabilities: None,
                agent_info: None,
                config_options: Vec::new(),
                models: None,
                terminals: HashMap::new(),
            })),
            collected_content: collected,
            event_tx,
            notification_manager,
            init_timeout,
        })
    }

    /// Legacy spawn_with_handler for backwards compatibility.
    pub async fn spawn_with_handler(
        command: &str,
        args: &[&str],
        handler: Arc<dyn AcpCallbackHandler>,
        notification_manager: Arc<Mutex<NotificationManager>>,
    ) -> Result<Self> {
        Self::spawn_with_timeout(command, args, handler, notification_manager, None).await
    }

    /// Subscribe to session update events
    pub fn subscribe_events(&self) -> broadcast::Receiver<SessionEvent> {
        self.event_tx.subscribe()
    }

    /// Add a notification sink for sending critical events to channels
    pub fn add_notification_sink(&self, sink: Arc<dyn NotificationSink>) {
        if let Ok(mut guard) = self.notification_manager.try_lock() {
            guard.add_sink(sink);
        }
    }

    /// Get the collected content from notifications
    pub async fn get_collected_content(&self) -> String {
        self.collected_content.lock().await.clone()
    }

    /// Clear collected content
    pub async fn clear_collected_content(&self) {
        self.collected_content.lock().await.clear();
    }

    /// Get current session ID
    pub async fn session_id(&self) -> Option<SessionId> {
        self.state.lock().await.session_id.clone()
    }

    /// Get agent capabilities (available after initialize)
    pub async fn capabilities(&self) -> Option<AgentCapabilities> {
        self.state.lock().await.capabilities.clone()
    }

    /// Get agent info (available after initialize)
    pub async fn agent_info(&self) -> Option<Implementation> {
        self.state.lock().await.agent_info.clone()
    }

    pub async fn config_options(&self) -> Vec<SessionConfigOption> {
        self.state.lock().await.config_options.clone()
    }

    pub async fn models(&self) -> Option<SessionModels> {
        self.state.lock().await.models.clone()
    }

    /// Check if agent supports loading sessions
    pub async fn supports_load_session(&self) -> bool {
        self.state
            .lock()
            .await
            .capabilities
            .as_ref()
            .and_then(|c| c.load_session)
            .unwrap_or(false)
    }

    /// Check if agent supports images in prompts
    pub async fn supports_images(&self) -> bool {
        self.state
            .lock()
            .await
            .capabilities
            .as_ref()
            .and_then(|c| c.prompt_capabilities.as_ref())
            .map(|p| p.image)
            .unwrap_or(false)
    }

    /// Check if agent supports audio in prompts
    pub async fn supports_audio(&self) -> bool {
        self.state
            .lock()
            .await
            .capabilities
            .as_ref()
            .and_then(|c| c.prompt_capabilities.as_ref())
            .map(|p| p.audio)
            .unwrap_or(false)
    }

    /// Check if agent supports embedded context (resources)
    pub async fn supports_embedded_context(&self) -> bool {
        self.state
            .lock()
            .await
            .capabilities
            .as_ref()
            .and_then(|c| c.prompt_capabilities.as_ref())
            .map(|p| p.embedded_context)
            .unwrap_or(false)
    }

    // ---------------------------------------------------------------------------
    // Client → Agent Methods
    // ---------------------------------------------------------------------------

    /// Initialize the ACP connection.
    pub async fn initialize(
        &self,
        client_name: &str,
        client_version: &str,
    ) -> Result<InitializeResponse> {
        tracing::info!("ACP: initializing connection");
        let params = serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientInfo": {"name": client_name, "version": client_version},
            "clientCapabilities": {
                "fs": { "readTextFile": true, "writeTextFile": true },
                "terminal": true
            }
        });
        // Use configurable init_timeout for initialization (can take long to load MCP servers)
        let resp = self.rpc_with_timeout(methods::INITIALIZE, params, self.init_timeout).await?;
        tracing::debug!(response = ?resp, "ACP initialize response");
        let result = resp
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let init_resp: InitializeResponse =
            serde_json::from_value(result).context("Failed to parse initialize response")?;

        // Store capabilities and agent info for later use
        let mut state = self.state.lock().await;
        state.capabilities = Some(init_resp.agent_capabilities.clone());
        state.agent_info = Some(init_resp.agent_info.clone());

        tracing::info!(
            agent_name = ?init_resp.agent_info.name,
            agent_version = ?init_resp.agent_info.version,
            "ACP: connection initialized"
        );
        Ok(init_resp)
    }

    /// Create a new session.
    pub async fn create_session(
        &self,
        cwd: &str,
        model: Option<&str>,
        mcp_servers: Option<Vec<McpServerConfig>>,
    ) -> Result<NewSessionResponse> {
        tracing::info!(cwd = %cwd, model = ?model, "ACP: creating session");
        let mut params = serde_json::json!({
            "cwd": cwd,
            "mcpServers": mcp_servers.unwrap_or_default()
        });

        if let Some(m) = model {
            params["modelId"] = serde_json::json!(m);
            tracing::debug!(model = %m, params = ?params, "Adding modelId to session/new request");
        } else {
            tracing::warn!("create_session: no model provided, will use agent default");
        }

        // Use configurable init_timeout for session creation (can take long to initialize)
        let resp = self.rpc_with_timeout(methods::SESSION_NEW, params, self.init_timeout).await?;
        tracing::debug!(response = ?resp, "ACP session/new response");
        let result = resp
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let session_resp: NewSessionResponse =
            serde_json::from_value(result).context("Failed to parse session/new response")?;

        let mut state = self.state.lock().await;
        state.session_id = Some(session_resp.session_id.clone());
        state.config_options = session_resp.config_options.clone().unwrap_or_default();
        state.models = session_resp.models.clone();

        tracing::info!(
            session_id = %session_resp.session_id,
            model = ?session_resp.models.as_ref().and_then(|m| m.available_models.first()).map(|m| &m.model_id),
            "ACP: session created"
        );

        if let Some(ref models) = session_resp.models {
            tracing::debug!("Available models: {:?}", models.available_models);
        }
        if let Some(ref opts) = session_resp.config_options {
            tracing::debug!("Config options available: {}", opts.len());
            for opt in opts {
                tracing::debug!(
                    "  - {}: {} (current: {})",
                    opt.id,
                    opt.name,
                    opt.current_value
                );
            }
        }

        Ok(session_resp)
    }

    /// Load an existing session.
    pub async fn load_session(
        &self,
        session_id: &SessionId,
        cwd: Option<&str>,
        mcp_servers: Option<Vec<McpServerConfig>>,
    ) -> Result<LoadSessionResponse> {
        let mut params = serde_json::json!({
            "sessionId": session_id,
        });
        if let Some(cwd) = cwd {
            params["cwd"] = serde_json::json!(cwd);
        }
        params["mcpServers"] = serde_json::json!(mcp_servers.unwrap_or_default());

        let resp = self.rpc(methods::SESSION_LOAD, params).await?;
        let result = resp
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let load_resp: LoadSessionResponse =
            serde_json::from_value(result).context("Failed to parse session/load response")?;
        self.state.lock().await.session_id = Some(session_id.to_string());
        Ok(load_resp)
    }

    /// Send a prompt to the agent.
    pub async fn send_prompt(&self, prompt: &str) -> Result<PromptResponse> {
        let session_id = self.session_id().await.context("No active session")?;
        tracing::info!(
            session_id = %session_id,
            prompt_len = prompt.len(),
            "ACP: sending prompt"
        );
        let params = serde_json::json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": prompt}]
        });

        // session/prompt can take a LONG time - use no timeout for this method
        let resp = self
            .rpc_no_timeout(methods::SESSION_PROMPT, params)
            .await
            .map_err(|e| {
                // If it's a timeout or subprocess died error, make it clearer
                if e.to_string().contains("timeout")
                    || e.to_string().contains("Subprocess task died")
                    || e.to_string().contains("Channel closed")
                {
                    let lang = crate::i18n::default_lang();
                    anyhow::anyhow!("{}", crate::i18n::t_fmt("acp_timeout", lang, &[("name", "OpenCode")]))
                } else {
                    e
                }
            })?;

        tracing::debug!("=== send_prompt raw response ===");
        tracing::debug!(
            "Full response: {}",
            serde_json::to_string(&resp).unwrap_or_default()
        );

        let result = resp
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        tracing::debug!("=== send_prompt result ===");
        tracing::debug!(
            "Result: {}",
            serde_json::to_string(&result).unwrap_or_default()
        );

        let prompt_resp: PromptResponse =
            serde_json::from_value(result.clone()).context("Failed to parse prompt response")?;

        tracing::debug!("=== send_prompt parsed ===");
        tracing::debug!("stop_reason: {:?}", prompt_resp.stop_reason);
        tracing::debug!("usage: {:?}", prompt_resp.usage);
        if let Some(ref r) = prompt_resp.result {
            tracing::debug!("content blocks: {}", r.content.len());
            for (i, block) in r.content.iter().enumerate() {
                match block {
                    crate::acp::types::ContentBlock::Text { text } => {
                        tracing::debug!("  [{}] Text: {}", i, text);
                    }
                    crate::acp::types::ContentBlock::Image { .. } => {
                        tracing::debug!("  [{}] Image", i);
                    }
                    crate::acp::types::ContentBlock::Resource { .. } => {
                        tracing::debug!("  [{}] Resource", i);
                    }
                    crate::acp::types::ContentBlock::ResourceLink { .. } => {
                        tracing::debug!("  [{}] ResourceLink", i);
                    }
                }
            }
            if let Some(ref calls) = r.tool_calls {
                tracing::debug!("tool_calls: {} calls", calls.len());
                for (i, call) in calls.iter().enumerate() {
                    tracing::debug!("  [{}] tool_call: id={}, name={}", i, call.id, call.name);
                }
            }
        }

        Ok(prompt_resp)
    }

    /// Send a prompt with content blocks (supports images, resources).
    pub async fn send_prompt_with_content(
        &self,
        prompt: Vec<ContentBlock>,
    ) -> Result<PromptResponse> {
        let session_id = self.session_id().await.context("No active session")?;
        let params = serde_json::json!({
            "sessionId": session_id,
            "prompt": prompt
        });
        // session/prompt can take a LONG time - use no timeout
        let resp = self.rpc_no_timeout(methods::SESSION_PROMPT, params).await?;
        let result = resp
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        serde_json::from_value(result).context("Failed to parse prompt response")
    }

    /// Cancel the current session operation.
    pub async fn cancel_session(&self) -> Result<()> {
        let session_id = self.session_id().await.context("No active session")?;
        let params = serde_json::json!({
            "sessionId": session_id
        });
        // Cancel is a notification, not a request
        self.send_notification(methods::SESSION_CANCEL, params)
            .await?;
        Ok(())
    }

    /// List all sessions.
    pub async fn list_sessions(&self, cwd: Option<&str>) -> Result<ListSessionsResponse> {
        let params = if let Some(cwd) = cwd {
            serde_json::json!({ "cwd": cwd })
        } else {
            serde_json::json!({})
        };
        let resp = self.rpc(methods::SESSION_LIST, params).await?;
        let result = resp
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        serde_json::from_value(result).context("Failed to parse session/list response")
    }

    /// Set the session mode.
    pub async fn set_mode(&self, mode_id: &str) -> Result<()> {
        let session_id = self.session_id().await.context("No active session")?;
        let params = serde_json::json!({
            "sessionId": session_id,
            "modeId": mode_id
        });
        let _resp = self.rpc(methods::SESSION_SET_MODE, params).await?;
        Ok(())
    }

    /// Set the model for the session.
    pub async fn set_model(&self, model_id: &str) -> Result<Vec<SessionConfigOption>> {
        let session_id = self.session_id().await.context("No active session")?;
        let params = serde_json::json!({
            "sessionId": session_id,
            "configId": "model",
            "value": model_id
        });
        tracing::debug!(model_id = %model_id, "Calling session/set_config_option");
        let resp = self.rpc(methods::SESSION_SET_CONFIG_OPTION, params).await?;
        tracing::debug!(response = ?resp, "set_model response");

        let result = resp
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let config_resp: SetSessionConfigOptionResponse =
            serde_json::from_value(result).context("Failed to parse config option response")?;

        let options = config_resp.config_options.unwrap_or_default();
        self.state.lock().await.config_options = options.clone();

        Ok(options)
    }

    /// Set a session config option.
    pub async fn set_config_option(
        &self,
        config_id: &str,
        value: &str,
    ) -> Result<Vec<SessionConfigOption>> {
        let session_id = self.session_id().await.context("No active session")?;
        let params = serde_json::json!({
            "sessionId": session_id,
            "configId": config_id,
            "value": value
        });
        let resp = self.rpc(methods::SESSION_SET_CONFIG_OPTION, params).await?;
        let result = resp
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let config_resp: SetSessionConfigOptionResponse =
            serde_json::from_value(result).context("Failed to parse config option response")?;
        Ok(config_resp.config_options.unwrap_or_default())
    }

    /// Authenticate with the agent.
    pub async fn authenticate(
        &self,
        method_id: &str,
        credentials: Option<serde_json::Value>,
    ) -> Result<()> {
        let params = serde_json::json!({
            "methodId": method_id,
            "credentials": credentials
        });
        let _resp = self.rpc(methods::AUTHENTICATE, params).await?;
        Ok(())
    }

    /// Shutdown the client.
    pub async fn shutdown(self) -> Result<()> {
        let guard = self.cmd_tx.lock().await;
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(SubprocessCmd::Shutdown).await;
        }
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Internal RPC Methods
    // ---------------------------------------------------------------------------

    /// Internal RPC call
    async fn rpc(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let id = {
            let mut state = self.state.lock().await;
            let id = state.next_id;
            state.next_id += 1;
            id
        };

        let (resp_tx, mut resp_rx) = mpsc::channel(1);

        let request = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))?;

        tracing::debug!(method, id, request = %request, "ACP sending request");

        // Send request while holding the lock, then release lock before waiting for response
        {
            let guard = self.cmd_tx.lock().await;
            let tx = guard.as_ref().context("Subprocess task died")?;
            tx.send(SubprocessCmd::SendRequest {
                request,
                response_tx: resp_tx,
            })
            .await
            .context("Failed to send request")?;
        } // Lock released here

        let resp = timeout(LONG_TIMEOUT, resp_rx.recv())
            .await
            .context("RPC timeout")?
            .context("Channel closed")?;

        tracing::debug!(method, id, response = ?resp, "ACP received response");
        resp
    }

    /// RPC call with custom timeout - for initialization/session creation.
    async fn rpc_with_timeout(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout_duration: Duration,
    ) -> Result<serde_json::Value> {
        let id = {
            let mut state = self.state.lock().await;
            let id = state.next_id;
            state.next_id += 1;
            id
        };

        let (resp_tx, mut resp_rx) = mpsc::channel(1);

        let request = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))?;

        tracing::info!(
            method,
            id,
            timeout_secs = timeout_duration.as_secs(),
            request_preview = &request[..request.len().min(200)],
            "ACP: sending request"
        );

        // Send request while holding the lock, then release lock before waiting for response
        {
            let guard = self.cmd_tx.lock().await;
            let tx = guard.as_ref().context("Subprocess task died")?;
            tx.send(SubprocessCmd::SendRequest {
                request,
                response_tx: resp_tx,
            })
            .await
            .context("Failed to send request")?;
        } // Lock released here

        tracing::info!(method, id, "ACP: waiting for response (timeout {}s)", timeout_duration.as_secs());

        let resp = timeout(timeout_duration, resp_rx.recv())
            .await
            .context("RPC timeout")?
            .context("Channel closed")?;

        tracing::info!(method, id, response_preview = ?resp.as_ref().ok().and_then(|r| serde_json::to_string(r).ok()).map(|s| s[..200.min(s.len())].to_string()), "ACP: received response");
        resp
    }


    /// RPC call without timeout - for long-running operations like
    /// session/prompt
    async fn rpc_no_timeout(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = {
            let mut state = self.state.lock().await;
            let id = state.next_id;
            state.next_id += 1;
            id
        };

        let (resp_tx, mut resp_rx) = mpsc::channel(1);

        let request = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))?;

        tracing::debug!(method, id, request = %request, "ACP sending request (no timeout)");

        // Send request while holding the lock, then release lock before waiting for response
        {
            let guard = self.cmd_tx.lock().await;
            let tx = guard.as_ref().context("Subprocess task died")?;
            tx.send(SubprocessCmd::SendRequest {
                request,
                response_tx: resp_tx,
            })
            .await
            .context("Failed to send request")?;
        } // Lock released here

        // Wait indefinitely for response (session/prompt can take very long)
        let resp = resp_rx
            .recv()
            .await
            .context("Channel closed - subprocess died")?;

        tracing::debug!(method, id, response = ?resp, "ACP received response");
        resp
    }

    /// Send a notification (no response expected)
    async fn send_notification(&self, method: &str, params: serde_json::Value) -> Result<()> {
        let notification = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }))?;

        // Send as request but don't wait for response
        let (resp_tx, _) = mpsc::channel(1);

        // Send request while holding the lock, then release immediately
        {
            let guard = self.cmd_tx.lock().await;
            let tx = guard.as_ref().context("Subprocess task died")?;
            tx.send(SubprocessCmd::SendRequest {
                request: notification,
                response_tx: resp_tx,
            })
            .await
            .context("Failed to send notification")?;
        } // Lock released here

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Subprocess Handler
// ---------------------------------------------------------------------------

/// Subprocess handler task
async fn run_subprocess(
    command: &str,
    args: &[String],
    mut cmd_rx: mpsc::Receiver<SubprocessCmd>,
    collected_content: Arc<Mutex<String>>,
    handler: Arc<dyn AcpCallbackHandler>,
    event_tx: broadcast::Sender<SessionEvent>,
    notification_manager: Arc<Mutex<NotificationManager>>,
) -> Result<()> {
    let mut child = Command::new(command)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("Failed to spawn ACP subprocess: {} {:?}", command, args))?;

    let mut stdin = child.stdin.take().context("Failed to get stdin")?;
    let stdout = child.stdout.take().context("Failed to get stdout")?;
    let stderr = child.stderr.take().context("Failed to get stderr")?;
    let mut reader = BufReader::new(stdout);
    let mut stderr_reader = BufReader::new(stderr);

    tracing::info!("ACP subprocess started: {} {:?}", command, args);

    // Spawn a task to continuously read stderr
    let stderr_task = tokio::spawn(async move {
        let mut stderr_buf = Vec::new();
        loop {
            stderr_buf.clear();
            match stderr_reader.read_until(b'\n', &mut stderr_buf).await {
                Ok(0) => {
                    tracing::debug!("ACP stderr EOF");
                    break;
                }
                Ok(_) => {
                    let stderr_line = String::from_utf8_lossy(&stderr_buf).trim().to_string();
                    if !stderr_line.is_empty() {
                        tracing::warn!("ACP stderr: {}", stderr_line);
                    }
                }
                Err(e) => {
                    tracing::error!("ACP stderr read error: {}", e);
                    break;
                }
            }
        }
    });

    loop {
        tokio::select! {
            // Handle commands from AcpClient
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(SubprocessCmd::SendRequest { request, response_tx }) => {
                        tracing::info!("ACP subprocess: received request from client");
                        collected_content.lock().await.clear();

                        let request_id = serde_json::from_str::<serde_json::Value>(&request)
                            .ok()
                            .and_then(|v| v.get("id").and_then(|i| i.as_i64()));

                        tracing::info!(
                            request_id,
                            request_preview = &request[..request.len().min(200)],
                            "ACP subprocess: writing request to stdin"
                        );

                        // Single write for JSON + newline
                        let mut combined = request.as_bytes().to_vec();
                        combined.push(b'\n');
                        if stdin.write_all(&combined).await.is_err() {
                            tracing::error!("ACP subprocess: stdin write error");
                            let _ = response_tx.send(Err(anyhow::anyhow!("Write error"))).await;
                            break;
                        }
                        if stdin.flush().await.is_err() {
                            tracing::error!("ACP subprocess: stdin flush error");
                            let _ = response_tx.send(Err(anyhow::anyhow!("Flush error"))).await;
                            break;
                        }

                        tracing::info!("ACP subprocess: request written to stdin, waiting for response");

                        // Read response and notifications until we get the matching response
                        let mut line_buf = Vec::new(); // Use byte buffer to handle non-UTF8

                        // Read until we get the response for this request (no timeout - can take very long!)
                        loop {
                            line_buf.clear();
                            tokio::select! {
                                result = reader.read_until(b'\n', &mut line_buf) => {
                                    match result {
                                        Ok(0) => {
                                            tracing::error!("ACP subprocess: EOF received");
                                            let _ = response_tx.send(Err(anyhow::anyhow!("EOF"))).await;
                                            break;
                                        }
                                        Ok(_) => {
                                            // Convert to string, replacing invalid UTF-8 sequences
                                            let line = String::from_utf8_lossy(&line_buf).trim().to_string();
                                            if line.is_empty() {
                                                continue;
                                            }

                                            if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) {
                                                let method_field = msg.get("method").and_then(|m| m.as_str());
                                                let resp_id = msg.get("id").and_then(|i| i.as_i64());

                                                // Log all incoming messages
                                                if let Some(method) = method_field {
                                                    tracing::info!("ACP subprocess: received notification method={} id={:?}", method, resp_id);
                                                } else if resp_id.is_some() {
                                                    tracing::info!("ACP subprocess: received response id={:?} preview={}", resp_id, &line[..line.len().min(200)]);
                                                } else {
                                                    tracing::debug!("ACP subprocess: received message: {}", line);
                                                }

                                                // Handle session/update notification
                                                if method_field == Some(methods::SESSION_UPDATE) {
                                                    handle_session_update(&msg, &collected_content, &event_tx, &notification_manager).await;
                                                    continue;
                                                }

                                                // Handle Agent → Client requests
                                                if let Some(method) = method_field {
                                                    tracing::info!("ACP subprocess: handling agent request method={}", method);
                                                    if handle_agent_request(&mut stdin, &msg, method, &handler).await {
                                                        continue;
                                                    }
                                                }

                                                // Check if this is the response to our request
                                                if resp_id == request_id {
                                                    tracing::info!("ACP subprocess: matched response id={} to request_id={}", resp_id.unwrap_or(-1), request_id.unwrap_or(-1));
                                                    let _ = response_tx.send(Ok(msg)).await;
                                                    tracing::info!("ACP subprocess: response sent to client");
                                                    break;
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            tracing::error!("ACP subprocess: JSON parse error: {}", e);
                                            let _ = response_tx.send(Err(anyhow::anyhow!("{}", e))).await;
                                            break;
                                        }
                                    }
                                }
                                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                                    // Continue polling
                                }
                            }
                        }
                    }
                    Some(SubprocessCmd::Shutdown) => {
                        tracing::info!("ACP subprocess shutting down");
                        break;
                    }
                    None => {
                        tracing::warn!("ACP subprocess: cmd channel closed");
                        break;
                    }
                }
            }
            // Prevent tight loop
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
    }

    Ok(())
}

/// Handle session/update notification
async fn handle_session_update(
    msg: &serde_json::Value,
    collected_content: &Arc<Mutex<String>>,
    event_tx: &broadcast::Sender<SessionEvent>,
    notification_manager: &Arc<Mutex<NotificationManager>>,
) {
    let params = msg.get("params");
    let session_id = params
        .and_then(|p| p.get("sessionId"))
        .and_then(|s| s.as_str())
        .map(String::from);
    let update = params.and_then(|p| p.get("update"));

    if let Some(update) = update {
        let session_update = update.get("sessionUpdate").and_then(|s| s.as_str());

        match session_update {
            Some("plan") => {
                tracing::debug!("ACP plan received");
                if let Some(entries) = update.get("entries").and_then(|e| e.as_array()) {
                    for entry in entries {
                        if let Some(content) = entry.get("content").and_then(|c| c.as_str()) {
                            tracing::debug!("Plan entry: {}", content);
                        }
                    }
                }
            }
            Some("user_message") | Some("user_message_chunk") => {
                tracing::debug!("ACP user_message received");
            }
            Some("agent_message") => {
                if let Some(content) = update.get("content") {
                    extract_text_content(content, collected_content).await;
                }
            }
            Some("agent_message_chunk") => {
                if let Some(content) = update.get("content") {
                    extract_text_content(content, collected_content).await;
                    if let Some(text) = content.get("text").and_then(|t| t.as_str()) {
                        let _ = event_tx.send(SessionEvent::AgentMessageChunk {
                            content: text.to_string(),
                        });
                    }
                }
            }
            Some("agent_thought_chunk") => {
                if let Some(content) = update.get("content") {
                    if let Some(text) = content.get("text").and_then(|t| t.as_str()) {
                        tracing::debug!("ACP thought: {}", text);
                        let _ = event_tx.send(SessionEvent::AgentThoughtChunk {
                            content: text.to_string(),
                        });
                    }
                }
            }
            Some("tool_call") => {
                let tool_call_id = update
                    .get("toolCallId")
                    .and_then(|t| t.as_str())
                    .unwrap_or("?")
                    .to_string();
                let title = update
                    .get("title")
                    .and_then(|t| t.as_str())
                    .map(String::from);
                let kind = parse_tool_kind(update.get("kind").and_then(|k| k.as_str()));
                let status = update.get("status").and_then(|s| s.as_str());

                tracing::debug!(
                    "ACP tool_call: {} - {:?} ({:?})",
                    tool_call_id,
                    title,
                    status
                );

                match status {
                    Some("pending") => {
                        let _ = event_tx.send(SessionEvent::ToolCallStarted {
                            tool_call_id: tool_call_id.clone(),
                            title: title.clone(),
                            kind: kind.clone(),
                        });
                        let _lang = crate::i18n::default_lang();
                        let notif = Notification::new(
                            NotificationPriority::Medium,
                            &crate::i18n::t("acp_tool_start", _lang),
                            &crate::i18n::t_fmt("acp_tool_executing", _lang, &[("title", title.as_deref().unwrap_or(""))]),
                        );
                        if let Ok(nm) = notification_manager.try_lock() {
                            nm.send(&notif.with_session_id(session_id.clone().unwrap_or_default()))
                                .await;
                        }
                    }
                    Some("in_progress") => {
                        let _ = event_tx.send(SessionEvent::ToolCallInProgress { tool_call_id });
                    }
                    Some("completed") => {
                        let result = update
                            .get("result")
                            .and_then(|r| r.get("text"))
                            .and_then(|t| t.as_str())
                            .map(String::from);
                        let _ = event_tx.send(SessionEvent::ToolCallCompleted {
                            tool_call_id: tool_call_id.clone(),
                            result: result.clone(),
                        });
                        let _lang = crate::i18n::default_lang();
                        let notif = Notification::new(
                            NotificationPriority::Medium,
                            &crate::i18n::t("acp_tool_done", _lang),
                            &crate::i18n::t_fmt("acp_tool_completed", _lang, &[("title", title.as_deref().unwrap_or(""))]),
                        );
                        if let Ok(nm) = notification_manager.try_lock() {
                            nm.send(&notif.with_session_id(session_id.clone().unwrap_or_default()))
                                .await;
                        }
                    }
                    Some("failed") => {
                        let error = update
                            .get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("Unknown error")
                            .to_string();
                        let _ = event_tx.send(SessionEvent::ToolCallFailed {
                            tool_call_id: tool_call_id.clone(),
                            error: error.clone(),
                        });
                        let _lang = crate::i18n::default_lang();
                        let notif = Notification::new(
                            NotificationPriority::High,
                            &crate::i18n::t("acp_tool_failed", _lang),
                            &crate::i18n::t_fmt("acp_tool_error", _lang, &[
                                ("title", title.as_deref().unwrap_or("")),
                                ("error", &error),
                            ]),
                        )
                        .with_burn_after_read();
                        if let Ok(nm) = notification_manager.try_lock() {
                            nm.send(&notif.with_session_id(session_id.clone().unwrap_or_default()))
                                .await;
                        }
                    }
                    _ => {}
                }
            }
            Some("mode_change") => {
                if let Some(mode_id) = update.get("modeId").and_then(|m| m.as_str()) {
                    tracing::debug!("ACP mode_change: {}", mode_id);
                    let _ = event_tx.send(SessionEvent::ModeChanged {
                        mode_id: mode_id.to_string(),
                    });
                }
            }
            Some("config_option_update") => {
                if let Some(options) = update.get("configOptions").and_then(|o| o.as_array()) {
                    let config_options: Vec<SessionConfigOption> = options
                        .iter()
                        .filter_map(|v| serde_json::from_value(v.clone()).ok())
                        .collect();
                    let _ = event_tx.send(SessionEvent::ConfigOptionUpdated {
                        options: config_options,
                    });
                }
            }
            Some("session_info_update") => {
                let title = update
                    .get("title")
                    .and_then(|t| t.as_str())
                    .map(String::from);
                let updated_at = update
                    .get("updatedAt")
                    .and_then(|t| t.as_str())
                    .map(String::from);
                let _ = event_tx.send(SessionEvent::SessionInfoUpdated {
                    title: title.clone(),
                    updated_at: updated_at.clone(),
                });
                let _lang = crate::i18n::default_lang();
                let notif = Notification::new(
                    NotificationPriority::High,
                    &crate::i18n::t("acp_session_created", _lang),
                    &crate::i18n::t_fmt("acp_session_info", _lang, &[
                        ("id", session_id.as_deref().unwrap_or("")),
                        ("title", title.as_deref().unwrap_or("")),
                    ]),
                )
                .with_burn_after_read();
                if let Ok(nm) = notification_manager.try_lock() {
                    nm.send(&notif).await;
                }
            }
            Some("usage_update") => {
                let used = update.get("used").and_then(|u| u.as_u64()).unwrap_or(0) as u32;
                let size = update.get("size").and_then(|s| s.as_u64()).unwrap_or(0) as u32;
                let _ = event_tx.send(SessionEvent::UsageUpdated { used, size });
            }
            Some("available_commands_update") => {
                if let Some(commands) = update.get("availableCommands").and_then(|c| c.as_array()) {
                    let cmds: Vec<AvailableCommand> = commands
                        .iter()
                        .filter_map(|v| serde_json::from_value(v.clone()).ok())
                        .collect();
                    let _ =
                        event_tx.send(SessionEvent::AvailableCommandsUpdated { commands: cmds });
                }
            }
            _ => {
                tracing::debug!("ACP session_update: {:?}", session_update);
            }
        }
    }
}

/// Extract text content from content block
async fn extract_text_content(content: &serde_json::Value, collected_content: &Arc<Mutex<String>>) {
    if let Some(text) = content.get("text").and_then(|t| t.as_str()) {
        collected_content.lock().await.push_str(text);
    }
    if let Some(arr) = content.as_array() {
        for item in arr {
            if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                collected_content.lock().await.push_str(text);
            }
        }
    }
}

/// Parse tool kind from string
fn parse_tool_kind(kind: Option<&str>) -> ToolKind {
    match kind {
        Some("read") => ToolKind::Read,
        Some("edit") => ToolKind::Edit,
        Some("delete") => ToolKind::Delete,
        Some("move") => ToolKind::Move,
        Some("search") => ToolKind::Search,
        Some("execute") => ToolKind::Execute,
        Some("think") => ToolKind::Think,
        Some("fetch") => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

/// Handle Agent → Client request (permissions, fs, terminal)
/// Returns true if handled, false if not an agent request
async fn handle_agent_request(
    stdin: &mut tokio::process::ChildStdin,
    msg: &serde_json::Value,
    method: &str,
    handler: &Arc<dyn AcpCallbackHandler>,
) -> bool {
    let request_id = msg.get("id").and_then(|i| i.as_i64());
    let params = msg
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let session_id = params
        .get("sessionId")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();

    let result: Result<serde_json::Value> = match method {
        // Permission request
        methods::SESSION_REQUEST_PERMISSION => {
            let tool_call_id = params
                .get("toolCall")
                .and_then(|t| t.get("id").and_then(|i| i.as_str()))
                .unwrap_or("");

            let options: Vec<PermissionOption> = params
                .get("options")
                .and_then(|o| o.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| serde_json::from_value(v.clone()).ok())
                        .collect()
                })
                .unwrap_or_default();

            let outcome = handler
                .handle_request_permission(&session_id, tool_call_id, options)
                .await;
            Ok(serde_json::to_value(RequestPermissionResponse { outcome })
                .unwrap_or(serde_json::Value::Null))
        }

        // File system operations
        methods::FS_READ_TEXT_FILE => {
            let path = params.get("path").and_then(|p| p.as_str()).unwrap_or("");
            match handler.handle_read_text_file(&session_id, path).await {
                Ok(contents) => Ok(serde_json::to_value(ReadTextFileResponse { contents })
                    .unwrap_or(serde_json::Value::Null)),
                Err(e) => Err(e),
            }
        }

        methods::FS_WRITE_TEXT_FILE => {
            let path = params.get("path").and_then(|p| p.as_str()).unwrap_or("");
            let contents = params
                .get("contents")
                .and_then(|c| c.as_str())
                .unwrap_or("");
            match handler
                .handle_write_text_file(&session_id, path, contents)
                .await
            {
                Ok(_) => Ok(serde_json::to_value(WriteTextFileResponse {})
                    .unwrap_or(serde_json::Value::Null)),
                Err(e) => Err(e),
            }
        }

        // Terminal operations
        methods::TERMINAL_CREATE => {
            let command = params.get("command").and_then(|c| c.as_str());
            let args = params.get("args").and_then(|a| a.as_array()).map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            });
            match handler
                .handle_terminal_create(&session_id, command, args)
                .await
            {
                Ok(terminal_id) => Ok(serde_json::to_value(CreateTerminalResponse { terminal_id })
                    .unwrap_or(serde_json::Value::Null)),
                Err(e) => Err(e),
            }
        }

        methods::TERMINAL_OUTPUT => {
            let terminal_id = params
                .get("terminalId")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            match handler
                .handle_terminal_output(&session_id, terminal_id)
                .await
            {
                Ok(resp) => Ok(serde_json::to_value(resp).unwrap_or(serde_json::Value::Null)),
                Err(e) => Err(e),
            }
        }

        methods::TERMINAL_KILL => {
            let terminal_id = params
                .get("terminalId")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            match handler.handle_terminal_kill(&session_id, terminal_id).await {
                Ok(_) => Ok(serde_json::to_value(KillTerminalResponse {})
                    .unwrap_or(serde_json::Value::Null)),
                Err(e) => Err(e),
            }
        }

        methods::TERMINAL_RELEASE => {
            let terminal_id = params
                .get("terminalId")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            match handler
                .handle_terminal_release(&session_id, terminal_id)
                .await
            {
                Ok(_) => Ok(serde_json::to_value(ReleaseTerminalResponse {})
                    .unwrap_or(serde_json::Value::Null)),
                Err(e) => Err(e),
            }
        }

        methods::TERMINAL_WAIT_FOR_EXIT => {
            let terminal_id = params
                .get("terminalId")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            match handler
                .handle_terminal_wait_for_exit(&session_id, terminal_id)
                .await
            {
                Ok(exit) => Ok(serde_json::to_value(WaitForTerminalExitResponse { exit })
                    .unwrap_or(serde_json::Value::Null)),
                Err(e) => Err(e),
            }
        }

        _ => return false, // Not an agent request we handle
    };

    // Send response back to agent
    if let Some(id) = request_id {
        let response = match result {
            Ok(value) => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": value
            }),
            Err(e) => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32603, "message": e.to_string() }
            }),
        };

        let response_str = serde_json::to_string(&response).unwrap_or_default();
        tracing::debug!("ACP response to agent: {}", response_str);

        let mut combined = response_str.as_bytes().to_vec();
        combined.push(b'\n');

        // Use blocking write since we're in async context
        use tokio::io::AsyncWriteExt;
        if let Err(e) = stdin.write_all(&combined).await {
            tracing::error!("ACP response write failed: {}", e);
        }
        if let Err(e) = stdin.flush().await {
            tracing::error!("ACP response flush failed: {}", e);
        }

        tracing::debug!("ACP response sent successfully for method {}", method);
    }

    true
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_handler_permission() {
        let _handler = DefaultAcpHandler;
        let options = [
            PermissionOption {
                option_id: "deny".to_string(),
                kind: PermissionOptionKind::RejectOnce,
                label: None,
            },
            PermissionOption {
                option_id: "allow".to_string(),
                kind: PermissionOptionKind::AllowOnce,
                label: None,
            },
        ];

        // Can't easily test async in unit test, but structure is valid
        assert!(matches!(options[1].kind, PermissionOptionKind::AllowOnce));
    }

    #[test]
    fn test_session_event_variants() {
        let event = SessionEvent::AgentMessageChunk {
            content: "test".to_string(),
        };
        assert!(matches!(event, SessionEvent::AgentMessageChunk { .. }));

        let event = SessionEvent::ToolCallStarted {
            tool_call_id: "call_1".to_string(),
            title: Some("Reading file".to_string()),
            kind: ToolKind::Read,
        };
        assert!(matches!(event, SessionEvent::ToolCallStarted { .. }));
    }
}
