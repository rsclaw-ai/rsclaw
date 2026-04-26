//! WASM plugin runtime — loads `.wasm` component-model plugins via wasmtime.
//!
//! Each WASM plugin exports (via WIT `plugin-api` interface):
//!   - `handle-tool(tool-name, args-json) -> result<string, string>` — executes a tool
//!
//! Tool metadata (name, description, JSON schema) lives in `plugin.json5` —
//! the host does not call back into the wasm to discover tools.
//!
//! Host functions provided to plugins (via WIT `host-browser` and `host-runtime`):
//!   - 13 browser automation functions (open, snapshot, click, fill, etc.)
//!   - `log`, `sleep`, `read-file`

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::debug;
use wasmtime::{
    Engine, Store, StoreLimits, StoreLimitsBuilder,
    component::{Component, Linker, bindgen},
};

/// Per-call wall-clock deadline in epoch ticks, relative to `set_epoch_deadline`
/// being called. The engine ticks every 100ms (see `mod.rs::load_all_plugins`),
/// so 18000 ticks ≈ 30 minutes. Browser-automation plugins (image / video
/// generation, scrape pagination) routinely run for several minutes; the
/// deadline only needs to be tight enough to kill a true runaway.
const EPOCH_DEADLINE_TICKS: u64 = 18000;

/// Per-store memory cap for wasm linear memory.
const MEMORY_CAP_BYTES: usize = 256 * 1024 * 1024;

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
    /// Minimum gap between successive `call_tool` invocations on this plugin
    /// (host-enforced rate limit). 0 disables throttling.
    min_call_interval: Duration,
    /// Last `call_tool` start time, used to compute the throttle delay.
    last_call: Mutex<Option<Instant>>,
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

/// State passed into the wasmtime `Store`, available to host functions.
struct HostState {
    browser: Arc<Mutex<Option<BrowserSession>>>,
    wasi: wasmtime_wasi::WasiCtx,
    wasi_table: wasmtime::component::ResourceTable,
    limits: StoreLimits,
}

fn new_host_state(browser: Arc<Mutex<Option<BrowserSession>>>) -> HostState {
    HostState {
        browser,
        wasi: wasmtime_wasi::WasiCtxBuilder::new().build(),
        wasi_table: wasmtime::component::ResourceTable::new(),
        limits: StoreLimitsBuilder::new()
            .memory_size(MEMORY_CAP_BYTES)
            .build(),
    }
}

/// Build a sandboxed `Store` for one plugin invocation: memory cap + epoch
/// deadline so a buggy plugin can't OOM or hang the gateway.
fn new_sandboxed_store(
    engine: &Engine,
    browser: Arc<Mutex<Option<BrowserSession>>>,
) -> Store<HostState> {
    let mut store = Store::new(engine, new_host_state(browser));
    store.limiter(|s| &mut s.limits);
    store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);
    store
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

