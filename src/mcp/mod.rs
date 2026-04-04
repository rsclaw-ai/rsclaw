//! MCP (Model Context Protocol) client — communicates with MCP servers
//! over stdin/stdout JSON-RPC to discover and invoke tools.
//!
//! Lifecycle:
//!   1. `McpClient::spawn()` — start the server subprocess
//!   2. `initialize()`       — MCP handshake (negotiate capabilities)
//!   3. `list_tools()`       — discover available tools
//!   4. `call_tool(name, args)` — invoke a tool
//!
//! MCP spec: https://spec.modelcontextprotocol.io/

use std::{
    collections::HashMap,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
    time,
};
use tracing::{debug, info};

use crate::{config::schema::McpServerConfig, provider::ToolDef};

const MCP_CALL_TIMEOUT_SECS: u64 = 60;
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

// ---------------------------------------------------------------------------
// McpClient
// ---------------------------------------------------------------------------

pub struct McpClient {
    pub name: String,
    stdin: Arc<Mutex<ChildStdin>>,
    stdout: Arc<Mutex<BufReader<ChildStdout>>>,
    child: Arc<Mutex<Child>>,
    next_id: Arc<AtomicU64>,
    timeout: Duration,
    /// Tools discovered via `tools/list`.
    pub tools: Vec<McpTool>,
}

/// A tool definition as returned by the MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input_schema: Value,
}

impl McpClient {
    /// Spawn an MCP server subprocess from config.
    pub async fn spawn(config: &McpServerConfig) -> Result<Self> {
        let mut cmd = Command::new(&config.command);
        if let Some(args) = &config.args {
            cmd.args(args);
        }
        if let Some(env) = &config.env {
            for (k, v) in env {
                cmd.env(k, v);
            }
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn MCP server `{}`", config.name))?;

        let stdin = child.stdin.take().context("MCP server stdin")?;
        let stdout = child.stdout.take().context("MCP server stdout")?;

        info!(name = %config.name, command = %config.command, "MCP server process started");

        Ok(Self {
            name: config.name.clone(),
            stdin: Arc::new(Mutex::new(stdin)),
            stdout: Arc::new(Mutex::new(BufReader::new(stdout))),
            child: Arc::new(Mutex::new(child)),
            next_id: Arc::new(AtomicU64::new(1)),
            timeout: Duration::from_secs(MCP_CALL_TIMEOUT_SECS),
            tools: Vec::new(),
        })
    }

    /// Send the MCP `initialize` handshake.
    pub async fn initialize(&self) -> Result<Value> {
        let result = self
            .rpc_call(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "rsclaw",
                        "version": env!("RSCLAW_BUILD_VERSION")
                    }
                }),
            )
            .await?;

        // Send `initialized` notification (no id, no response expected).
        self.rpc_notify("notifications/initialized", json!({}))
            .await?;

        info!(name = %self.name, "MCP server initialized");
        Ok(result)
    }

    /// Discover tools via `tools/list`.
    pub async fn list_tools(&mut self) -> Result<Vec<McpTool>> {
        let result = self.rpc_call("tools/list", json!({})).await?;

        let tools: Vec<McpTool> = result
            .get("tools")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        info!(name = %self.name, count = tools.len(), "MCP tools discovered");
        for t in &tools {
            debug!(server = %self.name, tool = %t.name, "  tool: {}", t.description);
        }

        self.tools = tools.clone();
        Ok(tools)
    }

    /// Invoke a tool via `tools/call`.
    pub async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<Value> {
        let result = self
            .rpc_call(
                "tools/call",
                json!({
                    "name": tool_name,
                    "arguments": arguments
                }),
            )
            .await?;

        Ok(result)
    }

    /// Convert discovered MCP tools to rsclaw `ToolDef` format for agent
    /// registration.
    pub fn as_tool_defs(&self) -> Vec<ToolDef> {
        self.tools
            .iter()
            .map(|t| ToolDef {
                name: format!("mcp_{}_{}", self.name, t.name),
                description: format!("[MCP:{}] {}", self.name, t.description),
                parameters: t.input_schema.clone(),
            })
            .collect()
    }

    /// Shutdown the MCP server.
    pub async fn shutdown(&self) {
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
        info!(name = %self.name, "MCP server stopped");
    }

    // -----------------------------------------------------------------------
    // JSON-RPC helpers
    // -----------------------------------------------------------------------

    async fn rpc_call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        self.send_line(&serde_json::to_string(&request)?).await?;

        // Read response lines, skipping notifications (no "id" field).
        let resp = time::timeout(self.timeout, async {
            loop {
                let line = self.read_line().await?;
                let val: Value = serde_json::from_str(&line)
                    .with_context(|| format!("MCP `{}` invalid JSON: {line}", self.name))?;
                // Skip notifications (messages without "id").
                if val.get("id").is_some() {
                    return Ok::<Value, anyhow::Error>(val);
                }
                debug!(name = %self.name, "MCP notification (skipped): {}", &line[..line.len().min(200)]);
            }
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "MCP `{}` call `{method}` timed out after {}s",
                self.name,
                self.timeout.as_secs()
            )
        })??;

        if let Some(err) = resp.get("error") {
            bail!("MCP `{}` error: {err}", self.name);
        }

        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn rpc_notify(&self, method: &str, params: Value) -> Result<()> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.send_line(&serde_json::to_string(&notification)?).await
    }

    async fn send_line(&self, line: &str) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .with_context(|| format!("write to MCP `{}`", self.name))?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn read_line(&self) -> Result<String> {
        let mut stdout = self.stdout.lock().await;
        let mut line = String::new();
        stdout
            .read_line(&mut line)
            .await
            .with_context(|| format!("read from MCP `{}`", self.name))?;
        if line.is_empty() {
            bail!("MCP `{}` stdout closed (server exited?)", self.name);
        }
        Ok(line.trim_end().to_owned())
    }
}

// ---------------------------------------------------------------------------
// MCP registry — holds all active MCP clients
// ---------------------------------------------------------------------------

/// Holds all active MCP server clients, keyed by server name.
pub struct McpRegistry {
    pub clients: Mutex<HashMap<String, Arc<McpClient>>>,
}

impl Default for McpRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl McpRegistry {
    pub fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
        }
    }

    pub async fn register(&self, client: Arc<McpClient>) {
        self.clients
            .lock()
            .await
            .insert(client.name.clone(), client);
    }

    /// Find the MCP client that owns a given tool name (prefixed with
    /// `mcp_<server>_`).
    pub async fn find_for_tool(&self, tool_name: &str) -> Option<Arc<McpClient>> {
        let clients = self.clients.lock().await;
        for (server_name, client) in clients.iter() {
            let prefix = format!("mcp_{}_", server_name);
            if tool_name.starts_with(&prefix) {
                return Some(Arc::clone(client));
            }
        }
        None
    }

    /// Get all tool defs from all registered MCP servers.
    pub async fn all_tool_defs(&self) -> Vec<ToolDef> {
        let clients = self.clients.lock().await;
        let mut defs = Vec::new();
        for client in clients.values() {
            defs.extend(client.as_tool_defs());
        }
        defs
    }
}
