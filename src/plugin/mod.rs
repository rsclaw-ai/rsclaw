//! Plugin subsystem.
//!
//! Plugins are directories under `~/.rsclaw/plugins/<name>/` with a
//! `plugin.json5` (or legacy `openclaw.plugin.json`) manifest.
//!
//! Supported runtimes:
//!   - `node` / `bun` / `deno` — Shell Bridge (subprocess JSON-RPC)
//!   - `wasm`                   — wasmtime component model
//!
//! Public API:
//!   - `PluginManifest` / `load_manifest()` / `scan_plugins()`
//!   - `SlotRegistry`   — memory + context_engine slots
//!   - `Plugin`         — live JS plugin handle (spawned subprocess)
//!   - `WasmPlugin`     — live WASM plugin handle (wasmtime)
//!   - `load_all_plugins()` — unified loader that dispatches by runtime

pub mod manifest;
pub mod shell_bridge;
pub mod slots;
pub mod wasm_runtime;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
pub use manifest::{LEGACY_MANIFEST_FILE, MANIFEST_FILE, PluginManifest, PluginToolDef, load_manifest, scan_plugins};
pub use shell_bridge::Plugin;
pub use slots::{ContextEngineSlot, MemoryItem, MemorySlot, MemoryStoreSlot, SlotRegistry};
pub use wasm_runtime::{WasmPlugin, WasmToolDef, load_wasm_plugin};
use tracing::{info, warn};

use crate::config::schema::PluginsConfig;

// ---------------------------------------------------------------------------
// PluginRegistry
// ---------------------------------------------------------------------------

/// Loaded and running plugins, indexed by name.
pub struct PluginRegistry {
    /// JS plugins (shell bridge).
    plugins: HashMap<String, Plugin>,
    /// WASM plugins (wasmtime).
    wasm_plugins: Vec<WasmPlugin>,
    pub slots: SlotRegistry,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self {
            plugins: HashMap::new(),
            wasm_plugins: Vec::new(),
            slots: SlotRegistry::new(),
        }
    }

    pub fn get(&self, name: &str) -> Option<&Plugin> {
        self.plugins.get(name)
    }

    pub fn all(&self) -> impl Iterator<Item = &Plugin> {
        self.plugins.values()
    }

    /// Get all loaded WASM plugins.
    pub fn wasm_all(&self) -> &[WasmPlugin] {
        &self.wasm_plugins
    }

    /// Total number of loaded plugins (JS + WASM).
    pub fn len(&self) -> usize {
        self.plugins.len() + self.wasm_plugins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty() && self.wasm_plugins.is_empty()
    }

    /// Number of JS plugins.
    pub fn js_count(&self) -> usize {
        self.plugins.len()
    }

    /// Number of WASM plugins.
    pub fn wasm_count(&self) -> usize {
        self.wasm_plugins.len()
    }

    /// Take WASM plugins out of the registry as a Vec.
    /// Used during startup to pass them to the agent runtime.
    pub fn take_wasm_plugins(&mut self) -> Vec<WasmPlugin> {
        std::mem::take(&mut self.wasm_plugins)
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Unified Loader
// ---------------------------------------------------------------------------

/// Scan a plugin directory, load all plugins (JS + WASM), and build a registry.
///
/// Dispatches each plugin to the appropriate runtime based on the `runtime`
/// field in its manifest.
pub async fn load_all_plugins(
    plugins_dir: &std::path::Path,
    config: Option<&PluginsConfig>,
    wasm_browser: Arc<tokio::sync::Mutex<Option<crate::browser::BrowserSession>>>,
) -> Result<PluginRegistry> {
    let manifests = scan_plugins(plugins_dir)?;
    let mut registry = PluginRegistry::new();

    // Shared wasmtime engine for all WASM plugins.
    let wasm_engine = if manifests.iter().any(|m| m.is_wasm()) {
        let mut wasm_config = wasmtime::Config::new();
        wasm_config.async_support(true);
        // Enable epoch interruption so we can bound wasm-CPU time per call
        // (caps runaway loops without affecting awaits on host async calls).
        wasm_config.epoch_interruption(true);
        let engine = wasmtime::Engine::new(&wasm_config)?;
        // Tick the engine at 100ms; per-call deadline is set in wasm_runtime.
        let tick_engine = engine.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(100));
            loop {
                ticker.tick().await;
                tick_engine.increment_epoch();
            }
        });
        Some(engine)
    } else {
        None
    };

    for manifest in manifests {
        // Check enable flag in config.
        let enabled = config
            .and_then(|c| c.entries.as_ref())
            .and_then(|e| e.get(&manifest.name))
            .and_then(|e| e.enabled)
            .unwrap_or(true);

        if !enabled {
            info!(plugin = %manifest.name, "plugin disabled via config");
            continue;
        }

        if manifest.is_wasm() {
            // WASM runtime
            let engine = wasm_engine.as_ref().expect("wasm engine initialized");
            match load_wasm_plugin(&manifest, engine, Arc::clone(&wasm_browser)).await {
                Ok(plugin) => {
                    info!(
                        plugin = %plugin.name,
                        tools = plugin.tools.len(),
                        version = ?manifest.version,
                        "WASM plugin loaded"
                    );
                    registry.wasm_plugins.push(plugin);
                }
                Err(e) => {
                    warn!(plugin = %manifest.name, "failed to load WASM plugin: {e:#}");
                }
            }
        } else {
            // JS runtime (shell bridge)
            match Plugin::spawn(manifest).await {
                Ok(plugin) => {
                    info!(plugin = %plugin.manifest.name, "JS plugin started");
                    registry
                        .plugins
                        .insert(plugin.manifest.name.clone(), plugin);
                }
                Err(e) => {
                    warn!("failed to start plugin: {e:#}");
                }
            }
        }
    }

    info!(
        total = registry.len(),
        js = registry.js_count(),
        wasm = registry.wasm_count(),
        "plugins loaded"
    );
    Ok(registry)
}
