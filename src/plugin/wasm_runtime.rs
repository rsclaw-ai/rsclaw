//! WASM plugin runtime — loads `.wasm` component-model plugins via wasmtime.
//!
//! Each WASM plugin exports (via WIT `plugin-api` interface):
//!   - `get-manifest() -> string` — returns a JSON-encoded manifest
//!   - `handle-tool(tool-name, args-json) -> result<string, string>` — executes a tool
//!
//! Host functions provided to plugins (via WIT `host-browser` and `host-runtime`):
//!   - 13 browser automation functions (open, snapshot, click, fill, etc.)
//!   - `log`, `sleep`, `read-file`

use std::{
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::debug;
use wasmtime::{
    Engine, Store,
    component::{Component, Linker, bindgen},
};

use crate::browser::BrowserSession;

// ---------------------------------------------------------------------------
// WIT bindgen — generates host trait and typed export accessors
// ---------------------------------------------------------------------------

bindgen!({
    path: "src/plugin/wit/world.wit",
    async: true,
    trappable_imports: true,
});

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A loaded WASM plugin, ready to dispatch tool calls.
pub struct WasmPlugin {
    /// Plugin name (from manifest).
    pub name: String,
    /// Semver version string (from manifest).
    pub version: Option<String>,
    /// Human-readable description (from manifest).
    pub description: Option<String>,
    /// Tools this plugin exposes.
    pub tools: Vec<WasmToolDef>,
    /// Path to the `.wasm` file on disk.
    pub wasm_path: PathBuf,
    /// Wasmtime engine (shared across plugins).
    engine: Engine,
    /// Compiled component (component model, not core module).
    component: Component,
    /// Pre-linked instance for fast re-instantiation.
    linker: Linker<HostState>,
    /// Reference to the browser session for host function callbacks.
    browser: Arc<Mutex<Option<BrowserSession>>>,
}

/// A tool definition extracted from a WASM plugin's manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmToolDef {
    /// Tool name (unique within the plugin).
    pub name: String,
    /// Human-readable description of what the tool does.
    pub description: String,
    /// JSON Schema for the tool's input parameters.
    pub parameters: serde_json::Value,
}

/// Raw manifest returned by `get_manifest()` from the WASM module.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WasmManifestRaw {
    name: String,
    version: Option<String>,
    description: Option<String>,
    #[serde(default)]
    tools: Vec<WasmToolDef>,
}

/// State passed into the wasmtime `Store`, available to host functions.
struct HostState {
    browser: Arc<Mutex<Option<BrowserSession>>>,
    wasi: wasmtime_wasi::WasiCtx,
    wasi_table: wasmtime::component::ResourceTable,
}

impl wasmtime_wasi::WasiView for HostState {
    fn ctx(&mut self) -> &mut wasmtime_wasi::WasiCtx {
        &mut self.wasi
    }
    fn table(&mut self) -> &mut wasmtime::component::ResourceTable {
        &mut self.wasi_table
    }
}

// ---------------------------------------------------------------------------
// Host trait implementations
// ---------------------------------------------------------------------------

impl rsclaw::jimeng::host_browser::Host for HostState {
    async fn browser_open(&mut self, url: String) -> Result<Result<String, String>> {
        Ok(self.browser_action("open", json!({"url": url})).await)
    }

    async fn browser_snapshot(&mut self) -> Result<Result<String, String>> {
        Ok(self.browser_action("snapshot", json!({})).await)
    }

    async fn browser_click(&mut self, ref_str: String) -> Result<Result<String, String>> {
        Ok(self.browser_action("click", json!({"ref": ref_str})).await)
    }

    async fn browser_click_at(&mut self, x: u32, y: u32) -> Result<Result<String, String>> {
        Ok(self.browser_action("click_at", json!({"x": x, "y": y})).await)
    }

