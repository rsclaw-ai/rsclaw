//! Shell Bridge — runs TypeScript/JavaScript plugins as subprocesses
//! communicating via JSON-RPC over stdin/stdout.
//!
//! Protocol:
//!   Request  (host → plugin):
//! `{"id":1,"method":"tool_call","params":{...}}\n`   Response (plugin → host):
//! `{"id":1,"result":{...}}\n`                           or
//! `{"id":1,"error":"message"}\n`
//!
//! Lifecycle:
//!   - `ShellBridgePlugin::spawn()` — start the subprocess
//!   - `call(method, params)`       — send a request, wait for response
//!   - `Drop`                       — kill the subprocess (RAII)
//!
//! Supported runtimes: `node`, `bun`, `deno`.

use std::{
    collections::HashMap,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, oneshot},
    task::JoinHandle,
    time,
};
use tracing::{debug, error, warn};

use super::manifest::PluginManifest;

/// Default per-call timeout in seconds.
const DEFAULT_CALL_TIMEOUT_SECS: u64 = 30;

// ---------------------------------------------------------------------------
// ShellBridgePlugin
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ShellBridgePlugin {
    pub name: String,
    stdin: Arc<Mutex<ChildStdin>>,
    child: Arc<Mutex<Child>>,
    next_id: Arc<AtomicI64>,
    timeout: Duration,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>,
    reader_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    host_dispatch: Arc<crate::plugin::host_methods::HostMethodRegistry>,
}

impl ShellBridgePlugin {
    /// Spawn the plugin subprocess and start the reader task that demuxes
    /// incoming lines into pending-request fulfillment or host method dispatch.
    pub async fn spawn(
        manifest: &PluginManifest,
        host_dispatch: Arc<crate::plugin::host_methods::HostMethodRegistry>,
    ) -> Result<Self> {
        let runtime = resolve_runtime(&manifest.runtime)?;
        let entry = manifest.dir.join(&manifest.entry);

        if !entry.exists() {
            bail!(
                "plugin `{}` entry not found: {}",
                manifest.name,
                entry.display()
            );
        }

        let mut child = Command::new(&runtime)
            .arg(&entry)
            .current_dir(&manifest.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "spawn plugin `{}` with {runtime} {}",
                    manifest.name,
                    entry.display()
                )
            })?;

        let stdin = child.stdin.take().context("plugin stdin")?;
        let stdout = child.stdout.take().context("plugin stdout")?;

        debug!(plugin = %manifest.name, runtime, "plugin subprocess started");

        let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let stdin_arc = Arc::new(Mutex::new(stdin));

        let reader_pending = pending.clone();
        let reader_stdin = stdin_arc.clone();
        let reader_dispatch = host_dispatch.clone();
        let reader_name = manifest.name.clone();
        let reader_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        debug!(plugin = %reader_name, "plugin stdout closed (EOF)");
                        break;
                    }
                    Ok(_) => {
                        if let Err(e) = handle_incoming(
                            line.trim_end_matches('\n'),
                            &reader_pending,
                            reader_stdin.clone(),
                            &reader_dispatch,
                            &reader_name,
                        )
                        .await
                        {
                            warn!(plugin = %reader_name, "incoming dispatch error: {e:#}");
                        }
                    }
                    Err(e) => {
                        error!(plugin = %reader_name, "stdout read error: {e:#}");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            name: manifest.name.clone(),
            stdin: stdin_arc,
            child: Arc::new(Mutex::new(child)),
            next_id: Arc::new(AtomicI64::new(1)),
            timeout: Duration::from_secs(DEFAULT_CALL_TIMEOUT_SECS),
            pending,
            reader_handle: Arc::new(Mutex::new(Some(reader_handle))),
            host_dispatch,
        })
    }

    /// Call a plugin method and return the result.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let request = json!({
            "id":     id,
            "method": method,
            "params": params,
        });

        let line = serde_json::to_string(&request).context("serialize request")?;

        // Send request.
        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(line.as_bytes())
                .await
                .with_context(|| format!("write to plugin `{}`", self.name))?;
            stdin.write_all(b"\n").await.context("write newline")?;
            stdin.flush().await.context("flush stdin")?;
        }

        // Wait for response with timeout.
        let response_line = time::timeout(self.timeout, self.read_line()).await;

        let line = match response_line {
            Ok(Ok(l)) => l,
            Ok(Err(e)) => {
                bail!("plugin `{}` read error: {e:#}", self.name)
            }
            Err(_) => bail!(
                "plugin `{}` call `{method}` timed out after {}s",
                self.name,
                self.timeout.as_secs()
            ),
        };

        let resp: Value = serde_json::from_str(&line)
            .with_context(|| format!("plugin `{}` returned invalid JSON: {line}", self.name))?;

        // Validate ID matches.
        if resp["id"] != id {
            warn!(
                plugin = %self.name,
                expected = id,
                got = ?resp["id"],
                "response ID mismatch"
            );
        }

        if let Some(err) = resp.get("error") {
            bail!("plugin `{}` error: {err}", self.name);
        }

        Ok(resp["result"].clone())
    }

    /// Kill the subprocess gracefully.
    pub async fn shutdown(&self) {
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
        debug!(plugin = %self.name, "plugin subprocess terminated");
    }

    async fn read_line(&self) -> Result<String> {
        let mut line = String::new();
        let mut stdout = self.stdout.lock().await;
        stdout
            .read_line(&mut line)
            .await
            .with_context(|| format!("read from plugin `{}`", self.name))?;
        Ok(line.trim_end_matches('\n').to_owned())
    }
}

