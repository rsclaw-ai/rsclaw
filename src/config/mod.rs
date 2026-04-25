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
    tracing::info!(path = %path.display(), "CDP: loading config file");

    let raw_config = load_json5(&path)
        .with_context(|| format!("failed to load config: {}", path.display()))?;

    tracing::info!(
        tools_present = raw_config.tools.is_some(),
        web_browser_present = raw_config.tools.as_ref().and_then(|t| t.web_browser.as_ref()).is_some(),
        web_browser_config = ?raw_config.tools.as_ref().and_then(|t| t.web_browser.as_ref()),
        "CDP: raw config parsed"
    );

    let runtime = raw_config.into_runtime()?;
    validator::validate(&runtime)?;

    tracing::info!(
        tools_present = runtime.ext.tools.is_some(),
        web_browser_present = runtime.ext.tools.as_ref().and_then(|t| t.web_browser.as_ref()).is_some(),
        web_browser_config = ?runtime.ext.tools.as_ref().and_then(|t| t.web_browser.as_ref()),
        "CDP: runtime config built"
    );

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

/// Resolve proxy allow list from env or config.
fn resolve_proxy_allow(config: &RuntimeConfig) -> Option<String> {
    if let Ok(v) = std::env::var("RSCLAW_PROXY_ALLOW") {
        if !v.trim().is_empty() { return Some(v.trim().to_owned()); }
    }
    config.raw.gateway.as_ref()
        .and_then(|g| g.proxy_allow.as_ref())
        .filter(|v| !v.is_empty())
        .cloned()
}

/// Resolve proxy deny list from env or config.
fn resolve_proxy_deny(config: &RuntimeConfig) -> Option<String> {
    if let Ok(v) = std::env::var("RSCLAW_PROXY_DENY") {
        if !v.trim().is_empty() { return Some(v.trim().to_owned()); }
    }
    config.raw.gateway.as_ref()
        .and_then(|g| g.proxy_deny.as_ref())
        .filter(|v| !v.is_empty())
        .cloned()
}

/// Check if a host matches a pattern (supports wildcards like *.openai.com).
fn host_matches_pattern(host: &str, pattern: &str) -> bool {
    let host = host.to_lowercase();
    let pattern = pattern.trim().to_lowercase();
    if pattern == "*" { return true; }
    if pattern.starts_with("*.") {
        let suffix = &pattern[1..]; // ".openai.com"
        host.ends_with(suffix) || host == pattern[2..]
    } else {
        host == pattern || host.ends_with(&format!(".{pattern}"))
    }
}

/// Check if a host matches any pattern in a comma-separated list.
fn host_matches_any(host: &str, patterns: &str) -> bool {
    patterns.split(',').any(|p| host_matches_pattern(host, p.trim()))
}

/// Apply proxy settings. Uses HTTP_PROXY/HTTPS_PROXY env vars for simple cases,
/// or reqwest::Proxy::custom for allow/deny lists.
/// Must be called early in gateway startup before HTTP clients are created.
pub fn apply_proxy_env(config: &RuntimeConfig) {
    let proxy_url = match resolve_proxy(config) {
        Some(u) => u,
        None => return,
    };

    let allow = resolve_proxy_allow(config);
    let deny = resolve_proxy_deny(config);

    // Build deny list: always include localhost + user deny list.
    let mut deny_list = "localhost,127.0.0.1,::1".to_owned();
    if let Some(ref d) = deny {
        deny_list = format!("{deny_list},{d}");
    }

    if allow.is_none() || allow.as_deref() == Some("*") {
        // Simple mode: proxy everything except deny list → use env vars.
        // SAFETY: called before tokio runtime starts, single-threaded at this point
        unsafe {
            std::env::set_var("HTTP_PROXY", &proxy_url);
            std::env::set_var("HTTPS_PROXY", &proxy_url);
            std::env::set_var("NO_PROXY", &deny_list);
        }
        tracing::info!(proxy = %proxy_url, deny = %deny_list, "global proxy configured (all domains)");
    } else {
        // Allow mode: only proxy matching domains.
        // Do NOT set HTTP_PROXY env var — that would proxy ALL requests.
        // Instead store the config globally. Channels that create their own
        // reqwest::Client will NOT use the proxy (which is correct — only
        // allowed domains should). The proxy is applied via build_proxy_client().
        //
        // For channels that DO need the proxy (e.g. wechat CDN upload),
        // they should use build_proxy_client() or we inject the proxy at
        // the point of use.
        unsafe { std::env::set_var("NO_PROXY", &deny_list); }
        PROXY_ALLOW.get_or_init(|| allow.clone().unwrap_or_default());
        PROXY_DENY.get_or_init(|| deny_list.clone());
        PROXY_URL.get_or_init(|| proxy_url.clone());
        tracing::info!(proxy = %proxy_url, allow = ?allow, deny = %deny_list, "global proxy configured (allow-list mode, selective)");
    }
}

