//! Config file loading: JSON5 parsing, `${VAR}` expansion, `$include`
//! resolution.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

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
                // Promote from debug to warn: unresolved placeholders
                // silently survive into runtime and surface as cryptic
                // upstream errors (e.g. an apiKey of literal
                // "${RSCLAW_API_KEY}" gets sent as a Bearer token →
                // "invalid api key" 401 from the provider). The user
                // needs to see this at gateway boot, not buried under
                // debug-level traffic.
                tracing::warn!(
                    var,
                    "env var referenced in config is not set; placeholder left verbatim"
                );
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

    // 1a. Bootstrap env: snapshot shell, load .env, reconcile.
    // Must run BEFORE expand_env_vars so process env reflects the
    // final resolved state. Best-effort — a write/read failure on
    // .env logs but does not abort gateway startup; downstream
    // expand_env_vars will warn on any var that remains unresolved.
    if std::env::var_os("RSCLAW_NO_ENV_SYNC").is_none() {
        if let Err(e) = super::env_resolution::reconcile(&raw, base_dir) {
            tracing::warn!(error = %e, "env reconcile failed (continuing with current process env)");
        }
    }

    // 1b. Expand env vars before any parsing.
    let expanded = expand_env_vars(&raw);

    // 2. Parse into a generic JSON value so we can handle $include.
    let mut value: serde_json::Value = json5::from_str(&expanded)
        .with_context(|| format!("JSON5 parse error in {}", path.display()))?;

    // 3. Resolve $include directives recursively.
    resolve_includes(&mut value, base_dir, 0)?;

    // 3b. Migrate legacy top-level credential fields → accounts.default.*
    //     so the startup code only reads `accounts` (no dual-path).
    migrate_channel_legacy_fields(&mut value);

    // 3c. Migrate the few channels that historically read credentials from
    //     env vars (whatsapp/line/zalo) into accounts.default.* too, so the
    //     env-var-only configs keep working after the dual-path read removal.
    migrate_channel_env_fallback(&mut value);

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
                // H5: reject path traversal (../, symlinks, etc.)
                let canonical = include_path.canonicalize().with_context(|| {
                    format!("$include path does not exist: {}", include_path.display())
                })?;
                let canonical_base = base_dir.canonicalize().with_context(|| {
                    format!("$include base dir does not exist: {}", base_dir.display())
                })?;
                if !canonical.starts_with(&canonical_base) {
                    anyhow::bail!(
                        "$include path escapes base directory: {}",
                        include_path.display()
                    );
                }
                let included = load_include_file(&canonical, depth + 1)?;
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
    base_dir().join("var").join("run").join("gateway.pid")
}

/// Gateway log file path: `$base_dir/var/logs/gateway.log`
pub fn log_file() -> PathBuf {
    base_dir().join("var").join("logs").join("gateway.log")
}

/// Look up site-rule files that match a URL's host.
///
/// Returns relative paths under `tools/web_browser/site-rules/`. Both
/// layouts are checked:
///   * `<host>.md` — flat (legacy zh sites)
///   * `<host_root>/*.md` — nested (browser-harness imports). `host_root` is
///     the part of the host before the first dot, e.g. `reddit` for
///     `www.reddit.com`.
///
/// Surfaced by `web_fetch` and `web_browser action=open` tool results so
/// the agent gets a hard pointer to read the rule before acting — the
/// prompt-only mention buried in the tool description was being ignored
/// on hosts where the agent thought it knew what to do.
pub fn applicable_site_rules(url: &str) -> Vec<String> {
    // Extract host without pulling in the `url` crate. Skip scheme via
    // `://` split, then take everything up to the first /?#: separator.
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host_with_port = after_scheme
        .find(|c: char| matches!(c, '/' | '?' | '#'))
        .map(|i| &after_scheme[..i])
        .unwrap_or(after_scheme);
    // Strip optional port (e.g. example.com:8080).
    let host = host_with_port
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host_with_port);
    if host.is_empty() {
        return Vec::new();
    }
    let host = host.strip_prefix("www.").unwrap_or(host).to_owned();

    let dir = base_dir()
        .join("tools")
        .join("web_browser")
        .join("site-rules");
    if !dir.is_dir() {
        return Vec::new();
    }

    let mut rules = Vec::new();

    let flat = dir.join(format!("{host}.md"));
    if flat.is_file() {
        rules.push(format!("site-rules/{host}.md"));
    }

    // Build a list of candidate directory names to try, ordered by
    // specificity. For `api.stackexchange.com` we want both:
    //   - `api`              (matches a hypothetical `site-rules/api/`)
    //   - `stackexchange`    (matches the actual `site-rules/stackexchange/`)
    // Plain `stackexchange.com` collapses to a single candidate.
    //
    // Previously only the leftmost label was tried, so subdomains like
    // `api.stackexchange.com`, `m.youtube.com`, or `cdn.shopify.com` never
    // resolved to the registrable-host rule directory and the agent saw
    // no rule at all.
    let mut candidates: Vec<String> = Vec::new();
    let labels: Vec<&str> = host.split('.').filter(|s| !s.is_empty()).collect();
    if let Some(first) = labels.first() {
        candidates.push((*first).to_owned());
    }
    if labels.len() >= 2 {
        let second_to_last = labels[labels.len() - 2];
        if !candidates.iter().any(|c| c == second_to_last) {
            candidates.push(second_to_last.to_owned());
        }
    }

    for cand in &candidates {
        let nested = dir.join(cand);
        if !nested.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&nested) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().is_some_and(|e| e == "md") {
                let name = p
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                if !name.is_empty() {
                    rules.push(format!("site-rules/{cand}/{name}"));
                }
            }
        }
    }

    rules
}