/// Canonicalize a filesystem path from a WASM plugin and reject anything that
/// resolves outside the plugin workspace. `~` expansion and absolute paths
/// in the input are tolerated *only* if the canonical result still lives
/// under the workspace dir — otherwise the call is rejected.
fn canonicalize_plugin_path(input: &str) -> Result<PathBuf, String> {
    let workspace = crate::config::loader::base_dir().join("workspace");
    let canonical = crate::agent::runtime::canonicalize_external_path(input, &workspace);
    if !canonical.starts_with(&workspace) {
        return Err(format!(
            "plugin path '{}' resolves outside workspace ({})",
            input,
            workspace.display()
        ));
    }
    Ok(canonical)
}

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

    async fn browser_eval(&mut self, code: String) -> Result<Result<String, String>> {
        Ok(self.browser_action("evaluate", json!({"js": code})).await)
    }

    async fn browser_wait_text(
        &mut self,
        text: String,
        timeout_ms: u32,
    ) -> Result<Result<String, String>> {
        let timeout_secs = u64::from(timeout_ms / 1000).max(1);
        Ok(self
            .browser_action(
                "wait",
                json!({"target": "text", "value": text, "timeout": timeout_secs}),
            )
            .await
            .map(|_| "ok".to_string()))
    }

    async fn wait_for_selector(
        &mut self,
        css_selector: String,
        timeout_ms: u32,
    ) -> Result<Result<String, String>> {
        let timeout_secs = u64::from(timeout_ms / 1000).max(1);
        Ok(self
            .browser_action(
                "wait",
                json!({"target": "element", "value": css_selector, "timeout": timeout_secs}),
            )
            .await
            .map(|_| "ok".to_string()))
    }

    async fn wait_for_network_idle(
        &mut self,
        timeout_ms: u32,
    ) -> Result<Result<String, String>> {
        let timeout_secs = u64::from(timeout_ms / 1000).max(1);
        Ok(self
            .browser_action(
                "wait",
                json!({"target": "networkidle", "timeout": timeout_secs}),
            )
            .await
            .map(|_| "ok".to_string()))
    }

    async fn eval_with_args(
        &mut self,
        code: String,
        args_json: String,
    ) -> Result<Result<String, String>> {
        // JSON is valid JS expression syntax, so we can embed args_json
        // directly as an object literal — no escaping dance required.
        let args_literal = if args_json.trim().is_empty() {
            "null".to_string()
        } else {
            args_json
        };
        let wrapped = format!(
            r#"(async function() {{
                const __args = ({args_literal});
                const __fn = ({code});
                const __out = await __fn(__args);
                return typeof __out === "string" ? __out : JSON.stringify(__out);
            }})()"#
        );
        Ok(self
            .browser_action("evaluate", json!({"js": wrapped}))
            .await)
    }

    async fn switch_latest_tab(&mut self) -> Result<Result<String, String>> {
        let mut guard = self.browser.lock().await;
        if guard.is_none() {
            return Ok(Err("browser not initialized".to_string()));
        }
        let session = guard.as_mut().expect("browser presence checked above");
        let tabs_val = match session.execute("list_tabs", &json!({})).await {
            Ok(v) => v,
            Err(e) => return Ok(Err(format!("list_tabs failed: {e:#}"))),
        };
        let tabs = match tabs_val.get("tabs").and_then(|t| t.as_array()) {
            Some(t) => t,
            None => return Ok(Err("list_tabs returned no tabs array".to_string())),
        };
        let last = match tabs.last() {
            Some(t) => t,
            None => return Ok(Err("no tabs to switch to".to_string())),
        };
        let tid = match last.get("id").and_then(|t| t.as_str()) {
            Some(s) => s,
            None => return Ok(Err("last tab has no id".to_string())),
        };
        let url = last.get("url").and_then(|u| u.as_str()).unwrap_or("?");
        match session.execute("switch_tab", &json!({"target_id": tid})).await {
            Ok(_) => Ok(Ok(format!("switched to tab: {url}"))),
            Err(e) => Ok(Err(format!("switch_tab failed: {e:#}"))),
        }
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
        let canonical = match canonicalize_plugin_path(&filepath) {
            Ok(p) => p,
            Err(e) => return Ok(Err(e)),
        };
        Ok(self
            .browser_action(
                "upload",
                json!({"ref": ref_str, "filepath": canonical.to_string_lossy()}),
            )
            .await)
    }

    async fn browser_get_url(&mut self) -> Result<Result<String, String>> {
        Ok(self.browser_action("get_url", json!({})).await)
    }
}

impl rsclaw::jimeng::host_runtime::Host for HostState {
    async fn log(&mut self, level: String, msg: String) -> Result<()> {
        // Use the module path as target (instead of "wasm_plugin") so plugin
        // logs inherit the default tracing filter level for this crate.
        match level.as_str() {
            "error" => tracing::error!(plugin_log = true, "{msg}"),
            "warn" => tracing::warn!(plugin_log = true, "{msg}"),
            "info" => tracing::info!(plugin_log = true, "{msg}"),
            "debug" => tracing::debug!(plugin_log = true, "{msg}"),
            _ => tracing::trace!(plugin_log = true, "{msg}"),
        }
        Ok(())
    }

    async fn sleep(&mut self, ms: u32) -> Result<()> {
        tokio::time::sleep(std::time::Duration::from_millis(u64::from(ms))).await;
        Ok(())
    }

