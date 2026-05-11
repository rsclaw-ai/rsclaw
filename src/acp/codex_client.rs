//! Codex ACP client - wraps codex-acp adapter for ACP protocol.
//!
//! Codex CLI now supports ACP protocol via @agentclientprotocol/codex-acp.
//! This client uses the standard ACP interface, unified with OpenCode/ClaudeCode.
//!
//! Installation: npm install -g @agentclientprotocol/codex-acp

use std::sync::Arc;

use anyhow::{Result, bail};
use tokio::sync::Mutex;

use crate::acp::client::AcpClient;
use crate::acp::notification::{NotificationManager, NotificationSink};

/// Codex ACP client - wraps `codex-acp` for coding task execution.
#[derive(Clone)]
pub struct CodexClient {
    client: AcpClient,
}

/// Result from a Codex execution.
pub struct CodexResult {
    /// Output text from Codex
    pub content: String,
    /// Stop reason
    pub stop_reason: crate::acp::types::StopReason,
}

impl CodexClient {
    /// Spawn a Codex ACP adapter subprocess and initialize it.
    pub async fn spawn(cwd: &str, command: Option<&str>, model: Option<&str>) -> Result<Self> {
        // Find codex-acp executable
        let cmd = command.unwrap_or("codex-acp");

        // Check if codex-acp is available
        let available = if cfg!(target_os = "windows") {
            // On Windows, search for .cmd wrapper
            let path_env = std::env::var("PATH").ok().unwrap_or_default();
            let separator = if path_env.contains(';') { ';' } else { ':' };
            path_env.split(separator).find_map(|dir| {
                let win_dir = if dir.starts_with('/') && dir.len() > 2 {
                    let parts: Vec<&str> = dir.splitn(3, '/').collect();
                    if parts.len() >= 3 && parts[0].is_empty() && parts[1].len() == 1 {
                        let drive = parts[1].to_uppercase();
                        let rest = parts[2];
                        format!("{}:\\{}", drive, rest.replace('/', "\\"))
                    } else {
                        dir.replace('/', "\\")
                    }
                } else {
                    dir.to_string()
                };
                let cmd_path = std::path::PathBuf::from(&win_dir).join("codex-acp.cmd");
                if cmd_path.exists() {
                    return Some(true);
                }
                let bin_path = std::path::PathBuf::from(&win_dir).join("codex-acp");
                if bin_path.exists() {
                    return Some(true);
                }
                None
            }).unwrap_or(false)
        } else {
            which::which(cmd).is_ok()
        };

        if !available {
            bail!(
                "codex-acp not found. Install with: npm install -g @agentclientprotocol/codex-acp\n\
                 Or set the path in agent config: codex.command = \"path/to/codex-acp\""
            );
        }

        tracing::info!(command = %cmd, cwd = %cwd, "Codex ACP: starting subprocess");

        // Spawn ACP client
        let client = AcpClient::spawn_with_timeout(
            cmd,
            &[],  // codex-acp takes no args
            Arc::new(crate::acp::client::DefaultAcpHandler),
            Arc::new(Mutex::new(NotificationManager::new())),
            None,  // use default timeout
            "Codex",
        ).await?;

        // Initialize
        client.initialize("rsclaw", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")).await?;

        // Create session
        let session_resp = client.create_session(cwd, model, None).await?;

        tracing::info!(
            session_id = %session_resp.session_id,
            current_model = ?session_resp.models.as_ref().and_then(|m| m.available_models.first()).map(|m| &m.model_id),
            "Codex ACP session created"
        );

        Ok(Self { client })
    }

    /// Get the underlying ACP client for event subscription.
    pub fn acp_client(&self) -> &AcpClient {
        &self.client
    }

    /// Add a notification sink for progress updates.
    pub fn add_notification_sink(&self, sink: Arc<dyn NotificationSink>) {
        self.client.add_notification_sink(sink);
    }

    /// Subscribe to session events.
    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<crate::acp::client::SessionEvent> {
        self.client.subscribe_events()
    }

    /// Execute a task via Codex ACP prompt.
    pub async fn execute(&self, prompt: &str) -> Result<CodexResult> {
        tracing::info!(prompt_len = prompt.len(), "Codex ACP: sending prompt");

        let resp = self.client.send_prompt(prompt).await?;

        // Get collected content
        let content = self.client.get_collected_content().await;

        tracing::info!(
            stop_reason = ?resp.stop_reason,
            content_len = content.len(),
            "Codex ACP: prompt completed"
        );

        Ok(CodexResult {
            content,
            stop_reason: resp.stop_reason,
        })
    }

    /// Get session ID.
    pub async fn session_id(&self) -> Option<String> {
        self.client.session_id().await
    }

    /// Shutdown the Codex ACP server.
    pub async fn shutdown(&self) {
        let _ = self.client.clone().shutdown().await;
    }
}