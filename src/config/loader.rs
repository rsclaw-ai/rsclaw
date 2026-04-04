//! Config file loading: JSON5 parsing, `${VAR}` expansion, `$include`
//! resolution.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use regex::Regex;
use tracing::debug;

use super::schema::Config;

/// Convert a path to a string using forward slashes (cross-platform safe for
/// JSON/config). On Windows, backslashes in paths break JSON string parsing.
pub fn path_to_forward_slash(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Matches `${VAR_NAME}` patterns.
static ENV_VAR_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").expect("valid regex")
});

/// Expand `${VAR}` references and `~/` tilde in a raw config string.
/// Variables that are not set are left verbatim and a warning is emitted.
/// `~/` is expanded to `$HOME/` so workspace and path values resolve correctly.
pub fn expand_env_vars(raw: &str) -> String {
    let expanded = ENV_VAR_RE
        .replace_all(raw, |caps: &regex::Captures<'_>| {
            let var = &caps[1];
            std::env::var(var).unwrap_or_else(|_| {
                debug!(var, "env var not set (referenced in config)");
                caps[0].to_string()
            })
        })
        .into_owned();

    // Expand ~/  →  $HOME/  so path values are absolute.
    if let Some(home) = dirs_next::home_dir() {
        let home_s = path_to_forward_slash(&home);
        // Replace every occurrence of ~/ (covers paths inside JSON strings).
        expanded.replace("~/", &format!("{home_s}/"))
    } else {
        expanded
    }
}

// ---------------------------------------------------------------------------
// JSON5 loader (openclaw.json / openclaw.json5)
// ---------------------------------------------------------------------------

/// Load and parse a JSON5 config file, resolving `$include` directives
/// and expanding `${VAR}` placeholders.
pub fn load_json5(path: &Path) -> Result<Config> {
    let base_dir = path.parent().unwrap_or(Path::new("."));
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config: {}", path.display()))?;

    // 1. Expand env vars before any parsing.
    let expanded = expand_env_vars(&raw);

    // 2. Parse into a generic JSON value so we can handle $include.
    let mut value: serde_json::Value = json5::from_str(&expanded)
        .with_context(|| format!("JSON5 parse error in {}", path.display()))?;

    // 3. Resolve $include directives recursively.
    resolve_includes(&mut value, base_dir, 0)?;

    // 4. Deserialize into the typed schema.
    let config: Config = serde_json::from_value(value)
        .with_context(|| format!("schema error in {}", path.display()))?;

    Ok(config)
}

// ---------------------------------------------------------------------------
// $include resolution
// ---------------------------------------------------------------------------

/// Maximum nesting depth for `$include` to prevent infinite recursion.
const MAX_INCLUDE_DEPTH: usize = 10;

/// Recursively replace `{ "$include": "./path/to/file.json5" }` nodes with the
/// contents of the referenced file.
fn resolve_includes(value: &mut serde_json::Value, base_dir: &Path, depth: usize) -> Result<()> {
    if depth > MAX_INCLUDE_DEPTH {
        anyhow::bail!("$include nesting exceeds maximum depth of {MAX_INCLUDE_DEPTH}");
    }

    match value {
        serde_json::Value::Object(map) => {
            // Collect keys that need $include resolution.
            let include_keys: Vec<String> = map
                .iter()
                .filter(|(_, v)| has_include(v))
                .map(|(k, _)| k.clone())
                .collect();

            for key in include_keys {
                let path_str = extract_include_path(&map[&key])
                    .with_context(|| format!("$include in key `{key}`"))?;
                // Expand ~/ before joining so absolute home paths work.
                let include_path = if let Some(rest) = path_str.strip_prefix("~/") {
                    dirs_next::home_dir().unwrap_or_default().join(rest)
                } else {
                    base_dir.join(&path_str)
                };
                let included = load_include_file(&include_path, depth + 1)?;
                map.insert(key, included);
            }

            // Recurse into remaining values.
            for v in map.values_mut() {
                resolve_includes(v, base_dir, depth)?;
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                resolve_includes(v, base_dir, depth)?;
            }
        }
        _ => {}
    }

    Ok(())
}

fn has_include(value: &serde_json::Value) -> bool {
    matches!(value, serde_json::Value::Object(m) if m.contains_key("$include") && m.len() == 1)
}

fn extract_include_path(value: &serde_json::Value) -> Result<String> {
    let map = value.as_object().expect("caller checked");
    map["$include"]
        .as_str()
        .map(str::to_owned)
        .with_context(|| "$include value must be a string path")
}

fn load_include_file(path: &Path, depth: usize) -> Result<serde_json::Value> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read $include: {}", path.display()))?;

    let expanded = expand_env_vars(&raw);

    let mut value: serde_json::Value = json5::from_str(&expanded)
        .with_context(|| format!("JSON5 parse error in $include {}", path.display()))?;

    let base_dir = path.parent().unwrap_or(Path::new("."));
    resolve_includes(&mut value, base_dir, depth)?;

    Ok(value)
}

// ---------------------------------------------------------------------------
// Config source detection
// ---------------------------------------------------------------------------