/// Read concatenated body of every rule returned by
/// [`applicable_site_rules`] for `url`.
///
/// Each rule body is preceded by a `# === path ===` separator line so the
/// agent can see which file each section came from. Returns `None` if no
/// rule applies.
///
/// Inlined directly into web_fetch/web_browser tool results so the agent
/// has the working approach at hand without needing a separate read_file
/// round-trip — the previous "hint that points at file paths" design was
/// being ignored.
pub fn applicable_site_rules_body(url: &str) -> Option<String> {
    let paths = applicable_site_rules(url);
    if paths.is_empty() {
        return None;
    }
    let dir = base_dir().join("tools").join("web_browser");
    let mut out = String::new();
    for rel in &paths {
        let p = dir.join(rel);
        if let Ok(body) = std::fs::read_to_string(&p) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("# === ");
            out.push_str(rel);
            out.push_str(" ===\n");
            out.push_str(body.trim_end());
            out.push('\n');
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Cache directory: `$base_dir/var/cache/`
pub fn cache_dir() -> PathBuf {
    base_dir().join("var").join("cache")
}

/// Defaults bundled into this binary. Unlike [`load_defaults_toml`], this
/// never reads the user-editable `$base_dir/defaults.toml`.
pub fn embedded_defaults_toml() -> &'static str {
    include_str!("../../../defaults.toml")
}

/// Load defaults.toml: prefer external file at `$base_dir/defaults.toml`,
/// fallback to the version embedded at compile time.
///
/// This allows production deployments to customize providers, channels,
/// exec safety rules, etc. without recompiling.
pub fn load_defaults_toml() -> String {
    let external = base_dir().join("defaults.toml");
    if external.exists()
        && let Err(e) = ensure_defaults_toml_up_to_date(&external)
    {
        tracing::warn!(path = %external.display(), error = %e, "failed to upgrade defaults.toml; using existing file");
    }
    if let Ok(content) = std::fs::read_to_string(&external) {
        debug!(path = %external.display(), "loaded external defaults.toml");
        content
    } else {
        embedded_defaults_toml().to_owned()
    }
}

fn ensure_defaults_toml_up_to_date(path: &Path) -> Result<()> {
    let user = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read defaults.toml: {}", path.display()))?;
    let Some(merged) = merge_defaults_toml(&user, embedded_defaults_toml()) else {
        return Ok(());
    };

    backup_defaults_before_upgrade(path);
    std::fs::write(path, merged)
        .with_context(|| format!("failed to write upgraded defaults.toml: {}", path.display()))?;
    debug!(path = %path.display(), "upgraded defaults.toml from embedded defaults");
    Ok(())
}

/// Merge a remotely-fetched `defaults.toml` into the on-disk file.
///
/// Reuses the same version-gated merge as the embedded upgrade path:
/// `remote_raw` is treated as the "shipped" source, so it only takes
/// effect when its `defaults_version` is newer than the user's current
/// file — shipped entries are refreshed, user-added entries preserved,
/// and the old file backed up first. No HTTP here: the caller (gateway
/// startup) fetches the bytes and hands them in, so a fetch failure never
/// reaches this function — the on-disk/local file is simply left intact.
///
/// Returns `Ok(true)` when the file was updated, `Ok(false)` when the
/// remote was not newer (no-op), and `Err` only when the remote payload
/// is not valid `defaults.toml` (unparseable / missing `defaults_version`)
/// or the write fails — callers should treat `Err` as "keep local".
pub fn merge_remote_defaults(remote_raw: &str) -> Result<bool> {
    // Validate the remote is a well-formed defaults.toml carrying a
    // version BEFORE touching the local file, so a corrupt/HTML error
    // page served at the URL can't clobber anything.
    let remote: DefaultsIndex =
        toml::from_str(remote_raw).context("remote defaults.toml is not valid TOML")?;
    if remote
        .meta
        .as_ref()
        .and_then(|m| m.defaults_version.as_deref())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .is_none()
    {
        anyhow::bail!("remote defaults.toml has no defaults_version");
    }

    let path = base_dir().join("defaults.toml");
    let local =
        std::fs::read_to_string(&path).unwrap_or_else(|_| embedded_defaults_toml().to_owned());

    let Some(merged) = merge_defaults_toml(&local, remote_raw) else {
        return Ok(false);
    };

    if path.exists() {
        backup_defaults_before_upgrade(&path);
    }
    std::fs::write(&path, merged)
        .with_context(|| format!("failed to write remote defaults.toml: {}", path.display()))?;
    tracing::info!(path = %path.display(), "applied remote defaults.toml update");
    Ok(true)
}

fn backup_defaults_before_upgrade(path: &Path) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup = path.with_extension(format!("toml.bak.{ts}"));
    let _ = std::fs::copy(path, backup);
}

