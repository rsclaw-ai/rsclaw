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
