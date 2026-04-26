//! Plugin manifest parser.
//!
//! Every plugin lives in its own directory under `~/.rsclaw/plugins/<name>/`.
//! rsclaw looks for a manifest file in this order:
//!   1. `plugin.json5`          — rsclaw native format (json5, supports wasm + js)
//!   2. `openclaw.plugin.json`  — OpenClaw compatibility (json, js-only)
//!
//! Example `plugin.json5`:
//! ```json5
//! {
//!   name: "jimeng",
//!   version: "1.0.0",
//!   description: "Jimeng image generation",
//!   runtime: "wasm",           // "wasm" | "node" | "bun" | "deno"
//!   entry: "./jimeng.wasm",    // or "./dist/index.js"
//!   tools: [
//!     {
//!       name: "txt2img",
//!       description: "Generate an image from text",
//!       inputSchema: { type: "object", properties: {} }
//!     }
//!   ]
//! }
//! ```

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// rsclaw native manifest filename.
pub const MANIFEST_FILE: &str = "plugin.json5";

/// OpenClaw compatibility manifest filename.
pub const LEGACY_MANIFEST_FILE: &str = "openclaw.plugin.json";

// ---------------------------------------------------------------------------
// PluginManifest
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifest {
    /// Unique plugin name (slug).
    /// rsclaw uses `name`; OpenClaw extensions use `id`. Both are accepted.
    #[serde(default)]
    pub name: String,
    /// OpenClaw extension ID (fallback for `name`).
    #[serde(default)]
    pub id: Option<String>,
    /// Semver version string.
    pub version: Option<String>,
    /// Human-readable description.
    pub description: Option<String>,
    /// Runtime: "node" | "bun" | "deno" | "wasm". Defaults to "node".
    #[serde(default = "default_runtime")]
    pub runtime: String,
    /// Entry point relative to the plugin directory.
    /// e.g. `"./jimeng.wasm"` or `"./dist/index.js"`.
    /// Optional for OpenClaw extensions (defaults to `"./dist/index.js"`).
    #[serde(default = "default_entry")]
    pub entry: String,
    /// Channels this plugin provides (OpenClaw extension field).
    #[serde(default)]
    pub channels: Vec<String>,
    /// Slots this plugin fills: `"memory"` | `"context_engine"`.
    #[serde(default)]
    pub slots: Vec<String>,
    /// Lifecycle hooks this plugin subscribes to.
    #[serde(default)]
    pub hooks: Vec<String>,
    /// Additional tool definitions exposed by this plugin.
    #[serde(default)]
    pub tools: Vec<PluginToolDef>,
    /// Minimum interval between tool calls in milliseconds. The host enforces
    /// this for wasm plugins (replaces the old plugin-side `host::sleep` at
    /// the top of every dispatch). Default: 0 (no throttling).
    #[serde(default)]
    pub min_call_interval_ms: u32,
    /// Minimum rsclaw version required.
    pub requires_rsclaw: Option<String>,
    /// Arbitrary extra fields for future compatibility.
    #[serde(default, flatten)]
    pub extra: HashMap<String, Value>,

    // --- runtime fields (not in JSON) ---
    /// Absolute path to the plugin directory.
    #[serde(skip)]
    pub dir: PathBuf,
}

fn default_entry() -> String {
    "./dist/index.js".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Option<Value>,
}

fn default_runtime() -> String {
    "node".to_owned()
}

impl PluginManifest {
    /// Normalize after parsing: resolve `id` -> `name` fallback.
    fn normalize(&mut self) {
        // OpenClaw uses `id`, rsclaw uses `name`.
        if self.name.is_empty() {
            if let Some(ref id) = self.id {
                self.name = id.clone();
            }
        }
    }

    /// Whether this plugin uses the WASM runtime.
    pub fn is_wasm(&self) -> bool {
        self.runtime == "wasm"
    }

    /// Whether this is an OpenClaw channel extension.
    pub fn is_channel_extension(&self) -> bool {
        !self.channels.is_empty()
    }

    /// Resolve the absolute path to the entry point.
    pub fn entry_path(&self) -> PathBuf {
        self.dir.join(&self.entry)
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Load a plugin manifest from a directory.
///
/// Tries `plugin.json5` first, then falls back to `openclaw.plugin.json`.
pub fn load_manifest(plugin_dir: &Path) -> Result<PluginManifest> {
    let json5_path = plugin_dir.join(MANIFEST_FILE);
    let legacy_path = plugin_dir.join(LEGACY_MANIFEST_FILE);

    if json5_path.exists() {
        load_manifest_json5(&json5_path, plugin_dir)
    } else if legacy_path.exists() {
        load_manifest_json(&legacy_path, plugin_dir)
    } else {
        anyhow::bail!(
            "no manifest found in {} (expected {} or {})",
            plugin_dir.display(),
            MANIFEST_FILE,
            LEGACY_MANIFEST_FILE,
        )
    }
}

/// Parse a `plugin.json5` manifest.
fn load_manifest_json5(path: &Path, plugin_dir: &Path) -> Result<PluginManifest> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read {}", path.display()))?;

    let mut manifest: PluginManifest = json5::from_str(&raw)
        .with_context(|| format!("json5 parse error in {}", path.display()))?;

    manifest.dir = plugin_dir.to_path_buf();
    manifest.normalize();
    Ok(manifest)
}

/// Parse a legacy `openclaw.plugin.json` manifest.
fn load_manifest_json(path: &Path, plugin_dir: &Path) -> Result<PluginManifest> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read {}", path.display()))?;