    async fn browser_fill(
        &mut self,
        ref_str: String,
        text: String,
    ) -> Result<Result<String, String>> {
        Ok(self.browser_action("fill", json!({"ref": ref_str, "text": text})).await)
    }

    async fn browser_press(&mut self, key: String) -> Result<Result<String, String>> {
        Ok(self.browser_action("press", json!({"key": key})).await)
    }

    async fn browser_scroll(
        &mut self,
        direction: String,
        amount: u32,
    ) -> Result<Result<String, String>> {
        Ok(self
            .browser_action("scroll", json!({"direction": direction, "amount": amount}))
            .await)
    }

    async fn browser_eval(&mut self, code: String) -> Result<Result<String, String>> {
        // Special command: switch to the newest/last tab
        if code == "__switch_latest_tab" {
            let mut guard = self.browser.lock().await;
            if guard.is_none() {
                return Ok(Err("browser not initialized".to_string()));
            }
            let session = guard.as_mut().unwrap();
            // list_tabs returns {"action":"list_tabs","tabs":[{"id":"...","url":"..."},...]}
            match session.execute("list_tabs", &json!({})).await {
                Ok(val) => {
                    if let Some(tabs) = val.get("tabs").and_then(|t| t.as_array()) {
                        tracing::info!("list_tabs: {} tab(s)", tabs.len());
                        if let Some(last_tab) = tabs.last() {
                            if let Some(tid) = last_tab.get("id").and_then(|t| t.as_str()) {
                                let url = last_tab.get("url").and_then(|u| u.as_str()).unwrap_or("?");
                                tracing::info!("switching to tab: {} url={}", tid, &url[..url.len().min(80)]);
                                match session.execute("switch_tab", &json!({"target_id": tid})).await {
                                    Ok(_) => return Ok(Ok(format!("switched to tab: {}", url))),
                                    Err(e) => return Ok(Err(format!("switch_tab failed: {e:#}"))),
                                }
                            }
                        }
                    }
                    return Ok(Err("no tabs found in list".to_string()));
                }
                Err(e) => return Ok(Err(format!("list_tabs failed: {e:#}"))),
            }
        }
        Ok(self.browser_action("evaluate", json!({"js": code})).await)
    }

    async fn browser_wait_text(
        &mut self,
        text: String,
        timeout_ms: u32,
    ) -> Result<Result<String, String>> {
        Ok(self
            .browser_action("wait", json!({"text": text, "timeout_ms": timeout_ms}))
            .await)
    }

    async fn browser_screenshot(&mut self) -> Result<Result<String, String>> {
        Ok(self.browser_action("screenshot", json!({})).await)
    }

    async fn browser_download(
        &mut self,
        ref_str: String,
        filename: String,
    ) -> Result<Result<String, String>> {
        Ok(self
            .browser_action("download", json!({"ref": ref_str, "path": filename}))
            .await)
    }

    async fn browser_upload(
        &mut self,
        ref_str: String,
        filepath: String,
    ) -> Result<Result<String, String>> {
        Ok(self
            .browser_action("upload", json!({"ref": ref_str, "filepath": filepath}))
            .await)
    }

    async fn browser_get_url(&mut self) -> Result<Result<String, String>> {
        Ok(self.browser_action("get_url", json!({})).await)
    }
}

impl rsclaw::jimeng::host_runtime::Host for HostState {
    async fn log(&mut self, level: String, msg: String) -> Result<()> {
        match level.as_str() {
            "error" => tracing::error!(target: "wasm_plugin", "{msg}"),
            "warn" => tracing::warn!(target: "wasm_plugin", "{msg}"),
            "info" => tracing::info!(target: "wasm_plugin", "{msg}"),
            "debug" => tracing::debug!(target: "wasm_plugin", "{msg}"),
            _ => tracing::trace!(target: "wasm_plugin", "{msg}"),
        }
        Ok(())
    }

    async fn sleep(&mut self, ms: u32) -> Result<()> {
        tokio::time::sleep(std::time::Duration::from_millis(u64::from(ms))).await;
        Ok(())
    }

