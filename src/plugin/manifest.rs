//! `openclaw.plugin.json` manifest parser.
//!
//! Every plugin must have an `openclaw.plugin.json` at its root.
//! rsclaw supports the same manifest format to ensure full compatibility
//! with existing OpenClaw plugins.
//!
//! Example manifest:
//! ```json
//! {
//!   "name": "my-plugin",
//!   "version": "1.0.0",
//!   "description": "Does something",
//!   "runtime": "node",
//!   "entry": "./dist/index.js",
//!   "slots": ["memory"],
//!   "hooks": ["before_prompt_build", "after_tool_call"],
//!   "tools": [
//!     {
//!       "name": "do_thing",
//!       "description": "Does the thing",
//!       "input_schema": { "type": "object", "properties": {} }
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

pub const MANIFEST_FILE: &str = "openclaw.plugin.json";

// ---------------------------------------------------------------------------
// PluginManifest
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifest {
    /// Unique plugin name (slug).
    pub name: String,
    /// Semver version string.
    pub version: Option<String>,
    /// Human-readable description.
    pub description: Option<String>,
    /// JS runtime: "node" | "bun" | "deno". Defaults to "node".
    #[serde(default = "default_runtime")]
    pub runtime: String,
    /// Entry point relative to the plugin directory, e.g. `"./dist/index.js"`.
    pub entry: String,
    /// Slots this plugin fills: `"memory"` | `"context_engine"`.
    #[serde(default)]
    pub slots: Vec<String>,
    /// Lifecycle hooks this plugin subscribes to.
    #[serde(default)]
    pub hooks: Vec<String>,
    /// Additional tool definitions exposed by this plugin.
    #[serde(default)]
    pub tools: Vec<PluginToolDef>,
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

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Load an `openclaw.plugin.json` from a directory.
pub fn load_manifest(plugin_dir: &Path) -> Result<PluginManifest> {
    let path = plugin_dir.join(MANIFEST_FILE);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;

    let mut manifest: PluginManifest = serde_json::from_str(&raw)
        .with_context(|| format!("JSON parse error in {}", path.display()))?;

    manifest.dir = plugin_dir.to_path_buf();
    Ok(manifest)
}

/// Scan a directory for plugin sub-directories (each must have
/// `openclaw.plugin.json`).
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
        let mf = plugin_dir.join(MANIFEST_FILE);
        if !mf.exists() {
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

    fn write_manifest(dir: &Path, json: &str) {
        std::fs::write(dir.join(MANIFEST_FILE), json).expect("write manifest");
    }

    #[test]
    fn parse_full_manifest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_manifest(
            tmp.path(),
            r#"{
  "name": "test-plugin",
  "version": "2.0.0",
  "description": "A test plugin",
  "runtime": "bun",
  "entry": "./index.js",
  "slots": ["memory"],
  "hooks": ["before_prompt_build"],
  "tools": [
    {
      "name": "do_thing",
      "description": "Does things",
      "inputSchema": { "type": "object" }
    }
  ]
}"#,
        );

        let m = load_manifest(tmp.path()).expect("load");
        assert_eq!(m.name, "test-plugin");
        assert_eq!(m.runtime, "bun");
        assert_eq!(m.slots, vec!["memory"]);
        assert_eq!(m.hooks, vec!["before_prompt_build"]);
        assert_eq!(m.tools.len(), 1);
    }

    #[test]
    fn parse_minimal_manifest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_manifest(tmp.path(), r#"{"name": "minimal", "entry": "./main.js"}"#);

        let m = load_manifest(tmp.path()).expect("load");
        assert_eq!(m.name, "minimal");
        assert_eq!(m.runtime, "node"); // default
        assert!(m.slots.is_empty());
    }

    #[test]
    fn scan_plugins_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        for slug in ["plugin-a", "plugin-b"] {
            let dir = tmp.path().join(slug);
            std::fs::create_dir_all(&dir).expect("mkdir");
            write_manifest(
                &dir,
                &format!(r#"{{"name":"{slug}","entry":"./index.js"}}"#),
            );
        }
        // A directory without manifest should be ignored.
        std::fs::create_dir_all(tmp.path().join("no-manifest")).expect("mkdir");

        let plugins = scan_plugins(tmp.path()).expect("scan");
        assert_eq!(plugins.len(), 2);
    }
}