#[derive(Default, serde::Deserialize)]
struct DefaultsMeta {
    #[serde(default)]
    defaults_version: Option<String>,
}

#[derive(Default, serde::Deserialize)]
struct NamedDefaultsEntry {
    name: String,
}

#[derive(Default, serde::Deserialize)]
struct DefaultsIndex {
    #[serde(default)]
    meta: Option<DefaultsMeta>,
    #[serde(default)]
    providers: Vec<NamedDefaultsEntry>,
    #[serde(default)]
    channels: Vec<NamedDefaultsEntry>,
    #[serde(default)]
    search_engines: Vec<NamedDefaultsEntry>,
}

fn merge_defaults_toml(user_raw: &str, builtin_raw: &str) -> Option<String> {
    let user: DefaultsIndex = toml::from_str(user_raw).ok()?;
    let builtin: DefaultsIndex = toml::from_str(builtin_raw).ok()?;
    let builtin_version = builtin.meta.as_ref()?.defaults_version.as_deref()?.trim();
    if builtin_version.is_empty() || !defaults_version_is_legacy(&user, builtin_version) {
        return None;
    }

    // Regenerate the file from the shipped catalog so every shipped entry
    // carries the latest fields (decorative + functional), then preserve the
    // user's *own* entries — those in their file but not shipped — by appending
    // their original blocks verbatim. `builtin_raw` already carries the bumped
    // `defaults_version`, so the rewritten file won't re-upgrade next launch.
    // The caller backs the old file up first, so a user who hand-edited a
    // shipped entry can recover from the `.bak.*` copy.
    let user_blocks = builtin_array_blocks(user_raw);
    let mut merged = builtin_raw.trim_end().to_owned();

    for (table, user_entries) in [
        ("providers", &user.providers),
        ("channels", &user.channels),
        ("search_engines", &user.search_engines),
    ] {
        let shipped = names_for(builtin_entries_for(&builtin, table));
        for entry in user_entries {
            if shipped.contains(&entry.name) {
                continue;
            }
            if let Some(block) = user_blocks.get(&(table.to_owned(), entry.name.clone())) {
                merged.push('\n');
                merged.push('\n');
                merged.push_str("# Preserved user-defined entry (kept across defaults upgrade).\n");
                merged.push_str(block.trim_end());
                merged.push('\n');
            }
        }
    }

    if !merged.ends_with('\n') {
        merged.push('\n');
    }

    (merged != user_raw).then_some(merged)
}

