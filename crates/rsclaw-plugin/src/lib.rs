//! Plugin subsystem.
//!
//! Plugins are directories under `~/.rsclaw/plugins/<name>/` with a
//! `plugin.json5` (or legacy `openclaw.plugin.json`) manifest.
//!
//! Supported runtimes:
//!   - `node` / `bun` / `deno` — JS runtime (subprocess JSON-RPC)
//!   - `wasm`                   — wasmtime component model
//!
//! Public API:
//!   - `PluginManifest` / `load_manifest()` / `scan_plugins()`
//!   - `SlotRegistry`   — memory + context_engine slots
//!   - `Plugin`         — live JS plugin handle (spawned subprocess)
//!   - `WasmPlugin`     — live WASM plugin handle (wasmtime)
//!   - `load_all_plugins()` — unified loader that dispatches by runtime

pub mod host_methods;
pub mod js_runtime;
pub mod manifest;
pub mod slots;
pub mod wasm_runtime;

use std::{collections::HashMap, sync::{Arc, OnceLock}};

use anyhow::Result;
use futures::future::BoxFuture;
pub use js_runtime::Plugin;
pub use manifest::{
    LEGACY_MANIFEST_FILE, MANIFEST_FILE, PluginManifest, PluginSlashCommand, PluginToolDef,
    load_manifest, scan_plugins,
};
pub use slots::{ContextEngineSlot, MemoryItem, MemorySlot, MemoryStoreSlot, SlotRegistry};
use tracing::{info, warn};

/// Invocation routing context passed to long-lived plugin background tasks.
#[derive(Debug, Clone, Default)]
pub struct PluginInvocationContext {
    pub target_id: String,
    pub channel: String,
    pub agent_id: String,
    pub peer_id: String,
    pub chat_id: String,
    pub session_key: String,
    pub is_group: bool,
}

/// Host-provided bridge for trusted WASM plugin background capabilities.
pub trait PluginBackgroundHost: Send + Sync {
    fn cron_register(
        &self,
        plugin: String,
        name: String,
        schedule_json: String,
        ctx: Option<PluginInvocationContext>,
    ) -> BoxFuture<'static, std::result::Result<String, String>>;

    fn sse_subscribe(
        &self,
        plugin: String,
        name: String,
        url: String,
        headers_json: String,
        resume_key: String,
        ctx: Option<PluginInvocationContext>,
    ) -> BoxFuture<'static, std::result::Result<String, String>>;

    fn push_outbound(
        &self,
        channel: String,
        peer_id: String,
        message_json: String,
        ctx: Option<PluginInvocationContext>,
    ) -> BoxFuture<'static, std::result::Result<String, String>>;

    fn submit_agent_turn(
        &self,
        session_key: String,
        prompt: String,
        route_json: String,
        ctx: Option<PluginInvocationContext>,
    ) -> BoxFuture<'static, std::result::Result<String, String>>;
}

static PLUGIN_BACKGROUND_HOST: OnceLock<Arc<dyn PluginBackgroundHost>> = OnceLock::new();

/// Install the process-wide background host used by trusted WASM plugins.
pub fn set_plugin_background_host(host: Arc<dyn PluginBackgroundHost>) {
    if PLUGIN_BACKGROUND_HOST.set(host).is_err() {
        warn!("plugin background host already installed, ignoring duplicate install");
    }
}

pub(crate) async fn cron_register(
    plugin: String,
    name: String,
    schedule_json: String,
    ctx: Option<PluginInvocationContext>,
) -> std::result::Result<String, String> {
    let Some(host) = PLUGIN_BACKGROUND_HOST.get().cloned() else {
        return Err("plugin background host is not installed".to_owned());
    };
    host.cron_register(plugin, name, schedule_json, ctx).await
}

pub(crate) async fn sse_subscribe(
    plugin: String,
    name: String,
    url: String,
    headers_json: String,
    resume_key: String,
    ctx: Option<PluginInvocationContext>,
) -> std::result::Result<String, String> {
    let Some(host) = PLUGIN_BACKGROUND_HOST.get().cloned() else {
        return Err("plugin background host is not installed".to_owned());
    };
    host.sse_subscribe(plugin, name, url, headers_json, resume_key, ctx).await
}

pub(crate) async fn push_outbound(
    channel: String,
    peer_id: String,
    message_json: String,
    ctx: Option<PluginInvocationContext>,
) -> std::result::Result<String, String> {
    let Some(host) = PLUGIN_BACKGROUND_HOST.get().cloned() else {
        return Err("plugin background host is not installed".to_owned());
    };
    host.push_outbound(channel, peer_id, message_json, ctx).await
}

pub(crate) async fn submit_agent_turn(
    session_key: String,
    prompt: String,
    route_json: String,
    ctx: Option<PluginInvocationContext>,
) -> std::result::Result<String, String> {
    let Some(host) = PLUGIN_BACKGROUND_HOST.get().cloned() else {
        return Err("plugin background host is not installed".to_owned());
    };
    host.submit_agent_turn(session_key, prompt, route_json, ctx).await
}
pub use wasm_runtime::{WasmPlugin, WasmToolDef, load_wasm_plugin};

use rsclaw_config::schema::PluginsConfig;

// ---------------------------------------------------------------------------
// PluginRegistry
// ---------------------------------------------------------------------------

/// Loaded and running plugins, indexed by name.
pub struct PluginRegistry {
    /// Shell plugins (subprocess + JSON-RPC bridge: node/bun/deno).
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

    /// Look up a JS-runtime plugin by name. Returns None if no such plugin
    /// is loaded or the plugin uses the wasm runtime.
    pub fn get_js(&self, name: &str) -> Option<&Plugin> {
        self.plugins.get(name)
    }

    /// Iterate over all loaded JS-runtime plugins as (name, plugin) pairs.
    /// Used by the agent runtime to build LLM tool definitions and the
    /// plugins system message.
    pub fn js_plugins_iter(&self) -> impl Iterator<Item = (&String, &Plugin)> {
        self.plugins.iter()
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
    wasm_browser: Arc<tokio::sync::Mutex<Option<rsclaw_browser::BrowserSession>>>,
    notify_tx: Option<tokio::sync::broadcast::Sender<rsclaw_channel::OutboundMessage>>,
    providers: Option<Arc<rsclaw_provider::registry::ProviderRegistry>>,
    vision_model: Option<String>,
) -> Result<PluginRegistry> {
    let manifests = scan_plugins(plugins_dir)?;
    let mut registry = PluginRegistry::new();

    let host_dispatch = Arc::new(host_methods::HostMethodRegistry::new(
        notify_tx,
        Arc::clone(&wasm_browser),
    ));

    // Shared wasmtime engine for all WASM plugins.
    let wasm_engine = if manifests.iter().any(|m| m.is_wasm()) {
        let mut wasm_config = wasmtime::Config::new();
        wasm_config.async_support(true);
        // Enable epoch interruption so we can bound wasm-CPU time per call
        // (caps runaway loops without affecting awaits on host async calls).
        wasm_config.epoch_interruption(true);
        let engine = wasmtime::Engine::new(&wasm_config)
            .map_err(|e| anyhow::anyhow!("create wasmtime engine: {e}"))?;
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
            match load_wasm_plugin(
                &manifest,
                engine,
                Arc::clone(&wasm_browser),
                providers.clone(),
                vision_model.clone(),
            )
            .await
            {
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
            // JS runtime (subprocess + JSON-RPC bridge: node/bun/deno)
            match Plugin::spawn(manifest, host_dispatch.clone()).await {
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
