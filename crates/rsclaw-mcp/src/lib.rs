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
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use rsclaw_config::schema::McpServerConfig;
use rsclaw_provider::ToolDef;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, oneshot},
    time,
};
use tracing::{debug, info, warn};

const MCP_CALL_TIMEOUT_SECS: u64 = 60;
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// In-flight requests keyed by JSON-RPC id. The reader task removes the
/// entry and fulfils the sender when the matching response arrives.
type PendingMap = HashMap<u64, oneshot::Sender<std::result::Result<Value, String>>>;
type SharedPending = Arc<std::sync::Mutex<PendingMap>>;

fn lock_pending(pending: &SharedPending) -> std::sync::MutexGuard<'_, PendingMap> {
    // A poisoned map only means another thread panicked while holding it;
    // the map itself is still consistent (plain inserts/removes).
    pending.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// McpClient
// ---------------------------------------------------------------------------

/// Handle to one running MCP server subprocess. Cloning shares the same
/// process, stdin writer and response demultiplexer.
#[derive(Clone)]
pub struct McpClient {
    pub name: String,
    stdin: Arc<Mutex<ChildStdin>>,
    child: Arc<Mutex<Child>>,
    next_id: Arc<AtomicU64>,
    timeout: Duration,
    /// Requests awaiting a response, fulfilled by the stdout reader task.
    pending: SharedPending,
    /// Set by the reader task once stdout closes; new calls fail fast.
    closed: Arc<AtomicBool>,
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
        #[cfg(windows)]
        {
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn MCP server `{}`", config.name))?;

        let stdin = child.stdin.take().context("MCP server stdin")?;
        let stdout = child.stdout.take().context("MCP server stdout")?;

        info!(name = %config.name, command = %config.command, "MCP server process started");

        let stdin = Arc::new(Mutex::new(stdin));
        let pending: SharedPending = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        tokio::spawn(reader_loop(
            config.name.clone(),
            BufReader::new(stdout),
            Arc::clone(&stdin),
            Arc::clone(&pending),
            Arc::clone(&closed),
        ));

        Ok(Self {
            name: config.name.clone(),
            stdin,
            child: Arc::new(Mutex::new(child)),
            next_id: Arc::new(AtomicU64::new(1)),
            timeout: Duration::from_secs(MCP_CALL_TIMEOUT_SECS),
            pending,
            closed,
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
                        "version": option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
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
        if let Err(e) = child.kill().await {
            tracing::debug!(error = %e, "mcp: child process kill failed");
        }
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

        let (tx, rx) = oneshot::channel();
        lock_pending(&self.pending).insert(id, tx);
        // The reader sets `closed` before draining the map, so checking after
        // the insert guarantees we either see the flag or get drained.
        if self.closed.load(Ordering::SeqCst) {
            lock_pending(&self.pending).remove(&id);
            bail!("MCP `{}` stdout closed (server exited?)", self.name);
        }

        if let Err(e) = self.send_line(&serde_json::to_string(&request)?).await {
            lock_pending(&self.pending).remove(&id);
            return Err(e);
        }

        let resp = match time::timeout(self.timeout, rx).await {
            Ok(Ok(Ok(val))) => val,
            Ok(Ok(Err(e))) => bail!("MCP `{}` call `{method}` failed: {e}", self.name),
            Ok(Err(_)) => bail!("MCP `{}` call `{method}` dropped (reader gone)", self.name),
            Err(_) => {
                lock_pending(&self.pending).remove(&id);
                bail!(
                    "MCP `{}` call `{method}` timed out after {}s",
                    self.name,
                    self.timeout.as_secs()
                );
            }
        };

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
        write_line(&self.stdin, line)
            .await
            .with_context(|| format!("write to MCP `{}`", self.name))
    }
}

async fn write_line(stdin: &Mutex<ChildStdin>, line: &str) -> Result<()> {
    let mut stdin = stdin.lock().await;
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Incoming message routing
// ---------------------------------------------------------------------------

/// What the reader task should do with one incoming JSON-RPC message.
#[derive(Debug, PartialEq)]
enum Routed {
    /// A response that matched (and fulfilled) a pending request.
    Delivered,
    /// A response whose id is unknown (e.g. arrived after a timeout).
    Unmatched,
    /// Server-initiated `ping`; reply with an empty result.
    Ping(Value),
    /// Other server-initiated request; reply with "method not found".
    ServerRequest(Value, String),
    /// Notification (no id); nothing to do.
    Notification,
}

/// Classify `msg` and, when it is a response, hand it to the matching
/// pending request. Pure apart from the pending-map mutation, so it can be
/// unit tested without a subprocess.
fn route_message(pending: &SharedPending, msg: Value) -> Routed {
    let method = msg.get("method").and_then(|m| m.as_str()).map(str::to_owned);
    let id = msg.get("id").filter(|v| !v.is_null()).cloned();
    match (method, id) {
        (Some(m), Some(id)) if m == "ping" => Routed::Ping(id),
        (Some(m), Some(id)) => Routed::ServerRequest(id, m),
        (Some(_), None) => Routed::Notification,
        (None, Some(id)) => {
            let key = id
                .as_u64()
                .or_else(|| id.as_str().and_then(|s| s.parse::<u64>().ok()));
            let sender = key.and_then(|k| lock_pending(pending).remove(&k));
            match sender {
                Some(tx) => {
                    if tx.send(Ok(msg)).is_err() {
                        debug!("MCP response receiver already dropped");
                    }
                    Routed::Delivered
                }
                None => Routed::Unmatched,
            }
        }
        (None, None) => Routed::Notification,
    }
}

/// Dedicated stdout reader: demultiplexes responses to pending requests and
/// answers server-initiated requests. On exit every pending call is failed.
async fn reader_loop(
    name: String,
    mut stdout: BufReader<ChildStdout>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: SharedPending,
    closed: Arc<AtomicBool>,
) {
    let mut line = String::new();
    loop {
        line.clear();
        match stdout.read_line(&mut line).await {
            Ok(0) => {
                debug!(name = %name, "MCP stdout closed (EOF)");
                break;
            }
            Ok(_) => {}
            Err(e) => {
                warn!(name = %name, "MCP stdout read error: {e:#}");
                break;
            }
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        let val: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                debug!(name = %name, "MCP non-JSON line ignored ({e}): {}", rsclaw_util::truncate_str(trimmed, 200));
                continue;
            }
        };
        let reply = match route_message(&pending, val) {
            Routed::Delivered => None,
            Routed::Unmatched => {
                warn!(name = %name, "MCP response with unknown id dropped (late after timeout?): {}", rsclaw_util::truncate_str(trimmed, 200));
                None
            }
            Routed::Notification => {
                debug!(name = %name, "MCP notification (ignored): {}", rsclaw_util::truncate_str(trimmed, 200));
                None
            }
            Routed::Ping(id) => Some(json!({"jsonrpc": "2.0", "id": id, "result": {}})),
            Routed::ServerRequest(id, method) => {
                debug!(name = %name, method = %method, "MCP server request not supported");
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": format!("method not found: {method}")},
                }))
            }
        };
        if let Some(reply) = reply {
            let text = reply.to_string();
            if let Err(e) = write_line(&stdin, &text).await {
                warn!(name = %name, "MCP reply to server request failed: {e:#}");
            }
        }
    }
    closed.store(true, Ordering::SeqCst);
    let drained: Vec<_> = lock_pending(&pending).drain().collect();
    for (_, tx) in drained {
        if tx.send(Err("server stdout closed (server exited?)".to_owned())).is_err() {
            debug!(name = %name, "MCP pending receiver already dropped");
        }
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
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// Register (or replace) a client under its server name.
    pub async fn register(&self, client: Arc<McpClient>) {
        self.clients
            .lock()
            .await
            .insert(client.name.clone(), client);
    }

    /// Find the MCP client that owns a given tool name (prefixed with
    /// `mcp_<server>_`).
    ///
    /// When several server names match (e.g. `github` and
    /// `github_enterprise`), the longest one wins so routing is deterministic.
    pub async fn find_for_tool(&self, tool_name: &str) -> Option<Arc<McpClient>> {
        let clients = self.clients.lock().await;
        let names: Vec<&String> = clients.keys().collect();
        let best = longest_matching_server(&names, tool_name)?;
        clients.get(best).map(Arc::clone)
    }

    /// Get all tool defs from all registered MCP servers.
    ///
    /// Iteration order is canonicalized by MCP server name (the HashMap
    /// key). Without sorting, `HashMap::values()` returns servers in a
    /// non-deterministic order which leaks into the rsclaw provider's
    /// `dynamic_prefix.tools` payload — the worker hashes those bytes
    /// to form its prefix cache key, so any reordering would force a
    /// fresh decode on every gateway restart even when the MCP server
    /// list is logically identical.
    pub async fn all_tool_defs(&self) -> Vec<ToolDef> {
        let clients = self.clients.lock().await;
        let mut keyed: Vec<(&String, &Arc<McpClient>)> = clients.iter().collect();
        keyed.sort_by(|a, b| a.0.cmp(b.0));
        let mut defs = Vec::new();
        for (_, client) in keyed {
            defs.extend(client.as_tool_defs());
        }
        defs
    }
}