fn defaults_version_is_legacy(user: &DefaultsIndex, builtin_version: &str) -> bool {
    let Some(user_version) = user
        .meta
        .as_ref()
        .and_then(|m| m.defaults_version.as_deref())
        .map(str::trim)
    else {
        return true;
    };
    if user_version.is_empty() {
        return true;
    }
    version_parts(user_version)
        .zip(version_parts(builtin_version))
        .is_none_or(|(user, builtin)| compare_version_parts(&user, &builtin).is_lt())
}

fn version_parts(raw: &str) -> Option<Vec<u64>> {
    let mut parts = Vec::new();
    for part in raw.split(|c: char| !c.is_ascii_digit()) {
        if part.is_empty() {
            continue;
        }
        parts.push(part.parse().ok()?);
    }
    (!parts.is_empty()).then_some(parts)
}

fn compare_version_parts(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    let len = a.len().max(b.len());
    for i in 0..len {
        let left = a.get(i).copied().unwrap_or(0);
        let right = b.get(i).copied().unwrap_or(0);
        match left.cmp(&right) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

fn names_for(entries: &[NamedDefaultsEntry]) -> HashSet<String> {
    entries.iter().map(|e| e.name.clone()).collect()
}

fn builtin_entries_for<'a>(builtin: &'a DefaultsIndex, table: &str) -> &'a [NamedDefaultsEntry] {
    match table {
        "providers" => &builtin.providers,
        "channels" => &builtin.channels,
        "search_engines" => &builtin.search_engines,
        _ => &[],
    }
}

fn builtin_array_blocks(raw: &str) -> HashMap<(String, String), String> {
    let mut out = HashMap::new();
    let mut current_table: Option<String> = None;
    let mut current = String::new();

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("[[") && trimmed.ends_with("]]") {
            store_defaults_block(&mut out, current_table.take(), &current);
            current.clear();
            current_table = Some(
                trimmed
                    .trim_start_matches("[[")
                    .trim_end_matches("]]")
                    .trim()
                    .to_owned(),
            );
        } else if trimmed.starts_with('[') {
            store_defaults_block(&mut out, current_table.take(), &current);
            current.clear();
        }

        if current_table.is_some() {
            current.push_str(line);
            current.push('\n');
        }
    }
    store_defaults_block(&mut out, current_table, &current);
    out
}

fn store_defaults_block(
    out: &mut HashMap<(String, String), String>,
    table: Option<String>,
    block: &str,
) {
    let Some(table) = table else {
        return;
    };
    if !matches!(table.as_str(), "providers" | "channels" | "search_engines") {
        return;
    }
    let Some(name) = defaults_block_name(block) else {
        return;
    };
    out.insert((table, name), block.to_owned());
}