    let mut manifest: PluginManifest = serde_json::from_str(&raw)
        .with_context(|| format!("JSON parse error in {}", path.display()))?;

    manifest.dir = plugin_dir.to_path_buf();
    manifest.normalize();
    Ok(manifest)
}

/// Scan a directory for plugin sub-directories (each must have a manifest).
pub fn scan_plugins(plugins_dir: &Path) -> Result<Vec<PluginManifest>> {
    if !plugins_dir.exists() {
        return Ok(Vec::new());
    }

    let mut manifests = Vec::new();

    for entry in std::fs::read_dir(plugins_dir)
        .with_context(|| format!("read plugins dir: {}", plugins_dir.display()))?
        .flatten()
    {
        let plugin_dir = entry.path();
        if !plugin_dir.is_dir() {
            continue;
        }
        // Must have at least one manifest file.
        let has_manifest = plugin_dir.join(MANIFEST_FILE).exists()
            || plugin_dir.join(LEGACY_MANIFEST_FILE).exists();
        if !has_manifest {
            continue;
        }
        match load_manifest(&plugin_dir) {
            Ok(m) => manifests.push(m),
            Err(e) => {
                tracing::warn!(
                    path = %plugin_dir.display(),
                    "failed to load plugin manifest: {e:#}"
                );
            }
        }
    }

    Ok(manifests)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).expect("write file");
    }

    #[test]
    fn parse_json5_manifest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_file(
            tmp.path(),
            MANIFEST_FILE,
            r#"{
  name: "test-wasm",
  version: "2.0.0",
  description: "A WASM plugin",
  runtime: "wasm",
  entry: "./plugin.wasm",
  tools: [
    {
      name: "do_thing",
      description: "Does things",
      inputSchema: { type: "object" }
    }
  ]
}"#,
        );

        let m = load_manifest(tmp.path()).expect("load");
        assert_eq!(m.name, "test-wasm");
        assert_eq!(m.version.as_deref(), Some("2.0.0"));
        assert_eq!(m.runtime, "wasm");
        assert!(m.is_wasm());
        assert_eq!(m.tools.len(), 1);
    }

    #[test]
    fn parse_legacy_manifest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_file(
            tmp.path(),
            LEGACY_MANIFEST_FILE,
            r#"{"name": "legacy", "entry": "./index.js"}"#,
        );

        let m = load_manifest(tmp.path()).expect("load");
        assert_eq!(m.name, "legacy");
        assert_eq!(m.runtime, "node"); // default
        assert!(!m.is_wasm());
    }

    #[test]
    fn json5_takes_priority_over_legacy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_file(
            tmp.path(),
            MANIFEST_FILE,
            r#"{ name: "native", entry: "./plugin.wasm", runtime: "wasm" }"#,
        );
        write_file(
            tmp.path(),
            LEGACY_MANIFEST_FILE,
            r#"{"name": "legacy", "entry": "./index.js"}"#,
        );

        let m = load_manifest(tmp.path()).expect("load");
        assert_eq!(m.name, "native");
        assert!(m.is_wasm());
    }

    #[test]
    fn parse_minimal_manifest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_file(
            tmp.path(),
            MANIFEST_FILE,
            r#"{ name: "minimal", entry: "./main.js" }"#,
        );

        let m = load_manifest(tmp.path()).expect("load");
        assert_eq!(m.name, "minimal");
        assert_eq!(m.runtime, "node");
        assert!(m.slots.is_empty());
    }

    #[test]
    fn scan_plugins_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Native plugin with plugin.json5
        let dir_a = tmp.path().join("plugin-a");
        std::fs::create_dir_all(&dir_a).expect("mkdir");
        write_file(
            &dir_a,
            MANIFEST_FILE,
            r#"{ name: "plugin-a", entry: "./a.wasm", runtime: "wasm" }"#,
        );
        // Legacy plugin with openclaw.plugin.json
        let dir_b = tmp.path().join("plugin-b");
        std::fs::create_dir_all(&dir_b).expect("mkdir");
        write_file(
            &dir_b,
            LEGACY_MANIFEST_FILE,
            &format!(r#"{{"name":"plugin-b","entry":"./index.js"}}"#),
        );
        // A directory without manifest should be ignored.
        std::fs::create_dir_all(tmp.path().join("no-manifest")).expect("mkdir");

        let plugins = scan_plugins(tmp.path()).expect("scan");
        assert_eq!(plugins.len(), 2);
    }
}
