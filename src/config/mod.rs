//! Config loading entry point.
//!
//! Priority order (first existing file wins):
//!   ~/.rsclaw/rsclaw.json5   ← rsclaw-native JSON5  (highest)
//!   ~/.openclaw/openclaw.json  ← openclaw compat, parsed as JSON5
//!   ~/.openclaw/openclaw.json5 ← openclaw compat JSON5
//!   (env overrides RSCLAW_CONFIG_PATH / OPENCLAW_CONFIG_PATH always win)
//!
//! Loading pipeline:
//!   detect config source
//!     → load + env-expand + $include resolve (JSON5)
//!       → schema deserialize (deny_unknown_fields)
//!         → cross-field validate
//!           → into_runtime (unified RuntimeConfig)

pub mod loader;
pub mod runtime;
pub mod schema;
pub mod secrets;
pub mod validator;

use anyhow::{Context, Result};
use loader::{detect_config_path, load_json5};
use runtime::{IntoRuntime, RuntimeConfig};

/// Detect, load, validate, and return the unified RuntimeConfig.
///
/// Panics-free: all errors are returned as `Err`.
pub fn load() -> Result<RuntimeConfig> {
    let path = detect_config_path().with_context(
        || "no config file found. Run `rsclaw setup` to create one, or set RSCLAW_CONFIG_PATH.",
    )?;

    tracing::info!(path = %path.display(), "loading config");

    load_from_path(&path)
}

/// Like `load()` but without INFO-level log (for CLI status commands).
pub fn load_quiet() -> Result<RuntimeConfig> {
    let path = detect_config_path().with_context(
        || "no config file found. Run `rsclaw setup` to create one, or set RSCLAW_CONFIG_PATH.",
    )?;

    load_from_path(&path)
}

fn load_from_path(path: &std::path::Path) -> Result<RuntimeConfig> {

    let runtime = load_json5(&path)
        .with_context(|| format!("failed to load config: {}", path.display()))?
        .into_runtime()?;

    validator::validate(&runtime)?;

    // Apply instance-isolation overrides set by --dev / --profile (AGENTS.md §26).
    let runtime = apply_env_overrides(runtime);

    Ok(runtime)
}

/// Apply environment variable overrides for multi-instance isolation.
/// Called after schema validation so overrides bypass schema constraints.
fn apply_env_overrides(mut cfg: RuntimeConfig) -> RuntimeConfig {
    if let Ok(port_str) = std::env::var("RSCLAW_PORT")
        && let Ok(port) = port_str.parse::<u16>()
    {
        cfg.gateway.port = port;
    }
    cfg
}

/// Load config from an explicit path (for tests and the `doctor` command).
pub fn load_from(path: std::path::PathBuf) -> Result<RuntimeConfig> {
    let runtime = load_json5(&path)?.into_runtime()?;
    validator::validate(&runtime)?;
    Ok(runtime)
}

/// Resolve the proxy URL from env var (highest priority) or config.
/// Returns None if no proxy is configured.
pub fn resolve_proxy(config: &RuntimeConfig) -> Option<String> {
    // RSCLAW_PROXY env var takes priority.
    if let Ok(p) = std::env::var("RSCLAW_PROXY") {
        let p = p.trim().to_owned();
        if !p.is_empty() { return Some(p); }
    }
    // Fallback to config file.
    config.raw.gateway.as_ref()
        .and_then(|g| g.proxy.as_ref())
        .filter(|p| !p.is_empty())
        .cloned()
}

/// Check if a URL should bypass the proxy (localhost, 127.0.0.1, etc.).
pub fn should_bypass_proxy(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.contains("://localhost") || lower.contains("://127.0.0.1")
        || lower.contains("://[::1]") || lower.contains("://0.0.0.0")
}

/// Apply proxy settings to standard environment variables so all reqwest clients
/// (including those in channels) automatically use the proxy.
/// Must be called early in gateway startup, before any HTTP clients are created.
/// Sets HTTP_PROXY, HTTPS_PROXY, and NO_PROXY (to skip localhost).
pub fn apply_proxy_env(config: &RuntimeConfig) {
    if let Some(proxy_url) = resolve_proxy(config) {
        // SAFETY: called once at startup before spawning threads.
        unsafe {
            std::env::set_var("HTTP_PROXY", &proxy_url);
            std::env::set_var("HTTPS_PROXY", &proxy_url);
        }
        // Don't proxy localhost (ollama, local services).
        let no_proxy = std::env::var("NO_PROXY").unwrap_or_default();
        if !no_proxy.contains("localhost") {
            let new_no_proxy = if no_proxy.is_empty() {
                "localhost,127.0.0.1,::1".to_owned()
            } else {
                format!("{no_proxy},localhost,127.0.0.1,::1")
            };
            unsafe { std::env::set_var("NO_PROXY", &new_no_proxy); }
        }
        tracing::info!(proxy = %proxy_url, "global HTTP proxy configured via env");
    }
}