fn defaults_block_name(block: &str) -> Option<String> {
    block.lines().find_map(|line| {
        let trimmed = line.trim();
        let value = trimmed.strip_prefix("name")?.trim_start();
        let value = value.strip_prefix('=')?.trim();
        value
            .strip_prefix('"')?
            .split_once('"')
            .map(|(name, _)| name.to_owned())
    })
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
// Legacy channel field migration
// ---------------------------------------------------------------------------

/// Migrate top-level credential fields in each channel config to
/// `accounts.default.<field>` format.
///
/// Before v0.6, channels were configured with flat top-level fields:
///   `channels.feishu.appId`, `channels.wechat.botToken`, etc.
/// Startup code then read both the top-level field AND `accounts.<name>`,
/// deduplicating entries. This migration moves top-level credentials into
/// `accounts.default.*` so startup code can read only `accounts`.
///
/// Operates at the raw JSON Value level (before typed deserialization)
/// so the Config struct never sees the old format after migration.
fn migrate_channel_legacy_fields(root: &mut serde_json::Value) {
    /// (channel_name, [legacy_field1, legacy_field2, ...])
    const LEGACY_FIELDS: &[(&str, &[&str])] = &[
        ("telegram", &["botToken", "tokenFile"]),
        ("discord", &["token"]),
        ("slack", &["botToken", "appToken"]),
        ("signal", &["phone"]),
        ("wechat", &["botToken"]),
        ("feishu", &["appId", "appSecret", "brand"]),
        ("dingtalk", &["appKey", "appSecret", "robotCode"]),
        ("qq", &["appId", "appSecret"]),
        ("wecom", &["botId", "secret", "wsUrl"]),
        ("line", &["channelAccessToken"]),
        ("zalo", &["accessToken"]),
        ("matrix", &["homeserver", "accessToken", "userId"]),
    ];

    let Some(channels) = root.get_mut("channels") else {
        return;
    };
    let Some(channels_map) = channels.as_object_mut() else {
        return;
    };

    for &(ch_name, fields) in LEGACY_FIELDS {
        let Some(ch) = channels_map.get_mut(ch_name) else {
            continue;
        };
        let Some(ch_obj) = ch.as_object_mut() else {
            continue;
        };

        // Does this channel have any legacy top-level credential fields set?
        let has_legacy = fields.iter().any(|f| {
            ch_obj
                .get(*f)
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty())
        });
        if !has_legacy {
            continue;
        }

        // Does it already have `accounts` with at least one non-empty entry?
        let has_accounts = ch_obj
            .get("accounts")
            .and_then(|a| a.as_object())
            .is_some_and(|m| {
                m.values().any(|v| {
                    v.as_object().is_some_and(|o| {
                        fields.iter().any(|f| {
                            o.get(*f)
                                .and_then(|s| s.as_str())
                                .is_some_and(|s| !s.is_empty())
                        })
                    })
                })
            });
        if has_accounts {
            // Accounts already configured — remove top-level fields to avoid
            // confusion (the accounts path is canonical).
            for f in fields {
                ch_obj.remove(*f);
            }
            continue;
        }

        // No accounts yet — migrate top-level fields into accounts.default.
        // Collect values upfront to avoid double-borrow on ch_obj.
        let mut migrated: Vec<(String, serde_json::Value)> = Vec::new();
        for f in fields {
            if let Some(val) = ch_obj.remove(*f) {
                if val.is_string() && val.as_str().is_some_and(|s| !s.is_empty()) {
                    migrated.push((f.to_string(), val));
                }
            }
        }
        if !migrated.is_empty() {
            let default_acct = ch_obj
                .entry("accounts")
                .or_insert_with(|| serde_json::json!({}));
            let default_map = match default_acct.as_object_mut() {
                Some(m) => m,
                None => {
                    tracing::warn!(channel = %ch_name, "accounts field is not an object, skipping migration");
                    continue;
                }
            };
            let default_entry = default_map
                .entry("default")
                .or_insert_with(|| serde_json::json!({}));
            let entry_map = match default_entry.as_object_mut() {
                Some(m) => m,
                None => {
                    tracing::warn!(channel = %ch_name, "account entry is not an object, skipping migration");
                    continue;
                }
            };
            for (key, val) in migrated {
                entry_map.insert(key, val);
            }
        }

        tracing::info!(channel = %ch_name, "migrated top-level fields to accounts.default");
    }
}