/// Pick the server whose `mcp_<server>_` prefix matches `tool_name`,
/// preferring the longest server name.
fn longest_matching_server<'a>(names: &[&'a String], tool_name: &str) -> Option<&'a String> {
    names
        .iter()
        .filter(|n| {
            tool_name
                .strip_prefix("mcp_")
                .and_then(|rest| rest.strip_prefix(n.as_str()))
                .is_some_and(|rest| rest.starts_with('_'))
        })
        .max_by_key(|n| n.len())
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_route_message_matches_ids() {
        let pending: SharedPending = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (tx1, mut rx1) = oneshot::channel();
        let (tx2, mut rx2) = oneshot::channel();
        lock_pending(&pending).insert(1, tx1);
        lock_pending(&pending).insert(2, tx2);

        // Response for id 2 goes to the id-2 waiter only.
        let r = route_message(&pending, json!({"jsonrpc":"2.0","id":2,"result":{"v":2}}));
        assert_eq!(r, Routed::Delivered);
        let got = rx2.try_recv().expect("id 2 delivered").expect("ok");
        assert_eq!(got["result"]["v"], 2);
        assert!(rx1.try_recv().is_err());

        // Late response for an unknown id is not delivered to anyone.
        let r = route_message(&pending, json!({"jsonrpc":"2.0","id":99,"result":{}}));
        assert_eq!(r, Routed::Unmatched);
        assert!(rx1.try_recv().is_err());

        // Server ping / other requests / notifications are not responses.
        let r = route_message(&pending, json!({"jsonrpc":"2.0","id":1,"method":"ping"}));
        assert_eq!(r, Routed::Ping(json!(1)));
        let r = route_message(&pending, json!({"jsonrpc":"2.0","id":"x","method":"roots/list"}));
        assert_eq!(r, Routed::ServerRequest(json!("x"), "roots/list".to_owned()));
        let r = route_message(&pending, json!({"jsonrpc":"2.0","method":"notifications/progress"}));
        assert_eq!(r, Routed::Notification);
        assert!(rx1.try_recv().is_err(), "id 1 still pending after ping");
        assert_eq!(lock_pending(&pending).len(), 1);
    }

    #[test]
    fn mcp_longest_prefix_routing() {
        let a = "github".to_owned();
        let b = "github_enterprise".to_owned();
        let names = vec![&a, &b];
        assert_eq!(longest_matching_server(&names, "mcp_github_enterprise_search"), Some(&b));
        assert_eq!(longest_matching_server(&names, "mcp_github_search"), Some(&a));
        assert_eq!(longest_matching_server(&names, "mcp_gitlab_search"), None);
    }
}