/// Return the first existing config file path, using the following priority:
///
/// 1. `RSCLAW_CONFIG_PATH` env var (set by `--config-path` -- highest priority)
/// 2. `$RSCLAW_BASE_DIR/rsclaw.json5` (set by `--base-dir`/`--dev`/`--profile`)
/// 3. `~/.rsclaw/rsclaw.json5` -- rsclaw-native default
/// 4. `.rsclaw.json5` in the current directory
///
/// OpenClaw config is NOT auto-loaded. Use `rsclaw setup` to migrate.
pub fn detect_config_path() -> Option<PathBuf> {
    // 1. RSCLAW_CONFIG_PATH -- explicit override (set by --config-path).
    if let Ok(p) = std::env::var("RSCLAW_CONFIG_PATH") {
        let path = expand_tilde_path(&p);
        if path.exists() {
            return Some(path);
        }
    }

    // 2. Base dir config (set by --base-dir / --dev / --profile).
    if let Ok(bd) = std::env::var("RSCLAW_BASE_DIR") {
        let p = expand_tilde_path(&bd).join("rsclaw.json5");
        if p.exists() {
            return Some(p);
        }
    }

    let home = dirs_next::home_dir()?;

    // 3. rsclaw-native default.
    let rsclaw = home.join(".rsclaw/rsclaw.json5");
    if rsclaw.exists() {
        return Some(rsclaw);
    }

    // 4. Current directory fallback.
    let local = PathBuf::from(".rsclaw.json5");
    if local.exists() {
        return Some(local);
    }

    None
}

/// Resolve the rsclaw base directory (state root), respecting env vars and
/// `--base-dir` CLI arg (injected as `RSCLAW_BASE_DIR` before this is called).
///
/// Resolution order:
///   1. `RSCLAW_BASE_DIR` (set by `--base-dir`, `--dev`, `--profile`)
///   2. Parent dir of the detected config file (if config is in ~/.openclaw/,
///      base_dir = ~/.openclaw/)
///   3. `~/.rsclaw` (default)
pub fn base_dir() -> PathBuf {
    // 1. Explicit override
    if let Ok(p) = std::env::var("RSCLAW_BASE_DIR") {
        return expand_tilde_path(&p);
    }

    // 2. Derive from config file location: data lives alongside config
    if let Some(config_path) = detect_config_path() {
        if let Some(parent) = config_path.parent() {
            return parent.to_path_buf();
        }
    }

    // 3. Default
    dirs_next::home_dir().unwrap_or_default().join(".rsclaw")
}

/// Gateway PID file path: `$base_dir/var/run/gateway.pid`
pub fn pid_file() -> PathBuf {
    base_dir().join("var/run/gateway.pid")
}

/// Gateway log file path: `$base_dir/var/logs/gateway.log`
pub fn log_file() -> PathBuf {
    base_dir().join("var/logs/gateway.log")
}

/// Cache directory: `$base_dir/var/cache/`
pub fn cache_dir() -> PathBuf {
    base_dir().join("var/cache")
}

/// Load defaults.toml: prefer external file at `$base_dir/defaults.toml`,
/// fallback to the version embedded at compile time.
///
/// This allows production deployments to customize providers, channels,
/// exec safety rules, etc. without recompiling.
pub fn load_defaults_toml() -> String {
    let external = base_dir().join("defaults.toml");
    if let Ok(content) = std::fs::read_to_string(&external) {
        debug!(path = %external.display(), "loaded external defaults.toml");
        content
    } else {
        include_str!("../../defaults.toml").to_owned()
    }
}

/// Expand a leading `~/` in a path string to the user's home directory.
/// Public alias used by `main.rs` for `--base-dir` resolution.
pub fn expand_tilde_path_pub(p: &str) -> PathBuf {
    expand_tilde_path(p)
}

fn expand_tilde_path(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        dirs_next::home_dir().unwrap_or_default().join(rest)
    } else if p == "~" {
        dirs_next::home_dir().unwrap_or_default()
    } else {
        PathBuf::from(p)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn expand_known_var() {
        // SAFETY: single-threaded test, no concurrent env access
        unsafe { std::env::set_var("TEST_API_KEY_RSCLAW", "sk-test-123") };
        let result = expand_env_vars(r#"{"apiKey": "${TEST_API_KEY_RSCLAW}"}"#);
        assert!(result.contains("sk-test-123"), "got: {result}");
    }

    #[test]
    fn expand_missing_var_leaves_verbatim() {
        let input = r#"{"apiKey": "${RSCLAW_NONEXISTENT_XYZ}"}"#;
        let result = expand_env_vars(input);
        assert!(
            result.contains("${RSCLAW_NONEXISTENT_XYZ}"),
            "got: {result}"
        );
    }

    #[test]
    fn include_directive_loads_nested_file() {
        let dir = tempfile::tempdir().unwrap();

        // Write sub-file
        let sub_path = dir.path().join("agents.json5");
        std::fs::write(&sub_path, r#"{ list: [{ id: "main", default: true }] }"#).unwrap();

        // Write main config that $includes sub-file
        let main_path = dir.path().join("openclaw.json5");
        std::fs::write(
            &main_path,
            r#"{ agents: { "$include": "./agents.json5" } }"#,
        )
        .unwrap();

        let cfg = load_json5(&main_path).unwrap();
        let agents = cfg.agents.expect("agents should be present");
        let list = agents.list.expect("agents.list should be present");
        assert_eq!(list[0].id, "main");
    }
}