/// Migrate env-var credentials into `accounts.default.<field>` for the three
/// channels that historically read them directly (whatsapp/line/zalo).
///
/// `refactor: strip legacy dual-path credential reads` removed the per-channel
/// `std::env::var(...)` fallbacks, so an install that supplied credentials ONLY
/// via env vars would silently lose them after upgrade. This restores that path
/// at config-load time, in the same shape the startup code now expects.
///
/// Semantics match the old `.or_else(env)` fallback:
/// - config field always WINS — env only fills a field that `accounts` lacks;
/// - only applies to a channel block that already EXISTS (i.e. the channel is
///   configured/enabled), since a channel with no config was never started.
fn migrate_channel_env_fallback(root: &mut serde_json::Value) {
    /// (channel_name, [(env_var, account_field)])
    const ENV_FIELDS: &[(&str, &[(&str, &str)])] = &[
        (
            "whatsapp",
            &[
                ("WHATSAPP_PHONE_NUMBER_ID", "phoneNumberId"),
                ("WHATSAPP_ACCESS_TOKEN", "accessToken"),
            ],
        ),
        (
            "line",
            &[("LINE_CHANNEL_ACCESS_TOKEN", "channelAccessToken")],
        ),
        ("zalo", &[("ZALO_ACCESS_TOKEN", "accessToken")]),
    ];

    let Some(channels_map) = root.get_mut("channels").and_then(|c| c.as_object_mut()) else {
        return;
    };

    for &(ch_name, env_fields) in ENV_FIELDS {
        // Only configured (present) channels — a channel with no block was
        // never started, env vars or not.
        let Some(ch_obj) = channels_map
            .get_mut(ch_name)
            .and_then(|c| c.as_object_mut())
        else {
            continue;
        };

        // Already has a non-empty accounts value for any of these fields?
        // Then config is authoritative; leave it alone.
        let has_accounts_cred = ch_obj
            .get("accounts")
            .and_then(|a| a.as_object())
            .is_some_and(|m| {
                m.values().any(|v| {
                    v.as_object().is_some_and(|o| {
                        env_fields.iter().any(|(_, f)| {
                            o.get(*f)
                                .and_then(|s| s.as_str())
                                .is_some_and(|s| !s.is_empty())
                        })
                    })
                })
            });
        if has_accounts_cred {
            continue;
        }

        let from_env: Vec<(String, String)> = env_fields
            .iter()
            .filter_map(|(env, field)| {
                std::env::var(env)
                    .ok()
                    .filter(|v| !v.is_empty())
                    .map(|v| (field.to_string(), v))
            })
            .collect();
        if from_env.is_empty() {
            continue;
        }

        let accounts = ch_obj
            .entry("accounts")
            .or_insert_with(|| serde_json::json!({}));
        let Some(accounts_map) = accounts.as_object_mut() else {
            tracing::warn!(channel = %ch_name, "accounts field is not an object, skipping env migration");
            continue;
        };
        let default_entry = accounts_map
            .entry("default")
            .or_insert_with(|| serde_json::json!({}));
        let Some(entry_map) = default_entry.as_object_mut() else {
            tracing::warn!(channel = %ch_name, "account entry is not an object, skipping env migration");
            continue;
        };
        for (field, val) in from_env {
            entry_map
                .entry(field)
                .or_insert_with(|| serde_json::Value::String(val));
        }
        tracing::info!(channel = %ch_name, "migrated env-var credentials to accounts.default");
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

    #[test]
    fn merge_defaults_refreshes_shipped_entries_and_keeps_user_added() {
        let user = r#"
[meta]
defaults_version = "2026.5.10"

[[providers]]
name = "openai"
label = "My OpenAI"
base_url = "https://proxy.example/v1"

[[providers]]
name = "mycorp"
label = "MyCorp Internal"
base_url = "https://llm.mycorp.internal/v1"

[[channels]]
name = "feishu"
label = "Custom Feishu"
"#;
        let builtin = r#"
[meta]
defaults_version = "2026.5.20"

[[providers]]
name = "rsclaw"
label = "RsClaw (recommended)"
base_url = "https://api.rsclaw.ai/v1/agent"

[[providers]]
name = "openai"
label = "OpenAI"
base_url = "https://api.openai.com/v1"

[[channels]]
name = "feishu"
label = "Feishu / Lark"
"#;

        let merged = merge_defaults_toml(user, builtin).expect("legacy user defaults should merge");

        // Version is bumped to the shipped one.
        assert!(merged.contains("defaults_version = \"2026.5.20\""));
        // Shipped entries are refreshed to the latest built-in definitions.
        assert!(merged.contains("name = \"rsclaw\""));
        assert!(merged.contains("base_url = \"https://api.rsclaw.ai/v1/agent\""));
        assert!(merged.contains("base_url = \"https://api.openai.com/v1\""));
        assert!(merged.contains("label = \"Feishu / Lark\""));
        // A hand-edit to a *shipped* entry is overwritten (recoverable via .bak.*).
        assert!(!merged.contains("My OpenAI"));
        assert!(!merged.contains("https://proxy.example/v1"));
        // A user-*added* entry (not shipped) is preserved verbatim.
        assert!(merged.contains("name = \"mycorp\""));
        assert!(merged.contains("MyCorp Internal"));
        assert!(merged.contains("https://llm.mycorp.internal/v1"));
    }

    #[test]
    fn merge_defaults_noop_when_user_version_current() {
        let builtin = r#"
[meta]
defaults_version = "2026.5.20"

[[providers]]
name = "openai"
label = "OpenAI"
"#;
        // Same version → not legacy → no rewrite.
        assert!(merge_defaults_toml(builtin, builtin).is_none());
    }

    #[test]
    fn merge_remote_defaults_rejects_invalid_payload() {
        // An HTML error page / garbage served at the URL must not be
        // treated as defaults — bail BEFORE touching the local file.
        let err = merge_remote_defaults("<html>404 Not Found</html>")
            .expect_err("non-TOML remote must error");
        assert!(err.to_string().contains("not valid TOML"));

        // Valid TOML but no version → can't version-gate → reject.
        let err = merge_remote_defaults("[[providers]]\nname = \"x\"\n")
            .expect_err("versionless remote must error");
        assert!(err.to_string().contains("no defaults_version"));
    }
}