    async fn read_file(&mut self, path: String) -> Result<Result<String, String>> {
        match tokio::fs::read_to_string(&path).await {
            Ok(contents) => Ok(Ok(contents)),
            Err(e) => Ok(Err(format!("failed to read {path}: {e}"))),
        }
    }
}

impl HostState {
    /// Execute a browser action by locking the shared browser session.
    /// Auto-starts Chrome if no session exists.
    async fn browser_action(&mut self, action: &str, args: Value) -> Result<String, String> {
        let mut guard = self.browser.lock().await;

        // Auto-start browser if not initialized.
        if guard.is_none() {
            tracing::info!("WASM plugin: auto-starting browser session");
            let chrome_path = crate::agent::platform::detect_chrome()
                .ok_or_else(|| "Chrome not found on this system".to_string())?;
            let session = BrowserSession::start(&chrome_path, true, Some("jimeng"))
                .await
                .map_err(|e| format!("failed to start Chrome: {e:#}"))?;
            *guard = Some(session);
        }

        let session = guard.as_mut().expect("browser session just initialized");
        match session.execute(action, &args).await {
            Ok(val) => {
                // Extract the payload field from action results so WASM plugins
                // get clean data, not the JSON wrapper.
                // snapshot → "text", screenshot → "image", others → full JSON
                for field in &["text", "image", "data", "url", "result"] {
                    if let Some(s) = val.get(field).and_then(|v| v.as_str()) {
                        return Ok(s.to_string());
                    }
                }
                Ok(val.to_string())
            }
            Err(e) => Err(format!("{e:#}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Build a `Linker<HostState>` with all host functions registered.
fn build_linker(engine: &Engine) -> Result<Linker<HostState>> {
    let mut linker = Linker::new(engine);
    // Add WASI interfaces (io, filesystem, etc.) required by wasm32-wasip2 components.
    wasmtime_wasi::add_to_linker_async(&mut linker)?;
    // Add our custom host interfaces.
    rsclaw::jimeng::host_browser::add_to_linker(&mut linker, |state: &mut HostState| state)?;
    rsclaw::jimeng::host_runtime::add_to_linker(&mut linker, |state: &mut HostState| state)?;
    Ok(linker)
}

/// Load a WASM plugin from a `PluginManifest`.
///
/// The manifest's `entry` field points to the `.wasm` file relative to the
/// plugin directory. The WASM component is compiled and its `get-manifest`
/// export is called to discover tools.
pub async fn load_wasm_plugin(
    manifest: &super::manifest::PluginManifest,
    engine: &Engine,
    browser: Arc<Mutex<Option<BrowserSession>>>,
) -> Result<WasmPlugin> {
    let path = manifest.entry_path();
    let wasm_bytes = std::fs::read(&path)
        .with_context(|| format!("failed to read WASM file: {}", path.display()))?;

    let component = Component::new(engine, &wasm_bytes)
        .with_context(|| format!("failed to compile WASM component: {}", path.display()))?;

    let linker = build_linker(engine)?;

    // Create a temporary store to call get-manifest and discover tools.
    let wasi = wasmtime_wasi::WasiCtxBuilder::new().build();
    let mut store = Store::new(
        engine,
        HostState {
            browser: Arc::clone(&browser),
            wasi,
            wasi_table: wasmtime::component::ResourceTable::new(),
        },
    );

    let instance = linker
        .instantiate_async(&mut store, &component)
        .await
        .with_context(|| format!("failed to instantiate component: {}", path.display()))?;

    // Look up the plugin-api interface and call get-manifest.
    let iface_idx = instance
        .get_export(&mut store, None, "rsclaw:jimeng/plugin-api")
        .with_context(|| "plugin-api interface not found in component exports")?;

    let get_manifest_idx = instance
        .get_export(&mut store, Some(&iface_idx), "get-manifest")
        .with_context(|| "get-manifest export not found in plugin-api interface")?;

    let get_manifest_fn = instance
        .get_typed_func::<(), (String,)>(&mut store, &get_manifest_idx)
        .with_context(|| "get-manifest has unexpected type")?;

    let (manifest_json,) = get_manifest_fn
        .call_async(&mut store, ())
        .await
        .with_context(|| "get-manifest call failed")?;

    get_manifest_fn
        .post_return_async(&mut store)
        .await
        .with_context(|| "get-manifest post-return failed")?;

    let wasm_manifest: WasmManifestRaw = serde_json::from_str(&manifest_json)
        .with_context(|| format!("invalid manifest JSON from {}: {manifest_json}", path.display()))?;

    // Prefer plugin.json5 metadata, fall back to wasm-internal manifest.
    Ok(WasmPlugin {
        name: manifest.name.clone(),
        version: manifest.version.clone().or(wasm_manifest.version),
        description: manifest.description.clone().or(wasm_manifest.description),
        tools: if wasm_manifest.tools.is_empty() { Vec::new() } else { wasm_manifest.tools },
        wasm_path: path.to_path_buf(),
        engine: engine.clone(),
        component,
        linker,
        browser,
    })
}

// ---------------------------------------------------------------------------
// Tool dispatch
// ---------------------------------------------------------------------------

impl WasmPlugin {
    /// Dispatch a tool call to this WASM plugin.
    ///
    /// The tool name must match one of the plugin's declared tools.
    /// Arguments are passed as a JSON value and the result is returned
    /// as a JSON value.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value> {
        // Verify the tool exists in this plugin's manifest.
        let _tool_def = self
            .tools
            .iter()
            .find(|t| t.name == tool_name)
            .with_context(|| {
                format!(
                    "tool '{}' not found in WASM plugin '{}'",
                    tool_name, self.name
                )
            })?;

        debug!(plugin = %self.name, tool = tool_name, "dispatching WASM tool call");

        // Fresh store per call for isolation.
        let wasi = wasmtime_wasi::WasiCtxBuilder::new().build();
        let mut store = Store::new(
            &self.engine,
            HostState {
                browser: Arc::clone(&self.browser),
                wasi,
                wasi_table: wasmtime::component::ResourceTable::new(),
            },
        );

        let instance = self
            .linker
            .instantiate_async(&mut store, &self.component)
            .await
            .context("failed to instantiate component for tool call")?;

        // Drill into the plugin-api interface to find handle-tool.
        let iface_idx = instance
            .get_export(&mut store, None, "rsclaw:jimeng/plugin-api")
            .with_context(|| "plugin-api interface not found")?;

        let handle_tool_idx = instance
            .get_export(&mut store, Some(&iface_idx), "handle-tool")
            .with_context(|| "handle-tool export not found")?;

        let handle_tool_fn = instance
            .get_typed_func::<(&str, &str), (Result<String, String>,)>(
                &mut store,
                &handle_tool_idx,
            )
            .with_context(|| "handle-tool has unexpected type")?;

        let args_json = serde_json::to_string(&args)
            .context("failed to serialize tool arguments")?;

        let (result,) = handle_tool_fn
            .call_async(&mut store, (tool_name, &args_json))
            .await
            .with_context(|| format!("handle-tool call failed for '{tool_name}'"))?;

        handle_tool_fn
            .post_return_async(&mut store)
            .await
            .with_context(|| "handle-tool post-return failed")?;

        match result {
            Ok(json_str) => {
                let value: serde_json::Value = serde_json::from_str(&json_str)
                    .with_context(|| {
                        format!("invalid JSON result from tool '{tool_name}': {json_str}")
                    })?;
                Ok(value)
            }
            Err(err_str) => {
                bail!("WASM plugin '{}' tool '{}' returned error: {}", self.name, tool_name, err_str)
            }
        }
    }
}