// TODO: OnceLock means proxy settings cannot be changed at runtime after
// initial configuration. If runtime proxy reconfiguration is needed,
// migrate to ArcSwap or a Mutex-guarded config cell.
static PROXY_ALLOW: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static PROXY_DENY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static PROXY_URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Build a reqwest::Client that respects the proxy allow/deny lists.
/// If an allow list is configured, only matching domains use the proxy.
pub fn build_proxy_client() -> reqwest::ClientBuilder {
    let mut builder = reqwest::Client::builder();

    let allow = PROXY_ALLOW.get().map(|s| s.as_str()).unwrap_or("");
    let proxy_url = PROXY_URL.get().map(|s| s.as_str()).unwrap_or("");

    let deny = PROXY_DENY.get().map(|s| s.as_str()).unwrap_or("");

    if !proxy_url.is_empty() && !allow.is_empty() && allow != "*" {
        // Custom proxy: only route matching hosts through proxy.
        let allow_owned = allow.to_owned();
        let deny_owned = deny.to_owned();
        let url_owned = proxy_url.to_owned();
        let proxy = reqwest::Proxy::custom(move |url| {
            let host = url.host_str().unwrap_or("");
            // Deny list takes priority over allow list.
            if !deny_owned.is_empty() && host_matches_any(host, &deny_owned) {
                return None;
            }
            if host_matches_any(host, &allow_owned) {
                Some(url_owned.clone())
            } else {
                None
            }
        });
        builder = builder.proxy(proxy);
    }
    builder
}

/// Detect the system timezone from the `TZ` env var or the local UTC offset.
///
/// Shared helper used by heartbeat and cron modules to avoid duplication.
pub fn system_tz() -> chrono_tz::Tz {
    // Try TZ env var first (works on Linux/macOS with IANA names like "Asia/Shanghai")
    if let Ok(tz_name) = std::env::var("TZ") {
        if let Ok(tz) = tz_name.parse() {
            return tz;
        }
    }
    // Fall back to detecting system offset and mapping to a timezone
    let local_offset = chrono::Local::now().offset().local_minus_utc();
    match local_offset {
        25200 => chrono_tz::Asia::Bangkok,     // +07:00
        28800 => chrono_tz::Asia::Shanghai,    // +08:00
        32400 => chrono_tz::Asia::Tokyo,       // +09:00
        36000 => chrono_tz::Australia::Sydney,  // +10:00
        -18000 => chrono_tz::US::Eastern,      // -05:00
        -21600 => chrono_tz::US::Central,      // -06:00
        -25200 => chrono_tz::US::Mountain,     // -07:00
        -28800 => chrono_tz::US::Pacific,      // -08:00
        0 => chrono_tz::UTC,
        _ => {
            tracing::warn!(offset_secs = local_offset, "unknown system timezone offset, using UTC. Set TZ env var for accuracy.");
            chrono_tz::UTC
        }
    }
}
