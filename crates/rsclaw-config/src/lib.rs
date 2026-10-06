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
//!       → schema deserialize (tolerates unknown fields on most structs)
//!         → cross-field validate
//!           → into_runtime (unified RuntimeConfig)

pub mod catalog;
pub mod env_file;
pub mod env_resolution;
pub mod loader;
pub mod runtime;
pub mod schema;
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

/// Fingerprint of a config file on disk: (path, mtime, len).
type ConfigFingerprint = (std::path::PathBuf, std::time::SystemTime, u64);

/// Process-wide cache for [`load_cached`].
static CONFIG_CACHE: std::sync::Mutex<Option<(ConfigFingerprint, RuntimeConfig)>> =
    std::sync::Mutex::new(None);

/// Like [`load()`], but reuses the last parsed config while the config file's
/// path, mtime and length are unchanged, returning a clone.
///
/// Intended for per-call hot paths (KB search, OCR, transcription) that only
/// need a fresh view of the file. Editing the file changes its mtime/len, so
/// edits still take effect on the next call. Note that edits to `$include`d
/// sub-files alone do not invalidate the cache. Any stat failure falls back to
/// a plain [`load()`]; parse/validation errors are returned like [`load()`].
/// Env reconcile is Once-guarded inside the loader, so a cache miss never
/// repeats its side effects.
pub fn load_cached() -> Result<RuntimeConfig> {
    let Some(path) = detect_config_path() else {
        return load();
    };
    load_cached_at(&path)
}

fn load_cached_at(path: &std::path::Path) -> Result<RuntimeConfig> {
    let Ok(meta) = std::fs::metadata(path) else {
        return load_from_path_logged(path);
    };
    let Ok(mtime) = meta.modified() else {
        return load_from_path_logged(path);
    };
    let key: ConfigFingerprint = (path.to_path_buf(), mtime, meta.len());

    {
        let guard = CONFIG_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached_key, cfg)) = guard.as_ref()
            && *cached_key == key
        {
            return Ok(cfg.clone());
        }
    }

    let result = load_from_path_logged(path);
    let mut guard = CONFIG_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match &result {
        Ok(cfg) => *guard = Some((key, cfg.clone())),
        Err(_) => *guard = None,
    }
    result
}

fn load_from_path_logged(path: &std::path::Path) -> Result<RuntimeConfig> {
    tracing::info!(path = %path.display(), "loading config");
    load_from_path(path)
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
        if !p.is_empty() {
            return Some(p);
        }
    }
    // Fallback to config file.
    config
        .raw
        .gateway
        .as_ref()
        .and_then(|g| g.proxy.as_ref())
        .filter(|p| !p.is_empty())
        .cloned()
}

/// Resolve proxy allow list from env or config.
fn resolve_proxy_allow(config: &RuntimeConfig) -> Option<String> {
    if let Ok(v) = std::env::var("RSCLAW_PROXY_ALLOW") {
        if !v.trim().is_empty() {
            return Some(v.trim().to_owned());
        }
    }
    config
        .raw
        .gateway
        .as_ref()
        .and_then(|g| g.proxy_allow.as_ref())
        .filter(|v| !v.is_empty())
        .cloned()
}

/// Resolve proxy deny list from env or config.
fn resolve_proxy_deny(config: &RuntimeConfig) -> Option<String> {
    if let Ok(v) = std::env::var("RSCLAW_PROXY_DENY") {
        if !v.trim().is_empty() {
            return Some(v.trim().to_owned());
        }
    }
    config
        .raw
        .gateway
        .as_ref()
        .and_then(|g| g.proxy_deny.as_ref())
        .filter(|v| !v.is_empty())
        .cloned()
}

/// Check if a host matches a pattern (supports wildcards like *.openai.com).
fn host_matches_pattern(host: &str, pattern: &str) -> bool {
    let host = host.to_lowercase();
    let pattern = pattern.trim().to_lowercase();
    if pattern == "*" {
        return true;
    }
    if pattern.starts_with("*.") {
        let suffix = &pattern[1..]; // ".openai.com"
        host.ends_with(suffix) || host == pattern[2..]
    } else {
        host == pattern || host.ends_with(&format!(".{pattern}"))
    }
}

/// Check if a host matches any pattern in a comma-separated list.
fn host_matches_any(host: &str, patterns: &str) -> bool {
    patterns
        .split(',')
        .any(|p| host_matches_pattern(host, p.trim()))
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
        unsafe {
            std::env::set_var("NO_PROXY", &deny_list);
        }
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

/// Detect the system timezone.
///
/// Order: an explicit IANA name in the `TZ` env var (user override), then
/// the OS-configured zone via `iana-time-zone` (`/etc/localtime` on Unix,
/// the registry/ICU on Windows), then UTC. A UTC-offset heuristic is NOT
/// used: an offset does not identify a zone (it cannot tell Europe/London
/// from UTC, or pick the right DST rules).
///
/// Shared helper used by heartbeat and cron modules to avoid duplication.
pub fn system_tz() -> chrono_tz::Tz {
    if let Ok(tz_name) = std::env::var("TZ") {
        // POSIX allows a leading ':' ("TZ=:Asia/Shanghai").
        if let Ok(tz) = tz_name.trim_start_matches(':').parse() {
            return tz;
        }
    }
    // Detected once per process: the lookup is a syscall / FFI call and the
    // failure warning should not repeat on every cron computation.
    static DETECTED: std::sync::OnceLock<chrono_tz::Tz> = std::sync::OnceLock::new();
    *DETECTED.get_or_init(|| match iana_time_zone::get_timezone() {
        Ok(name) => name.parse().unwrap_or_else(|_| {
            tracing::warn!(
                tz = %name,
                "system timezone is not a known IANA name, using UTC. Set TZ env var for accuracy."
            );
            chrono_tz::UTC
        }),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "cannot detect system timezone, using UTC. Set TZ env var for accuracy."
            );
            chrono_tz::UTC
        }
    })
}

pub mod config_json;

pub mod live_config;

#[cfg(test)]
mod load_cached_tests {
    use super::*;

    fn write_with_mtime(path: &std::path::Path, content: &str, mtime: std::time::SystemTime) {
        std::fs::write(path, content).expect("write config");
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open config");
        f.set_modified(mtime).expect("set mtime");
    }

    #[test]
    fn load_cached_invalidates_on_mtime_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rsclaw.json5");
        let t0 = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let t1 = t0 + std::time::Duration::from_secs(60);

        write_with_mtime(&path, "{ gateway: { port: 18001 } }", t0);
        let first = load_cached_at(&path).expect("first load");
        assert_eq!(first.gateway.port, 18001);

        // Same len + same mtime: served from cache even though bytes changed.
        write_with_mtime(&path, "{ gateway: { port: 18002 } }", t0);
        let cached = load_cached_at(&path).expect("cached load");
        assert_eq!(cached.gateway.port, 18001);

        // Same len, newer mtime: cache invalidated and file re-parsed.
        write_with_mtime(&path, "{ gateway: { port: 18002 } }", t1);
        let reloaded = load_cached_at(&path).expect("reload");
        assert_eq!(reloaded.gateway.port, 18002);
    }
}
