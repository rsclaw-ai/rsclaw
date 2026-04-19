//! Plugin subsystem.
//!
//! Plugins are TypeScript/JavaScript packages that extend rsclaw via the
//! Shell Bridge (subprocess JSON-RPC). They can fill *slots* (memory,
//! context_engine) and hook into the agent lifecycle.
//!
//! Public API:
//!   - `PluginManifest` / `load_manifest()` / `scan_plugins()`
//!   - `SlotRegistry`   — memory + context_engine slots
//!   - `Plugin`         — live plugin handle (spawned subprocess)
//!   - `ShellBridgePlugin` — low-level JSON-RPC bridge

pub mod manifest;
pub mod shell_bridge;
pub mod slots;
pub mod wasm_runtime;

use std::collections::HashMap;

use anyhow::Result;
pub use manifest::{MANIFEST_FILE, PluginManifest, PluginToolDef, load_manifest, scan_plugins};
pub use shell_bridge::Plugin;
pub use slots::{ContextEngineSlot, MemoryItem, MemorySlot, MemoryStoreSlot, SlotRegistry};
pub use wasm_runtime::{WasmPlugin, WasmToolDef, load_wasm_plugins, scan_wasm_plugins};
use tracing::{info, warn};

use crate::config::schema::PluginsConfig;

// ---------------------------------------------------------------------------
// PluginRegistry
// ---------------------------------------------------------------------------

/// Loaded and running plugins, indexed by name.
pub struct PluginRegistry {
    plugins: HashMap<String, Plugin>,
    pub slots: SlotRegistry,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self {
            plugins: HashMap::new(),
            slots: SlotRegistry::new(),
        }
    }

    pub fn get(&self, name: &str) -> Option<&Plugin> {
        self.plugins.get(name)
    }

    pub fn all(&self) -> impl Iterator<Item = &Plugin> {
        self.plugins.values()
    }

    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

/// Scan a plugin directory, spawn enabled plugins, and build a registry.
pub async fn load_plugins(
    plugins_dir: &std::path::Path,
    config: Option<&PluginsConfig>,
    _slot_config: Option<&crate::config::schema::PluginSlots>,
) -> Result<PluginRegistry> {
    let manifests = scan_plugins(plugins_dir)?;
    let mut registry = PluginRegistry::new();

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

        match Plugin::spawn(manifest).await {
            Ok(plugin) => {
                info!(plugin = %plugin.manifest.name, "plugin started");
                registry
                    .plugins
                    .insert(plugin.manifest.name.clone(), plugin);
            }
            Err(e) => {
                warn!("failed to start plugin: {e:#}");
            }
        }
    }

    info!(count = registry.len(), "plugins loaded");
    Ok(registry)
}