    async fn notify(&mut self, message: String) -> Result<Result<String, String>> {
        // For now, plugin progress notifications are surfaced via tracing.
        // When we wire wasm plugins into the agent runtime's notification_tx
        // (see runtime.rs), this becomes the dispatch point.
        tracing::info!(target: "wasm_plugin_notify", "{message}");
        Ok(Ok("ok".to_string()))
    }

    async fn read_file(&mut self, path: String) -> Result<Result<String, String>> {
        let canonical = match canonicalize_plugin_path(&path) {
            Ok(p) => p,
            Err(e) => return Ok(Err(e)),
        };
        match tokio::fs::read_to_string(&canonical).await {
            Ok(contents) => Ok(Ok(contents)),
            Err(e) => Ok(Err(format!("failed to read {}: {e}", canonical.display()))),
        }
    }
}

impl rsclaw::jimeng::host_storage::Host for HostState {
    async fn allocate_artifact(
        &mut self,
        filename: String,
    ) -> Result<Result<String, String>> {
        // Reject path separators — plugins must supply a bare filename.
        if filename.contains('/') || filename.contains('\\') {
            return Ok(Err(format!(
                "allocate_artifact: filename must not contain path separators: {filename}"
            )));
        }
        // Layout: <Downloads>/rsclaw/<videos|images|files>/<nanos_hex>/<filename>
        // Use the OS Downloads dir (visible, cross-platform: macOS/Windows/Linux
        // all expose one) so Tauri v2's asset protocol scope can match without
        // fighting `require_literal_leading_dot`. nanos_hex makes each
        // allocation unique so repeated filenames don't collide.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let category = match std::path::Path::new(&filename)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("mp4" | "mov" | "webm" | "mkv" | "avi" | "m4v") => "videos",
            Some("jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "svg") => "images",
            _ => "files",
        };
        let base = dirs_next::download_dir()
            .unwrap_or_else(|| {
                dirs_next::home_dir()
                    .unwrap_or_else(crate::config::loader::base_dir)
                    .join("Downloads")
            })
            .join("rsclaw")
            .join(category);
        let subdir = base.join(format!("{nanos:x}"));
        if let Err(e) = std::fs::create_dir_all(&subdir) {
            return Ok(Err(format!("allocate_artifact: create_dir: {e}")));
        }
        let full = subdir.join(&filename);
        tracing::debug!(target: "wasm_plugin", "allocated artifact: {}", full.display());
        Ok(Ok(full.to_string_lossy().to_string()))
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
    rsclaw::jimeng::host_storage::add_to_linker(&mut linker, |state: &mut HostState| state)?;
    Ok(linker)
}

/// Load a WASM plugin from a `PluginManifest`.
///
/// The manifest's `entry` field points to the `.wasm` file relative to the
/// plugin directory. We compile the component and pre-build the linker, but
/// do *not* instantiate — tools come from `plugin.json5`, which is the single
/// source of truth.
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

    let tools = manifest
        .tools
        .iter()
        .map(|t| WasmToolDef {
            name: t.name.clone(),
            description: t.description.clone(),
            parameters: t.input_schema.clone().unwrap_or(json!({"type": "object"})),
        })
        .collect();

    Ok(WasmPlugin {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        description: manifest.description.clone(),
        tools,
        wasm_path: path.to_path_buf(),
        engine: engine.clone(),
        component,
        linker,
        browser,
        min_call_interval: Duration::from_millis(u64::from(manifest.min_call_interval_ms)),
        last_call: Mutex::new(None),
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

        // Host-side rate limit: hold off until the configured interval has
        // elapsed since the previous call. Replaces per-plugin sleeps in
        // dispatch code.
        if !self.min_call_interval.is_zero() {
            let mut last = self.last_call.lock().await;
            if let Some(t) = *last {
                let elapsed = t.elapsed();
                if elapsed < self.min_call_interval {
                    tokio::time::sleep(self.min_call_interval - elapsed).await;
                }
            }
            *last = Some(Instant::now());
        }

        // Fresh store per call for isolation, with memory cap and epoch deadline.
        let mut store = new_sandboxed_store(&self.engine, Arc::clone(&self.browser));

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
