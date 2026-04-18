//! WASM plugin runtime — loads `.wasm` plugins via wasmtime.
//!
//! Each WASM plugin exports:
//!   - `get_manifest() -> *const u8` — returns a JSON-encoded manifest
//!   - `handle_tool(tool: *const u8, args: *const u8) -> *const u8` — executes a tool
//!
//! Host functions provided to plugins:
//!   - `browser_navigate(url)`, `browser_click(selector)`, etc.
//!   - `log(level, message)`
//!
//! This module is a scaffold. The full wasmtime component model integration
//! (host function linking, memory management, component model types) will be
//! completed in Task 7.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use wasmtime::{Config, Engine, Linker, Module, Store};

use crate::browser::BrowserSession;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A loaded WASM plugin, ready to dispatch tool calls.
pub struct WasmPlugin {
    /// Plugin name (from manifest).
    pub name: String,
    /// Tools this plugin exposes.
    pub tools: Vec<WasmToolDef>,
    /// Path to the `.wasm` file on disk.
    pub wasm_path: PathBuf,
    /// Wasmtime engine (shared across plugins).
    #[allow(dead_code)]
    engine: Engine,
    /// Compiled module.
    #[allow(dead_code)]
    module: Module,
    /// Reference to the browser session for host function callbacks.
    #[allow(dead_code)]
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
#[allow(dead_code)]
struct WasmManifestRaw {
    name: String,
    #[serde(default)]
    tools: Vec<WasmToolDef>,
}

/// State passed into the wasmtime `Store`, available to host functions.
#[allow(dead_code)]
struct HostState {
    browser: Arc<Mutex<Option<BrowserSession>>>,
    // TODO(task-7): Add memory allocator tracking, log buffers, etc.
}

// ---------------------------------------------------------------------------
// Directory scanning
// ---------------------------------------------------------------------------

/// Scan a directory for `.wasm` files and return their paths.
///
/// Non-`.wasm` entries and unreadable paths are silently skipped with a
/// debug-level log.
pub fn scan_wasm_plugins(dir: &Path) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            debug!(path = %dir.display(), error = %e, "cannot read WASM plugins directory");
            return Vec::new();
        }
    };

    let mut paths = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                debug!(error = %e, "skipping unreadable directory entry");
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("wasm") {
            debug!(path = %path.display(), "found WASM plugin");
            paths.push(path);
        }
    }
    paths
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Load all WASM plugins from a directory.
///
/// Each `.wasm` file is compiled and its `get_manifest()` export is called
/// to discover the plugin name and available tools.
///
/// Plugins that fail to load are logged at `warn` level and skipped.
pub async fn load_wasm_plugins(
    dir: &Path,
    browser: Arc<Mutex<Option<BrowserSession>>>,
) -> Result<Vec<WasmPlugin>> {
    let paths = scan_wasm_plugins(dir);
    if paths.is_empty() {
        debug!(dir = %dir.display(), "no WASM plugins found");
        return Ok(Vec::new());
    }

    // Shared engine config — async fuel-based execution.
    let mut config = Config::new();
    config.async_support(true);
    let engine = Engine::new(&config).context("failed to create wasmtime engine")?;

    let mut plugins = Vec::new();
    for path in &paths {
        match load_single_plugin(path, &engine, Arc::clone(&browser)).await {
            Ok(plugin) => {
                info!(
                    plugin = %plugin.name,
                    tools = plugin.tools.len(),
                    path = %path.display(),
                    "WASM plugin loaded"
                );
                plugins.push(plugin);
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to load WASM plugin");
            }
        }
    }

    info!(count = plugins.len(), "WASM plugins loaded");
    Ok(plugins)
}

/// Load a single `.wasm` file into a `WasmPlugin`.
async fn load_single_plugin(
    path: &Path,
    engine: &Engine,
    browser: Arc<Mutex<Option<BrowserSession>>>,
) -> Result<WasmPlugin> {
    let wasm_bytes = std::fs::read(path)
        .with_context(|| format!("failed to read WASM file: {}", path.display()))?;

    let module = Module::new(engine, &wasm_bytes)
        .with_context(|| format!("failed to compile WASM module: {}", path.display()))?;

    // TODO(task-7): Link host functions (browser_navigate, browser_click,
    //               browser_evaluate, log, etc.) via the Linker before
    //               instantiating the module.
    let _linker: Linker<HostState> = Linker::new(engine);

    // TODO(task-7): Instantiate the module, call `get_manifest()`, and parse
    //               the returned JSON to populate `name` and `tools`.
    //
    // For now, derive the plugin name from the filename and leave tools empty.
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    Ok(WasmPlugin {
        name,
        tools: Vec::new(),
        wasm_path: path.to_path_buf(),
        engine: engine.clone(),
        module,
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

        // TODO(task-7): Create a fresh Store<HostState>, instantiate the module
        //               with linked host functions, serialize `args` into WASM
        //               linear memory, call `handle_tool(tool_name, args)`, and
        //               deserialize the result.
        debug!(
            plugin = %self.name,
            tool = tool_name,
            "WASM tool call (stub — full impl in task-7)"
        );

        bail!(
            "WASM tool dispatch not yet implemented (task-7): plugin={}, tool={}, args={}",
            self.name,
            tool_name,
            args
        )
    }

    /// Register host functions on the linker.
    ///
    /// These functions are callable by the WASM module and provide access to
    /// browser automation, logging, and other host capabilities.
    #[allow(dead_code)]
    fn register_host_functions(
        _linker: &mut Linker<HostState>,
    ) -> Result<()> {
        // TODO(task-7): Register host functions:
        //   - "env" / "host_log"            — log(level, msg_ptr, msg_len)
        //   - "env" / "browser_navigate"    — navigate(url_ptr, url_len) -> status
        //   - "env" / "browser_click"       — click(selector_ptr, sel_len) -> status
        //   - "env" / "browser_evaluate"    — evaluate(js_ptr, js_len) -> result_ptr
        //   - "env" / "browser_screenshot"  — screenshot() -> bytes_ptr
        //   - "env" / "alloc"               — allocate memory in host for returns
        Ok(())
    }
}
