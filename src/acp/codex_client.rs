//! Codex client - wraps Codex MCP Server for ACP-like tool invocation
//!
//! Codex CLI (https://github.com/openai/codex) provides an MCP server mode
//! via `codex mcp-server`. This client wraps the MCP protocol to provide
//! a simple interface for executing coding tasks.
//!
//! Codex MCP tools:
//!   - `codex`: Start a new session (prompt, model, cwd, approval-policy, sandbox)
//!   - `codex-reply`: Continue a session (threadId, prompt)

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::config::schema::McpServerConfig;
use crate::mcp::McpClient;

/// Codex MCP client - wraps `codex mcp-server` for coding task execution.
#[derive(Clone)]
pub struct CodexClient {
    mcp_client: Arc<McpClient>,
    cwd: PathBuf,
    /// Optional model override
    model: Option<String>,
    /// Last thread ID for continuation (codex-reply)
    thread_id: Arc<Mutex<Option<String>>>,
}

/// Result from a Codex execution.
pub struct CodexResult {
    /// Thread ID for continuing the session (may be None for one-shot tasks)
    pub thread_id: Option<String>,
    /// Output text from Codex
    pub content: String,
}

impl CodexClient {
    /// Spawn a Codex MCP server subprocess and initialize it.
    pub async fn spawn(cwd: PathBuf, command: Option<&str>, model: Option<&str>) -> Result<Self> {
        let cmd = command.unwrap_or("codex");

        // Check if codex is available
        let available = tokio::process::Command::new(cmd)
            .arg("--version")
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);

        if !available {
            bail!(
                "Codex CLI not found. Install with: npm install -g @openai/codex\n\
                 Or set the path in agent config: codex.command = \"path/to/codex\""
            );
        }

        let config = McpServerConfig {
            name: "codex".to_string(),
            command: cmd.to_string(),
            args: Some(vec!["mcp-server".to_string()]),
            env: None,
        };

        let mut mcp_client = McpClient::spawn(&config).await?;

        // MCP handshake
        let init_result = mcp_client.initialize().await?;
        debug!(init = ?init_result, "Codex MCP initialized");

        // Discover tools
        let tools = mcp_client.list_tools().await?;
        let tool_names: Vec<_> = tools.iter().map(|t| &t.name).collect();
        info!(cwd = %cwd.display(), tools = ?tool_names, "Codex MCP client ready");

        // Verify expected tools exist
        if !tools.iter().any(|t| t.name == "codex") {
            warn!("Codex MCP server missing 'codex' tool - unexpected server behavior");
        }
        if !tools.iter().any(|t| t.name == "codex-reply") {
            warn!("Codex MCP server missing 'codex-reply' tool - session continuation unavailable");
        }

        Ok(Self {
            mcp_client: Arc::new(mcp_client),
            cwd,
            model: model.map(String::from),
            thread_id: Arc::new(Mutex::new(None)),
        })
    }

    /// Execute a task via Codex `codex` tool.
    pub async fn execute(&self, prompt: &str) -> Result<CodexResult> {
        let mut args = json!({
            "prompt": prompt,
            "cwd": self.cwd.to_string_lossy().to_string(),
            "approval-policy": "on-failure",  // Reasonable default
            "sandbox": "workspace-write",      // Allow file writes in workspace
        });

        // Add model override if configured
        if let Some(model) = &self.model {
            args["model"] = json!(model);
        }

        info!(prompt_len = prompt.len(), "Calling Codex MCP tool 'codex'");
        let result = self.mcp_client.call_tool("codex", args).await?;

        // Parse MCP CallToolResult format
        // Codex returns structuredContent with {threadId, content}
        let (thread_id, content) = self.parse_result(&result)?;

        // Store thread_id for potential continuation
        if let Some(tid) = &thread_id {
            *self.thread_id.lock().await = Some(tid.clone());
            info!(thread_id = %tid, "Codex session started");
        }

        Ok(CodexResult { thread_id, content })
    }

    /// Continue a session via `codex-reply` tool.
    pub async fn continue_session(&self, prompt: &str) -> Result<CodexResult> {
        let thread_id_guard = self.thread_id.lock().await;
        let thread_id = thread_id_guard
            .clone()
            .context("No active thread - call execute() first")?;

        let args = json!({
            "threadId": thread_id,
            "prompt": prompt
        });

        info!(thread_id = %thread_id, prompt_len = prompt.len(), "Calling Codex MCP tool 'codex-reply'");
        let result = self.mcp_client.call_tool("codex-reply", args).await?;

        let (new_thread_id, content) = self.parse_result(&result)?;

        // Update stored thread_id
        if let Some(tid) = &new_thread_id {
            *self.thread_id.lock().await = Some(tid.clone());
        }

        Ok(CodexResult {
            thread_id: new_thread_id,
            content,
        })
    }

    /// Parse MCP CallToolResult to extract threadId and content.
    /// Handles both structuredContent (Codex-specific) and standard MCP content array.
    fn parse_result(&self, result: &Value) -> Result<(Option<String>, String)> {
        // Codex MCP returns structuredContent with {threadId, content}
        if let Some(structured) = result.get("structuredContent") {
            let thread_id = structured
                .get("threadId")
                .and_then(|t| t.as_str())
                .map(String::from);

            let content = structured
                .get("content")
                .and_then(|c| c.as_str())
                .map(String::from)
                .unwrap_or_else(|| {
                    structured
                        .get("content")
                        .and_then(|c| c.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default()
                });

            return Ok((thread_id, content));
        }

        // Standard MCP format: {content: [{type: "text", text: "..."}], isError: bool}
        if let Some(content_arr) = result.get("content").and_then(|c| c.as_array()) {
            let text = content_arr
                .iter()
                .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n");

            // Check for error
            if result.get("isError").and_then(|e| e.as_bool()) == Some(true) {
                bail!("Codex error: {}", text);
            }

            return Ok((None, text));
        }

        // Fallback: raw result as string
        warn!(result = ?result, "Unexpected Codex MCP result format");
        Ok((None, result.to_string()))
    }

    /// Shutdown the Codex MCP server.
    pub async fn shutdown(&self) {
        self.mcp_client.shutdown().await;
    }
}