// ---------------------------------------------------------------------------
// Runtime resolver
// ---------------------------------------------------------------------------

/// Resolve the JS runtime binary path.
///
/// Priority: ~/.rsclaw/tools/node/ > system PATH.
/// Preference order: `bun` > `node` > `deno` (if the manifest doesn't specify).
fn resolve_runtime(runtime: &str) -> Result<String> {
    let candidates = match runtime {
        "bun" => vec!["bun"],
        "deno" => vec!["deno"],
        "node" => vec!["node"],
        other => vec![other],
    };

    // 1. Check ~/.rsclaw/tools/node/ first
    let tools_dir = crate::config::loader::base_dir().join("tools/node/bin");
    if tools_dir.exists() {
        for candidate in &candidates {
            let bin = tools_dir.join(candidate);
            if bin.exists() {
                return Ok(bin.to_string_lossy().to_string());
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        let tools_dir_win = crate::config::loader::base_dir().join("tools/node");
        if tools_dir_win.exists() {
            for candidate in &candidates {
                let bin = tools_dir_win.join(format!("{candidate}.exe"));
                if bin.exists() {
                    return Ok(bin.to_string_lossy().to_string());
                }
            }
        }
    }

    // 2. System PATH
    for candidate in &candidates {
        if which::which(candidate).is_ok() {
            return Ok(candidate.to_string());
        }
    }

    bail!(
        "no suitable JS runtime found for `{runtime}`. \
         Run `rsclaw tools install node`, download from https://gitfast.io, or install node/bun/deno manually."
    )
}

// ---------------------------------------------------------------------------
// Plugin trait adapter
// ---------------------------------------------------------------------------

/// `Plugin` wraps a `ShellBridgePlugin` and implements both `MemorySlot`
/// and a generic `call()` interface used by the hook system.
pub struct Plugin {
    inner: ShellBridgePlugin,
    pub manifest: PluginManifest,
}

impl Plugin {
    pub async fn spawn(
        manifest: PluginManifest,
        host_dispatch: Arc<crate::plugin::host_methods::HostMethodRegistry>,
    ) -> Result<Self> {
        let inner = ShellBridgePlugin::spawn(&manifest, host_dispatch).await?;
        Ok(Self { inner, manifest })
    }

    /// Call any plugin method.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.inner.call(method, params).await
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_node_runtime() {
        // node is almost certainly available; skip if not.
        let res = resolve_runtime("node");
        if which::which("node").is_ok() {
            assert!(res.is_ok());
        }
    }

    #[test]
    fn resolve_unknown_runtime_fails() {
        assert!(resolve_runtime("__nonexistent_runtime_xyz__").is_err());
    }
}
