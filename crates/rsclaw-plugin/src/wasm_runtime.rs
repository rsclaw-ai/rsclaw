//! WASM plugin runtime — loads `.wasm` component-model plugins via wasmtime.
//!
//! Each WASM plugin exports (via WIT `plugin-api` interface):
//!   - `handle-tool(tool-name, args-json) -> result<string, string>` — executes
//!     a tool
//!
//! Tool metadata (name, description, JSON schema) lives in `plugin.json5` —
//! the host does not call back into the wasm to discover tools.
//!
//! Host functions provided to plugins (via WIT `host-browser` and
//! `host-runtime`):
//!   - 13 browser automation functions (open, snapshot, click, fill, etc.)
//!   - `log`, `sleep`, `read-file`

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose};
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::debug;
use wasmtime::{
    Engine, Store, StoreLimits, StoreLimitsBuilder,
    component::{Component, Linker, bindgen},
};

/// Per-call wall-clock deadline in epoch ticks, relative to
/// `set_epoch_deadline` being called. The engine ticks every 100ms (see
/// `mod.rs::load_all_plugins`), so 18000 ticks ≈ 30 minutes. Browser-automation
/// plugins (image / video generation, scrape pagination) routinely run for
/// several minutes; the deadline only needs to be tight enough to kill a true
/// runaway.
const EPOCH_DEADLINE_TICKS: u64 = 18000;

/// Per-store memory cap for wasm linear memory.
const MEMORY_CAP_BYTES: usize = 256 * 1024 * 1024;

/// On-disk Chrome profile dir name used by all plugins (wasm + shell). Every
/// plugin (jimeng/douyin/xianyu/travel/...) shares this single profile so a
/// single Bytedance login spans all of them, a single Taobao login covers
/// travel + jimeng's downstream login flows, etc.
const SHARED_BROWSER_PROFILE: &str = "rsclaw";

type HostTrapResult<T> = std::result::Result<T, wasmtime::Error>;

static HOST_HTTP_TLS_PROVIDER: OnceLock<()> = OnceLock::new();
static HOST_HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

use rsclaw_browser::BrowserSession;

// ---------------------------------------------------------------------------
// WIT bindgen — generates host trait and typed export accessors
// ---------------------------------------------------------------------------

bindgen!({
    path: "src/wit/world.wit",
    world: "jimeng-plugin",
    imports: { default: async | trappable },
    exports: { default: async },
    require_store_data_send: true,
});

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A loaded WASM plugin, ready to dispatch tool calls.
pub struct WasmPlugin {
    /// Plugin name (from manifest).
    pub name: String,
    /// Semver version string (from manifest).
    pub version: Option<String>,
    /// Human-readable description (from manifest).
    pub description: Option<String>,
    /// Catalog summary (from manifest `summary`, else None → falls back to
    /// `description` at render time).
    pub summary: Option<String>,
    /// Tool names the manifest marks as common (from `commonTools`).
    pub common_tools: Vec<String>,
    /// Tools this plugin exposes.
    pub tools: Vec<WasmToolDef>,
    /// v2 toolGroups metadata from the manifest: group name → description.
    pub tool_groups: std::collections::HashMap<String, String>,
    /// Path to the `.wasm` file on disk.
    pub wasm_path: PathBuf,
    /// Wasmtime engine (shared across plugins).
    engine: Engine,
    /// Compiled component (component model, not core module).
    component: Component,
    /// Pre-linked instance for fast re-instantiation.
    linker: Linker<HostState>,
    /// Reference to the browser session for host function callbacks.
    browser: Arc<Mutex<Option<BrowserSession>>>,
    /// CDN routing rules declared by this plugin — applied when the plugin
    /// invokes `host::browser_download(url, ...)` so the host doesn't need
    /// to hardcode per-platform auth quirks.
    browser_cdn_rules: Vec<crate::manifest::CdnDownloadRule>,
    /// Resolved plugin config exposed through `host-config`.
    plugin_config: serde_json::Value,
    /// Requested host capabilities from the manifest.
    pub capabilities: Vec<String>,
    /// Slash command metadata from the manifest.
    pub slash_commands: Vec<crate::manifest::PluginSlashCommand>,
    /// Trusted tool aliases from plugin tool name to first-class host tool name.
    pub tool_aliases: HashMap<String, String>,
    /// Minimum gap between successive `call_tool` invocations on this plugin
    /// (host-enforced rate limit). 0 disables throttling.
    min_call_interval: Duration,
    /// Last `call_tool` start time, used to compute the throttle delay.
    last_call: Mutex<Option<Instant>>,
    /// Optional provider registry for host-vlm interface.
    providers: Option<Arc<rsclaw_provider::registry::ProviderRegistry>>,
    /// Default vision model name for host-vlm interface.
    vision_model: Option<String>,
}

/// A tool definition extracted from a WASM plugin's manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmToolDef {
    /// Tool name (unique within the plugin).
    pub name: String,
    /// Human-readable description of what the tool does.
    pub description: String,
    /// JSON Schema for the tool's input parameters.
    pub parameters: serde_json::Value,
    /// Plugin-author-declared "expose as real ToolDef by default" flag,
    /// mirroring `PluginToolDef.headline` from the JSON5 manifest. Read
    /// by `select_user_tools_pure` when computing the per-turn
    /// `dynamic_prefix.user_tools` set.
    #[serde(default)]
    pub headline: bool,
    /// Feature group (v2 toolGroups) — mirrors `PluginToolDef.group`.
    #[serde(default)]
    pub group: Option<String>,
}

/// Routing context for `host::notify` — when supplied by the agent
/// runtime, plugin notifications get forwarded as a real OutboundMessage
/// to the user's current channel; without it, notifications are
/// trace-logged only.
#[derive(Clone)]
pub struct WasmNotifyCtx {
    pub tx: tokio::sync::broadcast::Sender<rsclaw_channel::OutboundMessage>,
    pub target_id: String,
    pub channel: String,
    pub agent_id: String,
    pub peer_id: String,
    pub chat_id: String,
    pub session_key: String,
    pub is_group: bool,
    /// Originating channel account (e.g. feishu app account name). Stamped onto
    /// `OutboundMessage.account` so plugin notifications route via
    /// `<channel>/<account>` instead of the bare `<channel>` fallback — without
    /// it a multi-account feishu setup sends with an arbitrary app's token and
    /// Feishu rejects open_id targets with 99992361 "open_id cross app".
    pub account: Option<String>,
}

/// State passed into the wasmtime `Store`, available to host functions.
struct HostState {
    browser: Arc<Mutex<Option<BrowserSession>>>,
    wasi: wasmtime_wasi::WasiCtx,
    wasi_table: wasmtime::component::ResourceTable,
    limits: StoreLimits,
    notify_ctx: Option<WasmNotifyCtx>,
    /// CDN download rules from the calling plugin's manifest. Consulted by
    /// `browser_download` to attach a Referer when the URL matches.
    cdn_rules: Vec<crate::manifest::CdnDownloadRule>,
    /// Plugin name — used to scope per-plugin resources (SQLite DB path, etc.).
    plugin_name: String,
    /// Resolved plugin config visible to this invocation.
    plugin_config: serde_json::Value,
    /// Desktop session for host-desktop interface (input synthesis,
    /// screenshots).
    desktop: Box<dyn rsclaw_desktop::DesktopSession>,
    /// Optional provider registry for host-vlm interface.
    providers: Option<Arc<rsclaw_provider::registry::ProviderRegistry>>,
    /// Default vision model name for host-vlm interface.
    vision_model: Option<String>,
    /// ADB device serial (`RSCLAW_ANDROID_SERIAL` env var). Passed `-s
    /// <serial>` to every adb invocation; `None` uses the single attached
    /// device (adb default).
    android_serial: Option<String>,
    /// WDA session URL (`RSCLAW_IOS_WDA_URL` env var, default
    /// `http://localhost:8100`). Set when `ios-connect` succeeds.
    wda_url: Option<String>,
}

fn new_host_state(
    browser: Arc<Mutex<Option<BrowserSession>>>,
    notify_ctx: Option<WasmNotifyCtx>,
    cdn_rules: Vec<crate::manifest::CdnDownloadRule>,
    plugin_name: String,
    plugin_config: serde_json::Value,
    providers: Option<Arc<rsclaw_provider::registry::ProviderRegistry>>,
    vision_model: Option<String>,
) -> HostState {
    HostState {
        browser,
        wasi: wasmtime_wasi::WasiCtxBuilder::new().build(),
        wasi_table: wasmtime::component::ResourceTable::new(),
        limits: StoreLimitsBuilder::new()
            .memory_size(MEMORY_CAP_BYTES)
            .build(),
        notify_ctx,
        cdn_rules,
        plugin_name,
        plugin_config,
        desktop: rsclaw_desktop::create_session(),
        providers,
        vision_model,
        android_serial: std::env::var("RSCLAW_ANDROID_SERIAL").ok(),
        wda_url: None,
    }
}

/// Build a sandboxed `Store` for one plugin invocation: memory cap + epoch
/// deadline so a buggy plugin can't OOM or hang the gateway.
fn new_sandboxed_store(
    engine: &Engine,
    browser: Arc<Mutex<Option<BrowserSession>>>,
    notify_ctx: Option<WasmNotifyCtx>,
    cdn_rules: Vec<crate::manifest::CdnDownloadRule>,
    plugin_name: String,
    plugin_config: serde_json::Value,
    providers: Option<Arc<rsclaw_provider::registry::ProviderRegistry>>,
    vision_model: Option<String>,
) -> Store<HostState> {
    let mut store = Store::new(
        engine,
        new_host_state(
            browser,
            notify_ctx,
            cdn_rules,
            plugin_name,
            plugin_config,
            providers,
            vision_model,
        ),
    );
    store.limiter(|s| &mut s.limits);
    store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);
    store
}

impl wasmtime_wasi::WasiView for HostState {
    fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
        wasmtime_wasi::WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.wasi_table,
        }
    }
}

// ---------------------------------------------------------------------------
// Host trait implementations
// ---------------------------------------------------------------------------

/// Canonicalize a filesystem path from a WASM plugin and reject anything that
/// resolves outside the plugin workspace. `~` expansion and absolute paths
/// in the input are tolerated *only* if the canonical result still lives
/// under the workspace dir — otherwise the call is rejected.
fn canonicalize_plugin_path(input: &str) -> Result<PathBuf, String> {
    let workspace = rsclaw_config::loader::base_dir().join("workspace");
    let canonical = rsclaw_util::canonicalize_external_path(input, &workspace);
    if !canonical.starts_with(&workspace) {
        return Err(format!(
            "plugin path '{}' resolves outside workspace ({})",
            input,
            workspace.display()
        ));
    }
    Ok(canonical)
}

/// Same as `canonicalize_plugin_path` but also permits paths under
/// `~/.rsclaw/var/plugins/` and host-allocated artifact paths so plugins can
/// persist databases/config and write files returned by `allocate-artifact`.
fn canonicalize_writable_path(input: &str) -> Result<PathBuf, String> {
    let base = rsclaw_config::loader::base_dir();
    let workspace = base.join("workspace");
    let plugins_var = base.join("var").join("plugins");
    let downloads_rsclaw = dirs_next::download_dir()
        .unwrap_or_else(|| {
            dirs_next::home_dir()
                .unwrap_or_else(rsclaw_config::loader::base_dir)
                .join("Downloads")
        })
        .join("rsclaw");
    let canonical = rsclaw_util::canonicalize_external_path(input, &workspace);
    if canonical.starts_with(&workspace)
        || canonical.starts_with(&plugins_var)
        || canonical.starts_with(&downloads_rsclaw)
    {
        return Ok(canonical);
    }
    Err(format!(
        "writable path '{}' resolves outside allowed dirs (workspace, var/plugins, or Downloads/rsclaw)",
        input
    ))
}

/// Canonicalize a saved plugin artifact path for read-only document
/// extraction. In addition to workspace/plugin-var paths, this permits
/// `~/Downloads/rsclaw`, which is where `allocate-artifact` stores files.
fn canonicalize_plugin_artifact_path(input: &str) -> Result<PathBuf, String> {
    let base = rsclaw_config::loader::base_dir();
    let workspace = base.join("workspace");
    let plugins_var = base.join("var").join("plugins");
    let downloads_rsclaw = dirs_next::download_dir()
        .unwrap_or_else(|| {
            dirs_next::home_dir()
                .unwrap_or_else(rsclaw_config::loader::base_dir)
                .join("Downloads")
        })
        .join("rsclaw");
    let canonical = rsclaw_util::canonicalize_external_path(input, &workspace);
    if canonical.starts_with(&workspace)
        || canonical.starts_with(&plugins_var)
        || canonical.starts_with(&downloads_rsclaw)
    {
        return Ok(canonical);
    }
    Err(format!(
        "artifact path '{}' resolves outside allowed dirs (workspace, var/plugins, or Downloads/rsclaw)",
        input
    ))
}

fn canonicalize_browser_upload_path(plugin_name: &str, input: &str) -> Result<PathBuf, String> {
    let base = rsclaw_config::loader::base_dir();
    let workspace = base.join("workspace");
    let plugin_var = base.join("var").join("plugins").join(plugin_name);
    let downloads_rsclaw = dirs_next::download_dir()
        .unwrap_or_else(|| {
            dirs_next::home_dir()
                .unwrap_or_else(rsclaw_config::loader::base_dir)
                .join("Downloads")
        })
        .join("rsclaw");
    canonicalize_existing_file_in_roots(
        input,
        &workspace,
        &[workspace.clone(), plugin_var, downloads_rsclaw],
        "browser_upload",
    )
}

fn canonicalize_existing_file_in_roots(
    input: &str,
    workspace: &std::path::Path,
    allowed_roots: &[PathBuf],
    context: &str,
) -> Result<PathBuf, String> {
    let lexical = rsclaw_util::canonicalize_external_path(input, workspace);
    let meta = std::fs::metadata(&lexical)
        .map_err(|e| format!("{context}: stat {}: {e}", lexical.display()))?;
    if !meta.is_file() {
        return Err(format!(
            "{context}: path is not a regular file: {}",
            lexical.display()
        ));
    }
    let canonical = std::fs::canonicalize(&lexical)
        .map_err(|e| format!("{context}: canonicalize {}: {e}", lexical.display()))?;
    for root in allowed_roots {
        let root_canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        if canonical.starts_with(&root_canonical) {
            return Ok(canonical);
        }
    }
    Err(format!(
        "{context}: path '{}' resolves outside allowed dirs (workspace, plugin artifacts, or Downloads/rsclaw)",
        input
    ))
}

/// Extract readable text from a plugin-saved artifact.
pub(crate) async fn extract_text_from_plugin_file(path: &str) -> Result<String, String> {
    let canonical = canonicalize_plugin_artifact_path(path)?;
    let bytes = tokio::fs::read(&canonical)
        .await
        .map_err(|e| format!("failed to read {}: {e}", canonical.display()))?;
    let filename = canonical
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| canonical.to_string_lossy().into_owned());
    match rsclaw_channel::extract_file_text(&filename, &bytes).await {
        Some(text) if !text.trim().is_empty() => Ok(text),
        Some(_) => Err(format!(
            "no readable text extracted from {}",
            canonical.display()
        )),
        None => Err(format!(
            "unsupported file type or extraction failed for {}",
            canonical.display()
        )),
    }
}

/// Ingest a prepared document into the live knowledge base.
pub(crate) async fn kb_ingest_document(
    collection: &str,
    title: &str,
    content: &str,
    mime: &str,
) -> Result<String, String> {
    let collection = collection.trim().to_owned();
    let title = title.trim().to_owned();
    let content = content.to_owned();
    let mime = if mime.trim().is_empty() {
        "text/markdown".to_owned()
    } else {
        mime.trim().to_owned()
    };
    if collection.is_empty() {
        return Err("kb_ingest_document: collection is required".to_string());
    }
    if title.is_empty() {
        return Err("kb_ingest_document: title is required".to_string());
    }
    if content.trim().is_empty() {
        return Err("kb_ingest_document: content is required".to_string());
    }
    let kb = rsclaw_kb::global_service()
        .ok_or_else(|| "knowledge base is not available in this gateway".to_string())?;

    tokio::task::spawn_blocking(move || -> Result<String, String> {
        let find = || -> Result<Option<rsclaw_kb::model::KbCollection>, String> {
            kb.list_collections()
                .map_err(|e| e.to_string())
                .map(|cols| {
                    cols.into_iter()
                        .find(|c| c.name.eq_ignore_ascii_case(&collection))
                })
        };
        let collection_id = if let Some(c) = find()? {
            c.id
        } else {
            match kb.create_collection(&collection, None, None) {
                Ok(c) => c.id,
                Err(rsclaw_kb::KnowledgeError::DuplicateName) => find()?
                    .map(|c| c.id)
                    .ok_or_else(|| "collection vanished after duplicate".to_string())?,
                Err(e) => return Err(e.to_string()),
            }
        };

        let (doc_id, noop) = kb
            .ingest(&collection_id, &title, content.as_bytes(), Some(&mime))
            .map_err(|e| e.to_string())?;
        Ok(json!({
            "docId": doc_id,
            "collectionId": collection_id,
            "status": if noop { "duplicate" } else { "indexed" },
        })
        .to_string())
    })
    .await
    .map_err(|e| format!("kb ingest task failed: {e}"))?
}

impl rsclaw::plugin::host_browser::Host for HostState {
    async fn browser_open(&mut self, url: String) -> HostTrapResult<Result<String, String>> {
        Ok(self.browser_action("open", json!({"url": url})).await)
    }

    async fn browser_snapshot(&mut self) -> HostTrapResult<Result<String, String>> {
        Ok(self.browser_action("snapshot", json!({})).await)
    }

    async fn browser_click(&mut self, ref_str: String) -> HostTrapResult<Result<String, String>> {
        Ok(self.browser_action("click", json!({"ref": ref_str})).await)
    }

    async fn browser_click_at(&mut self, x: u32, y: u32) -> HostTrapResult<Result<String, String>> {
        Ok(self
            .browser_action("click_at", json!({"x": x, "y": y}))
            .await)
    }

    async fn browser_fill(
        &mut self,
        ref_str: String,
        text: String,
    ) -> HostTrapResult<Result<String, String>> {
        Ok(self
            .browser_action("fill", json!({"ref": ref_str, "text": text}))
            .await)
    }

    async fn browser_press(&mut self, key: String) -> HostTrapResult<Result<String, String>> {
        Ok(self.browser_action("press", json!({"key": key})).await)
    }

    async fn browser_eval(&mut self, code: String) -> HostTrapResult<Result<String, String>> {
        Ok(self.browser_action("evaluate", json!({"js": code})).await)
    }

    async fn browser_wait_text(
        &mut self,
        text: String,
        timeout_ms: u32,
    ) -> HostTrapResult<Result<String, String>> {
        let timeout_secs = u64::from(timeout_ms / 1000).max(1);
        Ok(self
            .browser_action(
                "wait",
                json!({"target": "text", "value": text, "timeout": timeout_secs}),
            )
            .await
            .map(|_| "ok".to_string()))
    }

    async fn wait_for_selector(
        &mut self,
        css_selector: String,
        timeout_ms: u32,
    ) -> HostTrapResult<Result<String, String>> {
        let timeout_secs = u64::from(timeout_ms / 1000).max(1);
        Ok(self
            .browser_action(
                "wait",
                json!({"target": "element", "value": css_selector, "timeout": timeout_secs}),
            )
            .await
            .map(|_| "ok".to_string()))
    }

    async fn wait_for_network_idle(
        &mut self,
        timeout_ms: u32,
    ) -> HostTrapResult<Result<String, String>> {
        let timeout_secs = u64::from(timeout_ms / 1000).max(1);
        Ok(self
            .browser_action(
                "wait",
                json!({"target": "networkidle", "timeout": timeout_secs}),
            )
            .await
            .map(|_| "ok".to_string()))
    }

    async fn eval_with_args(
        &mut self,
        code: String,
        args_json: String,
    ) -> HostTrapResult<Result<String, String>> {
        // JSON is valid JS expression syntax, so we can embed args_json
        // directly as an object literal — no escaping dance required.
        let args_literal = if args_json.trim().is_empty() {
            "null".to_string()
        } else {
            args_json
        };
        let wrapped = format!(
            r#"(async function() {{
                const __args = ({args_literal});
                const __fn = ({code});
                const __out = await __fn(__args);
                return typeof __out === "string" ? __out : JSON.stringify(__out);
            }})()"#
        );
        Ok(self
            .browser_action("evaluate", json!({"js": wrapped}))
            .await)
    }

    async fn switch_latest_tab(&mut self) -> HostTrapResult<Result<String, String>> {
        let mut guard = self.browser.lock().await;
        if guard.is_none() {
            return Ok(Err("browser not initialized".to_string()));
        }
        let session = guard.as_mut().expect("browser presence checked above");
        let tabs_val = match session.execute("list_tabs", &json!({})).await {
            Ok(v) => v,
            Err(e) => return Ok(Err(format!("list_tabs failed: {e:#}"))),
        };
        let tabs = match tabs_val.get("tabs").and_then(|t| t.as_array()) {
            Some(t) => t,
            None => return Ok(Err("list_tabs returned no tabs array".to_string())),
        };
        let last = match tabs.last() {
            Some(t) => t,
            None => return Ok(Err("no tabs to switch to".to_string())),
        };
        let tid = match last.get("id").and_then(|t| t.as_str()) {
            Some(s) => s,
            None => return Ok(Err("last tab has no id".to_string())),
        };
        let url = last.get("url").and_then(|u| u.as_str()).unwrap_or("?");
        match session
            .execute("switch_tab", &json!({"target_id": tid}))
            .await
        {
            Ok(_) => Ok(Ok(format!("switched to tab: {url}"))),
            Err(e) => Ok(Err(format!("switch_tab failed: {e:#}"))),
        }
    }

    async fn browser_screenshot(&mut self) -> HostTrapResult<Result<String, String>> {
        Ok(self.browser_action("screenshot", json!({})).await)
    }

    async fn browser_download(
        &mut self,
        ref_str: String,
        filename: String,
    ) -> HostTrapResult<Result<String, String>> {
        let mut args = json!({"ref": ref_str, "path": filename});
        // If the ref looks like a URL, consult the calling plugin's CDN
        // rules and attach a Referer when one matches. The host itself has
        // no domain knowledge — Bytedance / Douyin / future-platform quirks
        // live in each plugin's plugin.json5 under `browserCdn.downloadRules`.
        if ref_str.starts_with("http") {
            if let Some(rule) = self
                .cdn_rules
                .iter()
                .find(|r| r.match_hosts.iter().any(|m| ref_str.contains(m.as_str())))
            {
                args["referer"] = json!(rule.referer);
            }
        }
        Ok(self.browser_action("download", args).await)
    }

    async fn browser_upload(
        &mut self,
        ref_str: String,
        filepath: String,
    ) -> HostTrapResult<Result<String, String>> {
        let canonical = match canonicalize_browser_upload_path(&self.plugin_name, &filepath) {
            Ok(path) => path,
            Err(e) => return Ok(Err(e)),
        };
        // Note: cmd_upload expects `files: [path]` (array), not `filepath: path`.
        Ok(self
            .browser_action(
                "upload",
                json!({
                    "ref": ref_str,
                    "files": [canonical.to_string_lossy()],
                    "filepath": canonical.to_string_lossy(),
                }),
            )
            .await)
    }

    async fn browser_upload_multi(
        &mut self,
        ref_str: String,
        filepaths: Vec<String>,
    ) -> HostTrapResult<Result<String, String>> {
        if filepaths.is_empty() {
            return Ok(Err("browser_upload_multi: filepaths is empty".to_string()));
        }
        let mut canonical_files = Vec::with_capacity(filepaths.len());
        for fp in &filepaths {
            match canonicalize_browser_upload_path(&self.plugin_name, fp) {
                Ok(path) => canonical_files.push(path.to_string_lossy().to_string()),
                Err(e) => return Ok(Err(e)),
            }
        }
        Ok(self
            .browser_action(
                "upload",
                json!({
                    "ref": ref_str,
                    "files": canonical_files,
                }),
            )
            .await)
    }

    async fn browser_upload_via_chooser(
        &mut self,
        filepaths: Vec<String>,
        click_x: u32,
        click_y: u32,
    ) -> HostTrapResult<Result<String, String>> {
        if filepaths.is_empty() {
            return Ok(Err("browser_upload_via_chooser: filepaths is empty".to_string()));
        }
        // Canonicalize + sandbox-check every path (same policy as browser_upload).
        let mut canonical_files = Vec::with_capacity(filepaths.len());
        for fp in &filepaths {
            match canonicalize_browser_upload_path(&self.plugin_name, fp) {
                Ok(path) => canonical_files.push(path.to_string_lossy().to_string()),
                Err(e) => return Ok(Err(e)),
            }
        }
        Ok(self
            .browser_action(
                "upload_via_chooser",
                json!({
                    "files": canonical_files,
                    "x": click_x,
                    "y": click_y,
                }),
            )
            .await)
    }

    async fn browser_get_url(&mut self) -> HostTrapResult<Result<String, String>> {
        Ok(self.browser_action("get_url", json!({})).await)
    }
}

impl rsclaw::plugin::host_runtime::Host for HostState {
    async fn log(&mut self, level: String, msg: String) -> HostTrapResult<()> {
        // Use the module path as target (instead of "wasm_plugin") so plugin
        // logs inherit the default tracing filter level for this crate.
        match level.as_str() {
            "error" => tracing::error!(plugin_log = true, "{msg}"),
            "warn" => tracing::warn!(plugin_log = true, "{msg}"),
            "info" => tracing::info!(plugin_log = true, "{msg}"),
            "debug" => tracing::debug!(plugin_log = true, "{msg}"),
            _ => tracing::trace!(plugin_log = true, "{msg}"),
        }
        Ok(())
    }

    async fn sleep(&mut self, ms: u32) -> HostTrapResult<()> {
        tokio::time::sleep(std::time::Duration::from_millis(u64::from(ms))).await;
        Ok(())
    }

    async fn notify(&mut self, message: String) -> HostTrapResult<Result<String, String>> {
        tracing::info!(target: "wasm_plugin_notify", "{message}");
        if let Some(ctx) = &self.notify_ctx {
            let _ = ctx.tx.send(rsclaw_channel::OutboundMessage {
                target_id: ctx.target_id.clone(),
                is_group: false,
                text: message,
                reply_to: None,
                images: vec![],
                files: vec![],
                channel: Some(ctx.channel.clone()),
                account: ctx.account.clone(),
            });
            Ok(Ok("dispatched".to_string()))
        } else {
            Ok(Ok("logged_only".to_string()))
        }
    }

    async fn notify_with_image(
        &mut self,
        message: String,
        image_data_uri: String,
    ) -> HostTrapResult<Result<String, String>> {
        tracing::info!(target: "wasm_plugin_notify", "{message}");
        if let Some(ctx) = &self.notify_ctx {
            match ctx.tx.send(rsclaw_channel::OutboundMessage {
                target_id: ctx.target_id.clone(),
                is_group: false,
                text: message,
                reply_to: None,
                images: vec![image_data_uri],
                files: vec![],
                channel: Some(ctx.channel.clone()),
                account: ctx.account.clone(),
            }) {
                Ok(_) => Ok(Ok("dispatched".to_string())),
                Err(_) => Ok(Ok("no_receivers".to_string())),
            }
        } else {
            Ok(Ok("logged_only".to_string()))
        }
    }

    async fn notify_with_file(
        &mut self,
        message: String,
        file_path: String,
        mime: String,
    ) -> HostTrapResult<Result<String, String>> {
        tracing::info!(target: "wasm_plugin_notify", "{message}");
        if let Some(ctx) = &self.notify_ctx {
            // Enforce workspace allowlist on the supplied path. Plugins
            // can only attach files that already live under the workspace
            // dir — same containment rule used by `read_file`.
            let canonical = match canonicalize_plugin_artifact_path(&file_path) {
                Ok(p) => p,
                Err(e) => return Ok(Err(e)),
            };
            if !canonical.exists() {
                return Ok(Err(format!(
                    "notify_with_file: file does not exist: {}",
                    canonical.display()
                )));
            }
            let filename = canonical
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_string());
            let path_str = canonical.to_string_lossy().into_owned();
            match ctx.tx.send(rsclaw_channel::OutboundMessage {
                target_id: ctx.target_id.clone(),
                is_group: false,
                text: message,
                reply_to: None,
                images: vec![],
                files: vec![(filename, mime, path_str)],
                channel: Some(ctx.channel.clone()),
                account: ctx.account.clone(),
            }) {
                Ok(_) => Ok(Ok("dispatched".to_string())),
                Err(_) => Ok(Ok("no_receivers".to_string())),
            }
        } else {
            Ok(Ok("logged_only".to_string()))
        }
    }

    async fn kb_ingest_document(
        &mut self,
        collection: String,
        title: String,
        content: String,
        mime: String,
    ) -> HostTrapResult<Result<String, String>> {
        Ok(kb_ingest_document(&collection, &title, &content, &mime).await)
    }

    async fn read_file(&mut self, path: String) -> HostTrapResult<Result<String, String>> {
        let canonical = match canonicalize_plugin_path(&path) {
            Ok(p) => p,
            Err(e) => return Ok(Err(e)),
        };
        match tokio::fs::read_to_string(&canonical).await {
            Ok(contents) => Ok(Ok(contents)),
            Err(e) => Ok(Err(format!("failed to read {}: {e}", canonical.display()))),
        }
    }

    async fn extract_file_text(&mut self, path: String) -> HostTrapResult<Result<String, String>> {
        Ok(extract_text_from_plugin_file(&path).await)
    }

    async fn write_file(
        &mut self,
        path: String,
        contents: String,
    ) -> HostTrapResult<Result<String, String>> {
        let canonical = match canonicalize_writable_path(&path) {
            Ok(p) => p,
            Err(e) => return Ok(Err(e)),
        };
        if let Some(parent) = canonical.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return Ok(Err(format!(
                    "failed to create parent dirs for {}: {e}",
                    canonical.display()
                )));
            }
        }
        match tokio::fs::write(&canonical, contents).await {
            Ok(()) => Ok(Ok(canonical.to_string_lossy().into_owned())),
            Err(e) => Ok(Err(format!("failed to write {}: {e}", canonical.display()))),
        }
    }

    async fn ensure_dir(&mut self, path: String) -> HostTrapResult<Result<String, String>> {
        let canonical = match canonicalize_writable_path(&path) {
            Ok(p) => p,
            Err(e) => return Ok(Err(e)),
        };
        match tokio::fs::metadata(&canonical).await {
            Ok(meta) if meta.is_file() => {
                return Ok(Err(format!(
                    "ensure_dir: path exists and is a file, not a directory: {}",
                    canonical.display()
                )));
            }
            Ok(_) => return Ok(Ok(canonical.to_string_lossy().into_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match tokio::fs::create_dir_all(&canonical).await {
                    Ok(()) => Ok(Ok(canonical.to_string_lossy().into_owned())),
                    Err(e) => Ok(Err(format!(
                        "failed to create dir {}: {e}",
                        canonical.display()
                    ))),
                }
            }
            Err(e) => Ok(Err(format!("failed to stat {}: {e}", canonical.display()))),
        }
    }

    async fn sql_execute(
        &mut self,
        sql: String,
        params: Vec<String>,
    ) -> HostTrapResult<Result<String, String>> {
        if let Err(e) = validate_plugin_sql(&sql, PluginSqlKind::Execute) {
            return Ok(Err(format!("sql_execute blocked: {e}")));
        }
        let db_path = plugin_db_path(&self.plugin_name);
        if let Some(parent) = db_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return Ok(Err(format!("sql_execute: create_dir: {e}")));
            }
        }
        let result = tokio::task::spawn_blocking(move || {
            let conn = rusqlite::Connection::open(&db_path)?;
            let mut stmt = conn.prepare(&sql)?;
            let params_ref: Vec<&dyn rusqlite::ToSql> =
                params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
            let rows_affected = stmt.execute(params_ref.as_slice())?;
            let last_id = conn.last_insert_rowid();
            Ok::<_, rusqlite::Error>(
                json!({
                    "rows_affected": rows_affected,
                    "last_insert_rowid": last_id,
                })
                .to_string(),
            )
        })
        .await;
        match result {
            Ok(Ok(json)) => Ok(Ok(json)),
            Ok(Err(e)) => Ok(Err(format!("sql_execute error: {e}"))),
            Err(e) => Ok(Err(format!("sql_execute panic: {e}"))),
        }
    }

    async fn sql_query(
        &mut self,
        sql: String,
        params: Vec<String>,
    ) -> HostTrapResult<Result<String, String>> {
        if let Err(e) = validate_plugin_sql(&sql, PluginSqlKind::Query) {
            return Ok(Err(format!("sql_query blocked: {e}")));
        }
        let db_path = plugin_db_path(&self.plugin_name);
        if let Some(parent) = db_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return Ok(Err(format!("sql_query: create_dir: {e}")));
            }
        }
        let result = tokio::task::spawn_blocking(move || {
            let conn = rusqlite::Connection::open(&db_path)?;
            let mut stmt = conn.prepare(&sql)?;
            let params_ref: Vec<&dyn rusqlite::ToSql> =
                params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
            let column_names: Vec<String> = stmt
                .column_names()
                .into_iter()
                .map(|s| s.to_string())
                .collect();
            let rows = stmt.query_map(params_ref.as_slice(), |row| {
                let mut obj = serde_json::Map::new();
                for (i, name) in column_names.iter().enumerate() {
                    let val: serde_json::Value = match row.get_ref(i)? {
                        rusqlite::types::ValueRef::Null => serde_json::Value::Null,
                        rusqlite::types::ValueRef::Integer(v) => json!(v),
                        rusqlite::types::ValueRef::Real(v) => json!(v),
                        rusqlite::types::ValueRef::Text(v) => json!(String::from_utf8_lossy(v)),
                        rusqlite::types::ValueRef::Blob(v) => {
                            json!(base64::engine::general_purpose::STANDARD.encode(v))
                        }
                    };
                    obj.insert(name.clone(), val);
                }
                Ok(serde_json::Value::Object(obj))
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            serde_json::to_string(&out)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })
        .await;
        match result {
            Ok(Ok(json)) => Ok(Ok(json)),
            Ok(Err(e)) => Ok(Err(format!("sql_query error: {e}"))),
            Err(e) => Ok(Err(format!("sql_query panic: {e}"))),
        }
    }
}

#[derive(Clone, Copy)]
enum PluginSqlKind {
    Execute,
    Query,
}

fn validate_plugin_sql(sql: &str, kind: PluginSqlKind) -> std::result::Result<(), String> {
    let policy = sql_policy_text(sql);
    let trimmed = policy.trim();
    if trimmed.is_empty() {
        return Err("empty SQL".to_owned());
    }
    let statement = trimmed.strip_suffix(';').unwrap_or(trimmed).trim();
    if statement.contains(';') {
        return Err("multiple SQL statements are not allowed".to_owned());
    }
    let tokens: Vec<&str> = statement
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .collect();
    let Some(first) = tokens.first().copied() else {
        return Err("empty SQL".to_owned());
    };

    const BLOCKED_TOKENS: &[&str] = &[
        "alter",
        "analyze",
        "attach",
        "detach",
        "drop",
        "load_extension",
        "pragma",
        "reindex",
        "vacuum",
    ];
    for token in &tokens {
        if BLOCKED_TOKENS.contains(token) {
            return Err(format!("token `{token}` is not allowed"));
        }
        if *token == "kv" {
            return Err("reserved table `kv` is not available through host SQL".to_owned());
        }
    }

    match kind {
        PluginSqlKind::Query => {
            if first != "select" && first != "with" {
                return Err("sql_query only allows SELECT statements".to_owned());
            }
            for token in &tokens {
                if matches!(*token, "insert" | "update" | "delete" | "create" | "replace") {
                    return Err(format!("sql_query cannot contain `{token}`"));
                }
            }
        }
        PluginSqlKind::Execute => match first {
            "insert" | "update" | "delete" => {}
            "create" => {
                let second = tokens.get(1).copied();
                let third = tokens.get(2).copied();
                if second != Some("table")
                    && !(matches!(second, Some("temp" | "temporary")) && third == Some("table"))
                {
                    return Err("sql_execute only allows CREATE TABLE".to_owned());
                }
            }
            _ => {
                return Err(
                    "sql_execute only allows INSERT, UPDATE, DELETE, or CREATE TABLE".to_owned(),
                );
            }
        },
    }
    Ok(())
}

fn sql_policy_text(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' | '"' | '`' => {
                let quote = ch;
                out.push(' ');
                while let Some(inner) = chars.next() {
                    if inner == quote {
                        if chars.peek() == Some(&quote) {
                            let _ = chars.next();
                            continue;
                        }
                        break;
                    }
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                let _ = chars.next();
                for inner in chars.by_ref() {
                    if inner == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                let _ = chars.next();
                let mut prev = '\0';
                for inner in chars.by_ref() {
                    if prev == '*' && inner == '/' {
                        break;
                    }
                    prev = inner;
                }
                out.push(' ');
            }
            other => out.push(other.to_ascii_lowercase()),
        }
    }
    out
}

impl rsclaw::plugin::host_config::Host for HostState {
    async fn plugin_config(&mut self) -> HostTrapResult<Result<String, String>> {
        serde_json::to_string(&self.plugin_config)
            .map(Ok)
            .map_err(wasmtime::Error::from)
    }
}

impl rsclaw::plugin::host_context::Host for HostState {
    async fn current_context(&mut self) -> HostTrapResult<Result<String, String>> {
        let ctx = match &self.notify_ctx {
            Some(ctx) => json!({
                "plugin": self.plugin_name,
                "target_id": ctx.target_id,
                "channel": ctx.channel,
                "agent_id": ctx.agent_id,
                "peer_id": ctx.peer_id,
                "chat_id": ctx.chat_id,
                "session_key": ctx.session_key,
                "is_group": ctx.is_group,
            }),
            None => json!({
                "plugin": self.plugin_name,
                "target_id": "",
                "channel": "",
                "agent_id": "",
                "peer_id": "",
                "chat_id": "",
                "session_key": "",
                "is_group": false,
            }),
        };
        Ok(Ok(ctx.to_string()))
    }
}

impl rsclaw::plugin::host_http::Host for HostState {
    async fn request(
        &mut self,
        method: String,
        url: String,
        headers_json: String,
        body: String,
        timeout_ms: u32,
    ) -> HostTrapResult<Result<String, String>> {
        let headers: serde_json::Map<String, serde_json::Value> = if headers_json.trim().is_empty() {
            serde_json::Map::new()
        } else {
            match serde_json::from_str::<serde_json::Value>(&headers_json) {
                Ok(serde_json::Value::Object(map)) => map,
                Ok(_) => return Ok(Err("host_http.request: headers_json must be an object".to_owned())),
                Err(e) => return Ok(Err(format!("host_http.request: invalid headers_json: {e}"))),
            }
        };
        let timeout = if timeout_ms == 0 {
            Duration::from_secs(30)
        } else {
            Duration::from_millis(u64::from(timeout_ms))
        };
        let client = match host_http_client() {
            Ok(c) => c,
            Err(e) => return Ok(Err(format!("host_http.request: client build failed: {e}"))),
        };
        let method = match reqwest::Method::from_bytes(method.as_bytes()) {
            Ok(m) => m,
            Err(e) => return Ok(Err(format!("host_http.request: invalid method: {e}"))),
        };
        let url = match validate_host_http_url(&url).await {
            Ok(u) => u,
            Err(e) => return Ok(Err(format!("host_http.request: blocked URL: {e}"))),
        };
        let mut rb = client.request(method, url).timeout(timeout);
        for (k, v) in headers {
            let Some(s) = v.as_str() else {
                return Ok(Err(format!("host_http.request: header `{k}` must be a string")));
            };
            rb = rb.header(&k, s);
        }
        if !body.is_empty() {
            rb = rb.body(body);
        }
        let resp = match rb.send().await {
            Ok(r) => r,
            Err(e) => return Ok(Err(format!("host_http.request: transport error: {e}"))),
        };
        let status = resp.status().as_u16();
        let mut out_headers = serde_json::Map::new();
        for (k, v) in resp.headers() {
            if let Ok(s) = v.to_str() {
                out_headers.insert(k.as_str().to_owned(), json!(s));
            }
        }
        let body = match resp.text().await {
            Ok(t) => t,
            Err(e) => return Ok(Err(format!("host_http.request: body read failed: {e}"))),
        };
        Ok(Ok(json!({
            "status": status,
            "headers": out_headers,
            "body": body,
        }).to_string()))
    }
}

fn ensure_host_http_tls_provider() -> std::result::Result<(), String> {
    if rustls::crypto::CryptoProvider::get_default().is_some() {
        return Ok(());
    }
    if HOST_HTTP_TLS_PROVIDER.get().is_some() {
        return Ok(());
    }
    match rustls::crypto::aws_lc_rs::default_provider().install_default() {
        Ok(()) => {
            let _ = HOST_HTTP_TLS_PROVIDER.set(());
            Ok(())
        }
        Err(_) if rustls::crypto::CryptoProvider::get_default().is_some() => {
            let _ = HOST_HTTP_TLS_PROVIDER.set(());
            Ok(())
        }
        Err(_) => Err("failed to install rustls crypto provider".to_owned()),
    }
}

fn host_http_client() -> std::result::Result<reqwest::Client, String> {
    ensure_host_http_tls_provider()?;
    if let Some(client) = HOST_HTTP_CLIENT.get() {
        return Ok(client.clone());
    }
    let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .use_rustls_tls()
            .tls_built_in_root_certs(true)
            .build()
            .map_err(|e| e.to_string())?;
    let _ = HOST_HTTP_CLIENT.set(client);
    HOST_HTTP_CLIENT
        .get()
        .cloned()
        .ok_or_else(|| "host HTTP client init failed".to_owned())
}

async fn validate_host_http_url(raw: &str) -> std::result::Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw).map_err(|e| format!("invalid URL: {e}"))?;
    match url.scheme() {
        "http" | "https" => {}
        scheme => return Err(format!("scheme `{scheme}` is not allowed")),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL credentials are not allowed".to_owned());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "URL host is required".to_owned())?;
    if host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".localhost")
    {
        return Err("localhost is not allowed".to_owned());
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "URL port could not be resolved".to_owned())?;
    validate_host_http_endpoint(host, port).await?;
    Ok(url)
}

async fn validate_host_http_endpoint(host: &str, port: u16) -> std::result::Result<(), String> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return validate_host_http_ip(ip);
    }

    let mut addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("DNS lookup failed for `{host}`: {e}"))?;
    let mut resolved = false;
    for addr in addrs.by_ref() {
        resolved = true;
        validate_host_http_ip(addr.ip())?;
    }
    if !resolved {
        return Err(format!("DNS lookup returned no addresses for `{host}`"));
    }
    Ok(())
}

fn validate_host_http_ip(ip: IpAddr) -> std::result::Result<(), String> {
    if is_forbidden_host_http_ip(ip) && !unsafe_allow_private_host_http_for_debug() {
        return Err(format!("IP `{ip}` is not allowed"));
    }
    Ok(())
}

fn unsafe_allow_private_host_http_for_debug() -> bool {
    #[cfg(debug_assertions)]
    {
        std::env::var("RSCLAW_UNSAFE_PLUGIN_HTTP_ALLOW_PRIVATE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

fn is_forbidden_host_http_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_forbidden_host_http_ipv4(ip),
        IpAddr::V6(ip) => is_forbidden_host_http_ipv6(ip),
    }
}

fn is_forbidden_host_http_ipv4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 0
        || o[0] == 10
        || o[0] == 127
        || (o[0] == 100 && (64..=127).contains(&o[1]))
        || (o[0] == 169 && o[1] == 254)
        || (o[0] == 172 && (16..=31).contains(&o[1]))
        || (o[0] == 192 && o[1] == 168)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
        || o[0] >= 224
}

fn is_forbidden_host_http_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_forbidden_host_http_ipv4(v4);
    }
    let segments = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
}

impl rsclaw::plugin::host_kv::Host for HostState {
    async fn kv_get(&mut self, key: String) -> HostTrapResult<Result<String, String>> {
        plugin_kv_get(&self.plugin_name, key).await
    }

    async fn kv_set(&mut self, key: String, value: String) -> HostTrapResult<Result<String, String>> {
        plugin_kv_set(&self.plugin_name, key, value).await
    }

    async fn kv_delete(&mut self, key: String) -> HostTrapResult<Result<String, String>> {
        plugin_kv_delete(&self.plugin_name, key).await
    }
}

impl rsclaw::plugin::host_device::Host for HostState {
    async fn device_public_key(&mut self) -> HostTrapResult<Result<String, String>> {
        match load_device_signing_key().await {
            Ok(key) => Ok(Ok(device_public_key_json(&key))),
            Err(e) => Ok(Err(e)),
        }
    }

    async fn device_sign(&mut self, payload: String) -> HostTrapResult<Result<String, String>> {
        match load_device_signing_key().await {
            Ok(key) => {
                let sig = key.sign(payload.as_bytes());
                Ok(Ok(json!({
                    "alg": "ed25519",
                    "publicKey": general_purpose::STANDARD.encode(key.verifying_key().as_bytes()),
                    "signature": general_purpose::STANDARD.encode(sig.to_bytes()),
                })
                .to_string()))
            }
            Err(e) => Ok(Err(e)),
        }
    }
}

impl rsclaw::plugin::host_background::Host for HostState {
    async fn cron_register(
        &mut self,
        name: String,
        schedule_json: String,
    ) -> HostTrapResult<Result<String, String>> {
        Ok(crate::cron_register(
            self.plugin_name.clone(),
            name,
            schedule_json,
            self.invocation_context(),
        )
        .await)
    }

    async fn sse_subscribe(
        &mut self,
        name: String,
        url: String,
        headers_json: String,
        resume_key: String,
    ) -> HostTrapResult<Result<String, String>> {
        Ok(crate::sse_subscribe(
            self.plugin_name.clone(),
            name,
            url,
            headers_json,
            resume_key,
            self.invocation_context(),
        )
        .await)
    }

    async fn sse_status(&mut self, name: String) -> HostTrapResult<Result<String, String>> {
        Ok(crate::sse_status(
            self.plugin_name.clone(),
            name,
            self.invocation_context(),
        )
        .await)
    }

    async fn sse_unsubscribe(&mut self, name: String) -> HostTrapResult<Result<String, String>> {
        Ok(crate::sse_unsubscribe(
            self.plugin_name.clone(),
            name,
            self.invocation_context(),
        )
        .await)
    }

    async fn push_outbound(
        &mut self,
        channel: String,
        peer_id: String,
        message_json: String,
    ) -> HostTrapResult<Result<String, String>> {
        Ok(crate::push_outbound(
            channel,
            peer_id,
            message_json,
            self.invocation_context(),
        )
        .await)
    }

    async fn submit_agent_turn(
        &mut self,
        session_key: String,
        prompt: String,
        route_json: String,
    ) -> HostTrapResult<Result<String, String>> {
        Ok(crate::submit_agent_turn(
            session_key,
            prompt,
            route_json,
            self.invocation_context(),
        )
        .await)
    }
}

/// Return the SQLite database path for a given plugin name.
fn plugin_db_path(plugin_name: &str) -> PathBuf {
    rsclaw_config::loader::base_dir()
        .join("var")
        .join("plugins")
        .join(plugin_name)
        .join("plugin.db")
}

fn device_key_path() -> PathBuf {
    rsclaw_config::loader::base_dir()
        .join("device")
        .join("host-ed25519.key")
}

async fn load_device_signing_key() -> Result<SigningKey, String> {
    tokio::task::spawn_blocking(|| {
        let path = device_key_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("host_device: create key dir failed: {e}"))?;
        }
        if path.exists() {
            restrict_device_key_permissions(&path)?;
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| format!("host_device: read key failed: {e}"))?;
            let bytes = general_purpose::STANDARD
                .decode(raw.trim())
                .map_err(|e| format!("host_device: key base64 decode failed: {e}"))?;
            let key_bytes: [u8; 32] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| "host_device: key must be 32 bytes".to_owned())?;
            return Ok(SigningKey::from_bytes(&key_bytes));
        }
        let key_bytes: [u8; 32] = rand::random();
        let encoded = general_purpose::STANDARD.encode(key_bytes);
        write_device_key_restricted(&path, &encoded)?;
        Ok(SigningKey::from_bytes(&key_bytes))
    })
    .await
    .map_err(|e| format!("host_device: key task failed: {e}"))?
}

/// Write the device key creating the file with `0o600` from the start, so
/// the secret is never on disk under the default (world/group-readable)
/// umask even briefly. A plain `fs::write` + later `chmod` leaves a
/// TOCTOU window where a same-host attacker can read the key. On non-unix
/// (no mode bits) fall back to a plain write; Windows protection relies on
/// the per-user profile ACL.
fn write_device_key_restricted(path: &std::path::Path, encoded: &str) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("host_device: open key for write failed: {e}"))?;
        f.write_all(encoded.as_bytes())
            .map_err(|e| format!("host_device: write key failed: {e}"))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, encoded)
            .map_err(|e| format!("host_device: write key failed: {e}"))?;
    }
    // Belt-and-suspenders: if the file pre-existed (concurrent create) the
    // mode above is a no-op, so re-assert restrictive perms.
    restrict_device_key_permissions(path)?;
    Ok(())
}

fn restrict_device_key_permissions(path: &std::path::Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("host_device: set key permissions failed: {e}"))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn device_public_key_json(key: &SigningKey) -> String {
    json!({
        "alg": "ed25519",
        "publicKey": general_purpose::STANDARD.encode(key.verifying_key().as_bytes()),
    })
    .to_string()
}

async fn plugin_kv_get(plugin_name: &str, key: String) -> HostTrapResult<Result<String, String>> {
    match plugin_kv_get_value(plugin_name, &key).await {
        Ok(Some(value)) => Ok(Ok(value)),
        Ok(None) => Ok(Ok(String::new())),
        Err(e) => Ok(Err(format!("host_kv.get: {e}"))),
    }
}

/// Read a plugin-scoped key/value entry for trusted host-side integrations.
pub async fn plugin_kv_get_value(plugin_name: &str, key: &str) -> Result<Option<String>, String> {
    let db_path = plugin_db_path(plugin_name);
    let key = key.to_owned();
    let result = tokio::task::spawn_blocking(move || {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        }
        let conn = rusqlite::Connection::open(db_path)?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            [],
        )?;
        let mut stmt = conn.prepare("SELECT value FROM kv WHERE key = ?1")?;
        let value: Option<String> = stmt.query_row([key], |row| row.get(0)).ok();
        Ok::<_, rusqlite::Error>(value)
    })
    .await;
    match result {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) => Err(format!("host_kv.get panic: {e}")),
    }
}

async fn plugin_kv_set(
    plugin_name: &str,
    key: String,
    value: String,
) -> HostTrapResult<Result<String, String>> {
    match plugin_kv_set_value(plugin_name, &key, &value).await {
        Ok(()) => Ok(Ok("ok".to_owned())),
        Err(e) => Ok(Err(format!("host_kv.set: {e}"))),
    }
}

/// Write a plugin-scoped key/value entry for trusted host-side integrations.
pub async fn plugin_kv_set_value(plugin_name: &str, key: &str, value: &str) -> Result<(), String> {
    let db_path = plugin_db_path(plugin_name);
    let key = key.to_owned();
    let value = value.to_owned();
    let result = tokio::task::spawn_blocking(move || {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        }
        let conn = rusqlite::Connection::open(db_path)?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            [],
        )?;
        conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (key, value),
        )?;
        Ok::<_, rusqlite::Error>(())
    })
    .await;
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) => Err(format!("host_kv.set panic: {e}")),
    }
}

async fn plugin_kv_delete(plugin_name: &str, key: String) -> HostTrapResult<Result<String, String>> {
    let db_path = plugin_db_path(plugin_name);
    let result = tokio::task::spawn_blocking(move || {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        }
        let conn = rusqlite::Connection::open(db_path)?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            [],
        )?;
        let changed = conn.execute("DELETE FROM kv WHERE key = ?1", [key])?;
        Ok::<_, rusqlite::Error>(changed)
    })
    .await;
    match result {
        Ok(Ok(changed)) => Ok(Ok(json!({ "deleted": changed }).to_string())),
        Ok(Err(e)) => Ok(Err(format!("host_kv.delete: {e}"))),
        Err(e) => Ok(Err(format!("host_kv.delete panic: {e}"))),
    }
}

fn resolve_plugin_config(raw: &serde_json::Value) -> serde_json::Value {
    fn walk(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(map) => {
                let source = map.get("source").and_then(|v| v.as_str());
                let id = map.get("id").and_then(|v| v.as_str());
                if source == Some("env") && let Some(id) = id {
                    return std::env::var(id)
                        .map(serde_json::Value::String)
                        .unwrap_or(serde_json::Value::Null);
                }
                serde_json::Value::Object(
                    map.iter()
                        .map(|(k, v)| (k.clone(), walk(v)))
                        .collect(),
                )
            }
            serde_json::Value::Array(arr) => {
                serde_json::Value::Array(arr.iter().map(walk).collect())
            }
            other => other.clone(),
        }
    }
    walk(raw)
}

impl rsclaw::plugin::host_storage::Host for HostState {
    async fn allocate_artifact(
        &mut self,
        filename: String,
    ) -> HostTrapResult<Result<String, String>> {
        Ok(allocate_dl_paths(&filename, 1)
            .map(|paths| paths.into_iter().next().unwrap_or_default()))
    }

    async fn allocate_artifact_group(
        &mut self,
        filename: String,
        count: u32,
    ) -> HostTrapResult<Result<Vec<String>, String>> {
        Ok(allocate_dl_paths(&filename, count.max(1) as usize))
    }
}

// ---------------------------------------------------------------------------
// host-media trait implementation
// ---------------------------------------------------------------------------

impl rsclaw::plugin::host_media::Host for HostState {
    async fn extract_audio(
        &mut self,
        input_path: String,
    ) -> HostTrapResult<Result<String, String>> {
        let ffmpeg_bin = match rsclaw_platform::detect_ffmpeg() {
            Some(p) => p,
            None => {
                return Ok(Err(
                    "ffmpeg not found. Run: rsclaw tools install ffmpeg".to_string()
                ));
            }
        };

        let out_path = match allocate_dl_paths("audio.wav", 1) {
            Ok(mut p) => p.pop().unwrap_or_default(),
            Err(e) => return Ok(Err(e)),
        };

        let output = tokio::process::Command::new(&ffmpeg_bin)
            .args([
                "-y",
                "-i",
                &input_path,
                "-vn",
                "-acodec",
                "pcm_s16le",
                "-ar",
                "16000",
                "-ac",
                "1",
                &out_path,
            ])
            .output()
            .await;

        match output {
            Ok(o) if o.status.success() => Ok(Ok(out_path)),
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                Ok(Err(format!("ffmpeg failed: {stderr}")))
            }
            Err(e) => Ok(Err(format!("ffmpeg spawn error: {e}"))),
        }
    }

    async fn transcribe(
        &mut self,
        audio_path: String,
        _language: String,
    ) -> HostTrapResult<Result<String, String>> {
        let bytes = match tokio::fs::read(&audio_path).await {
            Ok(b) => b,
            Err(e) => return Ok(Err(format!("read audio file failed: {e}"))),
        };

        let mime = if audio_path.to_lowercase().ends_with(".wav") {
            "audio/wav"
        } else {
            "audio/mpeg"
        };

        let client = reqwest::Client::new();
        match rsclaw_channel::transcription::transcribe_audio(&client, &bytes, &audio_path, mime)
            .await
        {
            Ok(text) => Ok(Ok(text)),
            Err(e) => Ok(Err(format!("transcription failed: {e:#}"))),
        }
    }

    async fn extract_keyframes(
        &mut self,
        video_path: String,
        count: u32,
    ) -> HostTrapResult<Result<Vec<String>, String>> {
        let ffmpeg_bin = match rsclaw_platform::detect_ffmpeg() {
            Some(p) => p,
            None => {
                return Ok(Err(
                    "ffmpeg not found. Run: rsclaw tools install ffmpeg".to_string()
                ));
            }
        };

        let count = count.max(1).min(20) as usize;
        let out_paths = match allocate_dl_paths("frame.png", count) {
            Ok(p) => p,
            Err(e) => return Ok(Err(e)),
        };

        // Get video duration via ffprobe
        let duration_secs: f64 = {
            let probe = tokio::process::Command::new(&ffmpeg_bin)
                .args([
                    "-v",
                    "error",
                    "-show_entries",
                    "format=duration",
                    "-of",
                    "default=noprint_wrappers=1:nokey=1",
                    &video_path,
                ])
                .output()
                .await;
            match probe {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .parse()
                    .unwrap_or(0.0),
                _ => 0.0,
            }
        };

        if duration_secs <= 0.0 {
            return Ok(Err("could not determine video duration".to_string()));
        }

        let interval = duration_secs / count as f64;
        let out_pattern = out_paths[0].replace(".png", "_%03d.png");

        let output = tokio::process::Command::new(&ffmpeg_bin)
            .args([
                "-y",
                "-i",
                &video_path,
                "-vf",
                &format!("fps=1/{interval},scale=480:-1"),
                &out_pattern,
            ])
            .output()
            .await;

        match output {
            Ok(o) if o.status.success() => Ok(Ok(out_paths)),
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                Ok(Err(format!("ffmpeg failed: {stderr}")))
            }
            Err(e) => Ok(Err(format!("ffmpeg spawn error: {e}"))),
        }
    }
}

/// Build `count` canonical download paths, all sharing the same
/// `dl_<kind>_<TS><abc>` base. For `count > 1` each path gets a `_N`
/// (1-based) index suffix; for `count == 1` no suffix is appended.
///
/// Layout: `~/Downloads/rsclaw/<category>/<dl_kind_TS_abc[_N]>.<ext>`.
/// The host owns the on-disk shape; plugins only pass a hint filename
/// whose extension drives the category and ext.
pub(crate) fn allocate_dl_paths(filename: &str, count: usize) -> Result<Vec<String>, String> {
    if filename.contains('/') || filename.contains('\\') {
        return Err(format!(
            "allocate_artifact: filename must not contain path separators: {filename}"
        ));
    }
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin")
        .to_ascii_lowercase();
    let kind = rsclaw_channel::kind_from_extension(&ext);
    let category = rsclaw_channel::category_for_kind(kind);
    let dir = dirs_next::download_dir()
        .unwrap_or_else(|| {
            dirs_next::home_dir()
                .unwrap_or_else(rsclaw_config::loader::base_dir)
                .join("Downloads")
        })
        .join("rsclaw")
        .join(category);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err(format!("allocate_artifact: create_dir: {e}"));
    }
    // Pick a (timestamp, abc) base that doesn't collide with anything
    // already on disk. 26^3 = 17 576 combinations, so for any sane rate
    // a single retry is enough; cap at 10 so we surface a real failure
    // instead of looping if the directory is somehow saturated.
    let ts = chrono::Local::now().format("%Y%m%d%H%M").to_string();
    for _ in 0..10 {
        let abc: String = (0..3)
            .map(|_| (rand::random::<u8>() % 26 + b'a') as char)
            .collect();
        let base = format!("dl_{kind}_{ts}{abc}");
        let names: Vec<String> = if count <= 1 {
            vec![format!("{base}.{ext}")]
        } else {
            (1..=count).map(|i| format!("{base}_{i}.{ext}")).collect()
        };
        if names.iter().any(|n| dir.join(n).exists()) {
            continue;
        }
        let paths: Vec<String> = names
            .into_iter()
            .map(|n| dir.join(n).to_string_lossy().to_string())
            .collect();
        tracing::debug!(target: "wasm_plugin", "allocated artifact group: {} paths under {}", paths.len(), dir.display());
        return Ok(paths);
    }
    Err("allocate_artifact: could not pick a unique name after 10 attempts".to_owned())
}

// ---------------------------------------------------------------------------
// ADB helper functions (host-android)
// ---------------------------------------------------------------------------

/// Run `adb [-s SERIAL] SUBCMD...` and return stdout as UTF-8.
async fn adb_run_str(serial: Option<&str>, sub: &[&str]) -> Result<String, String> {
    let mut args: Vec<String> = Vec::with_capacity(sub.len() + 2);
    if let Some(s) = serial {
        args.push("-s".into());
        args.push(s.into());
    }
    for &s in sub {
        args.push(s.into());
    }
    let out = tokio::process::Command::new("adb")
        .args(&args)
        .output()
        .await
        .map_err(|e| format!("adb spawn failed: {e} (is adb in PATH?)"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("adb ({}): {}", out.status, stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run `adb [-s SERIAL] SUBCMD...` and return raw stdout bytes (screencap).
async fn adb_run_bytes(serial: Option<&str>, sub: &[&str]) -> Result<Vec<u8>, String> {
    let mut args: Vec<String> = Vec::with_capacity(sub.len() + 2);
    if let Some(s) = serial {
        args.push("-s".into());
        args.push(s.into());
    }
    for &s in sub {
        args.push(s.into());
    }
    let out = tokio::process::Command::new("adb")
        .args(&args)
        .output()
        .await
        .map_err(|e| format!("adb spawn failed: {e} (is adb in PATH?)"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("adb ({}): {}", out.status, stderr.trim()));
    }
    Ok(out.stdout)
}

/// Characters refused in any `input text` payload that ultimately runs
/// through the device's shell via `adb shell`. The shell sees the whole
/// trailing argv joined with spaces, so `\n`, `\r`, `\0` would let an
/// attacker-supplied text smuggle a second command after the first.
/// Quoting/escaping `input text` arguments correctly across device shells
/// (sh / mksh / toybox) is far harder than rejecting the small set of
/// metacharacters that have no legitimate use in user-visible input.
const ADB_INPUT_REFUSED_CHARS: &[char] = &[
    ';', '&', '|', '>', '<', '$', '`', '\\', '"', '\'', '\n', '\r', '\0',
];

/// Per-call counter that names temp UI dumps on the device so concurrent
/// callers from different plugins don't clobber the same path.
static ADB_UI_DUMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Dump the UI hierarchy via uiautomator and return the XML string.
///
/// Writes to a unique path under `/sdcard/` per call (process pid +
/// monotonically increasing counter) so two concurrent
/// `android-get-ui-xml` calls don't race over the same file. Best-effort
/// removes the temp file after reading so /sdcard doesn't accumulate
/// dumps over a long session — failure to remove is silent (the next
/// call uses a fresh path anyway).
async fn adb_ui_xml(serial: Option<&str>, compressed: bool) -> Result<String, String> {
    // Prefer the UIAutomator2 server: the built-in `uiautomator dump` blocks
    // waiting for the UI to go idle and is KILLED on apps with continuous
    // animation/content (Xianyu, Douyin, …). u2's /source dumps immediately.
    // Fall back to the legacy dump if u2 can't be brought up (e.g. server APK
    // not installed on the device).
    match u2_ui_xml(serial).await {
        Ok(xml) => return Ok(xml),
        Err(e) => {
            tracing::warn!("u2 source unavailable ({e}); falling back to uiautomator dump");
        }
    }
    let seq = ADB_UI_DUMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dest = format!("/sdcard/rsclaw_ui_dump_{}_{}.xml", std::process::id(), seq);
    let dump_args: &[&str] = if compressed {
        &["shell", "uiautomator", "dump", "--compressed", &dest]
    } else {
        &["shell", "uiautomator", "dump", &dest]
    };
    adb_run_str(serial, dump_args)
        .await
        .map_err(|e| format!("uiautomator dump: {e}"))?;
    let xml = adb_run_str(serial, &["exec-out", "cat", &dest]).await?;
    let _ = adb_run_str(serial, &["shell", "rm", "-f", &dest]).await;
    Ok(xml)
}

// ---------------------------------------------------------------------------
// UIAutomator2 server backend (host-android UI reads)
// ---------------------------------------------------------------------------

/// Device-side port the appium-uiautomator2 server listens on.
const U2_DEVICE_PORT: u16 = 6790;

struct U2Conn {
    base: String,
    /// A live u2 WebDriver session id (needed for /element/active + setValue).
    /// `/source` is served session-less so reads don't require this.
    session: Option<String>,
    /// Held so the `am instrument` adb child isn't reaped (it keeps the
    /// on-device server alive). Never awaited.
    _instr: Option<tokio::process::Child>,
}

/// Per-serial u2 connection cache (process-global; HostState is per-call).
fn u2_conns() -> &'static Mutex<std::collections::HashMap<String, U2Conn>> {
    static M: OnceLock<Mutex<std::collections::HashMap<String, U2Conn>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Per-serial setup guards — prevents concurrent u2 setup of the SAME device
/// while allowing different devices to set up in parallel.
fn u2_setup_locks() -> &'static Mutex<HashMap<String, Arc<Mutex<()>>>> {
    static M: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Clone the cached base URL for a serial, releasing the lock immediately.
async fn u2_cached_base(key: &str) -> Option<String> {
    u2_conns().lock().await.get(key).map(|c| c.base.clone())
}

/// Local forwarded port for a serial — distinct per device so multiple
/// devices don't collide. Derived deterministically from the serial.
fn u2_local_port(serial: Option<&str>) -> u16 {
    match serial {
        None => 6790,
        Some(s) => {
            let mut h: u32 = 2166136261;
            for b in s.bytes() { h = (h ^ b as u32).wrapping_mul(16777619); }
            6800 + (h % 600) as u16 // 6800..7400
        }
    }
}

/// Ensure the u2 server is running and forwarded; return its base URL.
///
/// The main connection cache lock is only held for brief map reads/writes,
/// never across ADB commands, HTTP calls, or the readiness poll loop. A
/// per-serial guard prevents concurrent setup of the same device while
/// allowing different devices to set up in parallel.
async fn u2_ensure(serial: Option<&str>) -> Result<String, String> {
    let key = serial.unwrap_or("").to_string();

    // Fast path: grab the cached base URL, release the lock, then verify
    // liveness via HTTP without holding it.
    if let Some(base) = u2_cached_base(&key).await {
        if u2_status_ok(&base).await {
            return Ok(base);
        }
        u2_conns().lock().await.remove(&key);
    }

    // Per-serial setup guard: only one task sets up a given device at a
    // time, but different devices can set up concurrently.
    let setup_guard = {
        let mut locks = u2_setup_locks().lock().await;
        locks
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    let _setup_guard = setup_guard.lock().await;

    // Re-check cache: another task may have completed setup while we waited.
    if let Some(base) = u2_cached_base(&key).await {
        if u2_status_ok(&base).await {
            return Ok(base);
        }
        u2_conns().lock().await.remove(&key);
    }

    // Require the server APKs (cls/appium install these; we don't bundle them).
    let pkgs = adb_run_str(serial, &["shell", "pm", "list", "packages"]).await?;
    if !pkgs.contains("io.appium.uiautomator2.server.test") {
        return Err(
            "appium-uiautomator2-server not installed on device (run `cls android uiauto setup` \
             or appium first)"
                .into(),
        );
    }

    let local = u2_local_port(serial);
    let base = format!("http://127.0.0.1:{local}");
    let fwd = format!("tcp:{local}");
    let dev = format!("tcp:{U2_DEVICE_PORT}");
    adb_run_str(serial, &["forward", &fwd, &dev])
        .await
        .map_err(|e| format!("adb forward: {e}"))?;

    // Already up (started by a previous run / cls / appium)? Reuse it.
    if u2_status_ok(&base).await {
        let session = u2_open_session(&base).await;
        u2_conns()
            .lock()
            .await
            .insert(key.clone(), U2Conn { base: base.clone(), session, _instr: None });
        return Ok(base);
    }

    // Spawn the instrumentation server (long-lived; held in the cache).
    // kill_on_drop ensures the process is terminated when a stale U2Conn is
    // evicted from the cache, preventing orphaned instrumentation processes.
    let mut cmd = tokio::process::Command::new("adb");
    if let Some(s) = serial { cmd.arg("-s").arg(s); }
    cmd.args([
        "shell", "am", "instrument", "-w", "-e", "disableAnalytics", "true",
        "io.appium.uiautomator2.server.test/androidx.test.runner.AndroidJUnitRunner",
    ]);
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| format!("spawn u2 instrument: {e}"))?;

    // Poll /status up to ~30s.
    let mut ok = false;
    for _ in 0..60 {
        if u2_status_ok(&base).await { ok = true; break; }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    if !ok {
        return Err("u2 server did not become ready within 30s".into());
    }
    let session = u2_open_session(&base).await;
    u2_conns()
        .lock()
        .await
        .insert(key, U2Conn { base: base.clone(), session, _instr: Some(child) });
    Ok(base)
}

/// Reuse an existing u2 session or create one. Returns None on failure
/// (reads via /source still work; only focused-input needs a session).
async fn u2_open_session(base: &str) -> Option<String> {
    let client = host_http_client().ok()?;
    // Reuse.
    if let Ok(resp) = client.get(format!("{base}/sessions"))
        .timeout(std::time::Duration::from_secs(5)).send().await
        && let Ok(v) = resp.json::<Value>().await
        && let Some(id) = v.get("value").and_then(|a| a.as_array()).and_then(|a| a.first())
            .and_then(|s| s.get("sessionId").or_else(|| s.get("id"))).and_then(|x| x.as_str())
        && !id.is_empty()
    {
        return Some(id.to_string());
    }
    // Create.
    let caps = json!({"capabilities":{"alwaysMatch":{
        "platformName":"Android","appium:automationName":"UiAutomator2","appium:noReset":true}}});
    let resp = client.post(format!("{base}/session")).json(&caps)
        .timeout(std::time::Duration::from_secs(20)).send().await.ok()?;
    let v: Value = resp.json().await.ok()?;
    v.pointer("/value/sessionId").or_else(|| v.get("sessionId"))
        .and_then(|x| x.as_str()).map(String::from)
}

/// ADBKeyboard (com.android.adbkeyboard) IME package — injects arbitrary
/// Unicode into the focused field via broadcast, IME-level, so it works even on
/// Flutter-rendered inputs that expose NO accessibility element (where u2's
/// element/setValue has nothing to target). This is the only reliable CJK path
/// for such pages.
const ADBKB_IME: &str = "com.android.adbkeyboard/.AdbIME";
const ADBKB_PKG: &str = "com.android.adbkeyboard";

/// Tracks which serials already have ADBKeyboard selected as the active IME.
fn adbkb_ready() -> &'static Mutex<std::collections::HashSet<String>> {
    static M: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Ensure ADBKeyboard is installed and selected as the active IME for `serial`.
/// Self-healing: verifies the ACTUAL current IME rather than trusting the cache
/// alone — the device IME can be changed out from under us (manual switch, another
/// app), which previously left the cache stale and made CJK input silently no-op.
async fn adbkb_ensure(serial: Option<&str>) -> Result<(), String> {
    let key = serial.unwrap_or("").to_string();
    // Fast path: cached AND the device confirms ADBKeyboard is still active.
    if adbkb_ready().lock().await.contains(&key)
        && adb_run_str(serial, &["shell", "settings", "get", "secure", "default_input_method"])
            .await
            .map(|out| out.contains(ADBKB_IME))
            .unwrap_or(false)
    {
        return Ok(());
    }
    let pkgs = adb_run_str(serial, &["shell", "pm", "list", "packages"]).await?;
    if !pkgs.contains(ADBKB_PKG) {
        return Err("ADBKeyboard not installed (needed for CJK on Flutter inputs); \
                    sideload https://github.com/senzhk/ADBKeyBoard ADBKeyboard.apk".into());
    }
    if let Err(e) = adb_run_str(serial, &["shell", "ime", "enable", ADBKB_IME]).await {
        tracing::warn!(serial = ?serial, error = %e, "best-effort ime enable ADBKeyboard");
    }
    adb_run_str(serial, &["shell", "ime", "set", ADBKB_IME]).await
        .map_err(|e| format!("ime set ADBKeyboard: {e}"))?;
    // Let the IME switch settle.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    adbkb_ready().lock().await.insert(key);
    Ok(())
}

/// Type `text` into the focused field via ADBKeyboard's base64 broadcast.
async fn adbkb_type(serial: Option<&str>, text: &str) -> Result<(), String> {
    adbkb_ensure(serial).await?;
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    adb_run_str(serial, &["shell", "am", "broadcast", "-a", "ADB_INPUT_B64", "--es", "msg", &b64])
        .await
        .map(|_| ())
        .map_err(|e| format!("ADBKeyboard broadcast: {e}"))
}

/// Percent-encode a string for safe use in a URL path segment. u2 session and
/// element IDs come from the server's JSON responses and are typically
/// alphanumeric, but encode defensively against any special characters.
fn u2_url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Type `text` into the currently FOCUSED element via u2 (Unicode-capable —
/// the device-side instrumentation handles CJK, unlike `adb shell input text`
/// which is ASCII-only). Caller is expected to have already focused the field
/// (e.g. by tapping it). Returns Err if u2 / a session isn't available.
async fn u2_type_focused(serial: Option<&str>, text: &str) -> Result<(), String> {
    let base = u2_ensure(serial).await?;
    let sid = {
        let map = u2_conns().lock().await;
        map.get(serial.unwrap_or("")).and_then(|c| c.session.clone())
            .ok_or("u2 session unavailable")?
    };
    let client = host_http_client()?;
    // Focused element.
    let resp = client.get(format!("{base}/session/{}/element/active", u2_url_encode(&sid)))
        .timeout(std::time::Duration::from_secs(8)).send().await
        .map_err(|e| format!("u2 active element: {e}"))?;
    let v: Value = resp.json().await.map_err(|e| format!("u2 active body: {e}"))?;
    let aid = v.get("value").and_then(|x| x.get("ELEMENT").or_else(|| x.get("element-6066-11e4-a52e-4f735466cecf")))
        .and_then(|x| x.as_str())
        .ok_or("no focused element (tap the field first)")?
        .to_string();
    let body = json!({ "text": text, "value": [text] });
    let resp = client.post(format!(
        "{base}/session/{}/element/{}/value",
        u2_url_encode(&sid),
        u2_url_encode(&aid),
    ))
        .json(&body).timeout(std::time::Duration::from_secs(10)).send().await
        .map_err(|e| format!("u2 setValue: {e}"))?;
    if resp.status().is_success() { Ok(()) }
    else { Err(format!("u2 setValue http {}", resp.status())) }
}

/// Resolve `(base, session_id)` for a serial, bringing the u2 server up if
/// needed. Every gesture below needs a live session (W3C `/actions` and
/// `/screenshot` are session-scoped on this server build).
async fn u2_session(serial: Option<&str>) -> Result<(String, String), String> {
    let base = u2_ensure(serial).await?;
    let sid = {
        let map = u2_conns().lock().await;
        map.get(serial.unwrap_or("")).and_then(|c| c.session.clone())
    };
    match sid {
        Some(s) => Ok((base, s)),
        None => Err("u2 session unavailable".into()),
    }
}

/// Tap at `(x, y)` via the u2 server's W3C `/actions` endpoint — no `adb shell
/// input` process per call, so no screen flash and no adb spawn storm in tight
/// monitor loops.
async fn u2_tap(serial: Option<&str>, x: u32, y: u32) -> Result<(), String> {
    let (base, sid) = u2_session(serial).await?;
    let client = host_http_client()?;
    let body = json!({"actions":[{
        "type":"pointer","id":"finger1","parameters":{"pointerType":"touch"},
        "actions":[
            {"type":"pointerMove","duration":0,"x":x,"y":y},
            {"type":"pointerDown","button":0},
            {"type":"pause","duration":60},
            {"type":"pointerUp","button":0}
        ]}]});
    let resp = client
        .post(format!("{base}/session/{}/actions", u2_url_encode(&sid)))
        .json(&body)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("u2 tap: {e}"))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("u2 tap http {}", resp.status()))
    }
}

/// Swipe from `(x1,y1)` to `(x2,y2)` over `duration_ms` via the u2 `/actions`
/// endpoint (a single held pointer move, like a finger drag).
async fn u2_swipe(
    serial: Option<&str>,
    x1: u32,
    y1: u32,
    x2: u32,
    y2: u32,
    duration_ms: u32,
) -> Result<(), String> {
    let (base, sid) = u2_session(serial).await?;
    let client = host_http_client()?;
    let dur = duration_ms.max(1);
    let body = json!({"actions":[{
        "type":"pointer","id":"finger1","parameters":{"pointerType":"touch"},
        "actions":[
            {"type":"pointerMove","duration":0,"x":x1,"y":y1},
            {"type":"pointerDown","button":0},
            {"type":"pointerMove","duration":dur,"x":x2,"y":y2},
            {"type":"pointerUp","button":0}
        ]}]});
    let resp = client
        .post(format!("{base}/session/{}/actions", u2_url_encode(&sid)))
        .json(&body)
        .timeout(std::time::Duration::from_secs((dur / 1000 + 12) as u64))
        .send()
        .await
        .map_err(|e| format!("u2 swipe: {e}"))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("u2 swipe http {}", resp.status()))
    }
}

/// Capture a screenshot via the u2 `/screenshot` endpoint. Returns the raw
/// (un-prefixed) base64 PNG string. Unlike `adb exec-out screencap`, this
/// reuses the persistent instrumentation connection — no per-frame adb child.
async fn u2_screenshot_b64(serial: Option<&str>) -> Result<String, String> {
    let (base, sid) = u2_session(serial).await?;
    let client = host_http_client()?;
    let resp = client
        .get(format!("{base}/session/{}/screenshot", u2_url_encode(&sid)))
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("u2 screenshot: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("u2 screenshot http {}", resp.status()));
    }
    let v: Value = resp.json().await.map_err(|e| format!("u2 screenshot body: {e}"))?;
    v.get("value")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| "u2 screenshot: empty value".to_string())
}

async fn u2_status_ok(base: &str) -> bool {
    let Ok(client) = host_http_client() else { return false };
    match client
        .get(format!("{base}/status"))
        .timeout(std::time::Duration::from_secs(4))
        .send()
        .await
    {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

/// Fetch the current UI hierarchy from u2 and normalize it to the legacy
/// `<node ...>` tag shape the rest of the host (and plugins) parse.
async fn u2_ui_xml(serial: Option<&str>) -> Result<String, String> {
    let base = u2_ensure(serial).await?;
    // The sessionless `GET /source` errors on this server build; the page
    // source must be fetched under a live session (esp. for Flutter pages
    // whose semantics tree only materializes within a session).
    let sid = {
        let map = u2_conns().lock().await;
        map.get(serial.unwrap_or("")).and_then(|c| c.session.clone())
    };
    let path = match &sid {
        Some(s) => format!("{base}/session/{}/source", u2_url_encode(s)),
        None => format!("{base}/source"),
    };
    let client = host_http_client()?;
    let resp = client
        .get(&path)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("u2 /source: {e}"))?;
    let body = resp.text().await.map_err(|e| format!("u2 /source body: {e}"))?;
    // u2 returns {"value":"<xml>"} (or sometimes raw XML).
    let xml = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.get("value").and_then(|x| x.as_str()).map(String::from))
        .unwrap_or(body);
    Ok(u2_normalize_xml(&xml))
}

/// u2 emits one element per class-named tag (`<android.widget.TextView .../>`)
/// whereas the built-in dump uses `<node class="…" …>`. The host/plugin
/// parsers key on `<node `, so rewrite every element tag name — both open
/// and closing — to `node` (attributes on open tags, including the existing
/// `class="…"`, are preserved).
fn u2_normalize_xml(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len() + 64);
    let mut chars = xml.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '<' {
            if let Some(&(_, n)) = chars.peek() {
                if n.is_ascii_alphabetic() {
                    // Element open tag — consume the tag name, emit `node`.
                    let mut name = String::new();
                    while let Some(&(_, ch)) = chars.peek() {
                        if ch.is_whitespace() || ch == '>' || ch == '/' { break; }
                        name.push(ch);
                        chars.next();
                    }
                    let _ = (i, name);
                    out.push_str("<node");
                    continue;
                } else if n == '/' {
                    // Possible closing tag — consume '/' and peek further.
                    chars.next();
                    if let Some(&(_, m)) = chars.peek()
                        && m.is_ascii_alphabetic()
                    {
                        let mut name = String::new();
                        while let Some(&(_, ch)) = chars.peek() {
                            if ch.is_whitespace() || ch == '>' { break; }
                            name.push(ch);
                            chars.next();
                        }
                        let _ = (i, name);
                        out.push_str("</node");
                        continue;
                    }
                    // Not an element close; emit consumed chars as-is.
                    out.push_str("</");
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}

/// Decode XML character references (e.g. `&#10;` → newline) found in
/// uiautomator attribute values.
fn adb_xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#10;", "\n")
        .replace("&#xA;", "\n")
}

/// Extract a single XML attribute value from a `<node ...>` tag string.
fn adb_xml_attr<'a>(node: &'a str, attr: &str) -> &'a str {
    let needle = format!(" {}=\"", attr);
    match node.find(&needle) {
        None => "",
        Some(i) => {
            let s = i + needle.len();
            match node[s..].find('"') {
                None => "",
                Some(e) => &node[s..s + e],
            }
        }
    }
}

/// Parse `"[x1,y1][x2,y2]"` bounds string and return the center coordinate.
fn adb_bounds_center(bounds: &str) -> (i32, i32) {
    let coords: Vec<i32> = bounds
        .split(|c: char| !c.is_ascii_digit() && c != '-')
        .filter(|s: &&str| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    if coords.len() >= 4 {
        ((coords[0] + coords[2]) / 2, (coords[1] + coords[3]) / 2)
    } else {
        (0, 0)
    }
}

/// Scan UI XML for `<node>` tags and return those matching the selector.
fn adb_match_elements(xml: &str, sel_type: &str, sel_val: &str) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(rel) = xml[pos..].find("<node ") {
        let start = pos + rel;
        // Opening tag ends at the first `>` (attribute values use &gt; for literal
        // `>`).
        let tag_end = xml[start..]
            .find('>')
            .map(|r| start + r + 1)
            .unwrap_or(xml.len());
        let node = &xml[start..tag_end.min(xml.len())];
        pos = tag_end.max(start + 1);

        // Decode `text` AND `content-desc` — both come through XML
        // attribute encoding (text containing `"` arrives as `&quot;`,
        // newlines as `&#10;`). The earlier code only decoded `content-
        // desc`, which meant `text="Don't"` showed up in matches and
        // JSON as `Don&apos;t` and `text-contains` would miss obvious
        // user-visible strings. Match against the decoded form so
        // selectors see what a human reading the screen sees.
        let text = adb_xml_unescape(adb_xml_attr(node, "text"));
        let rid = adb_xml_attr(node, "resource-id");
        let cdesc = adb_xml_unescape(adb_xml_attr(node, "content-desc"));
        let class = adb_xml_attr(node, "class");
        let bounds = adb_xml_attr(node, "bounds");
        let clickable = adb_xml_attr(node, "clickable") == "true";

        let matched = match sel_type {
            "resource-id" => rid == sel_val,
            "text" => text == sel_val,
            "text-contains" => !sel_val.is_empty() && text.contains(sel_val),
            "content-desc" => cdesc == sel_val,
            "content-desc-contains" => !sel_val.is_empty() && cdesc.contains(sel_val),
            "class" => class == sel_val,
            _ => false,
        };
        if !matched {
            continue;
        }

        let (cx, cy) = adb_bounds_center(bounds);
        out.push(serde_json::json!({
            "text": text,
            "resource-id": rid,
            "content-desc": cdesc,
            "bounds": {"centerX": cx, "centerY": cy, "raw": bounds},
            "clickable": clickable,
        }));
    }
    out
}

impl HostState {
    fn invocation_context(&self) -> Option<crate::PluginInvocationContext> {
        self.notify_ctx.as_ref().map(|ctx| crate::PluginInvocationContext {
            target_id: ctx.target_id.clone(),
            channel: ctx.channel.clone(),
            agent_id: ctx.agent_id.clone(),
            peer_id: ctx.peer_id.clone(),
            chat_id: ctx.chat_id.clone(),
            session_key: ctx.session_key.clone(),
            is_group: ctx.is_group,
        })
    }

    /// Execute a browser action by locking the shared browser session.
    /// Auto-starts Chrome if no session exists.
    async fn browser_action(&mut self, action: &str, args: Value) -> Result<String, String> {
        let mut guard = self.browser.lock().await;

        // Auto-start browser if not initialized.
        if guard.is_none() {
            tracing::info!("WASM plugin: auto-starting browser session");
            let chrome_path = rsclaw_platform::detect_chrome()
                .ok_or_else(|| {
                    anyhow::anyhow!("Chrome not found; run: rsclaw tools install chrome")
                })
                .map_err(|e| format!("failed to obtain Chrome: {e:#}"))?;
            // All plugins share one Chrome profile so that auth state
            // (cookies, localStorage) is reused across the session — e.g.
            // a single login to Bytedance covers jimeng + douyin + xianyu,
            // a single Taobao login covers travel + jimeng. Callers should
            // treat this as an opaque shared identifier.
            let session = BrowserSession::start(&chrome_path, true, Some(SHARED_BROWSER_PROFILE))
                .await
                .map_err(|e| format!("failed to start Chrome: {e:#}"))?;
            *guard = Some(session);
        }

        let session = guard.as_mut().expect("browser session just initialized");
        match session.execute(action, &args).await {
            Ok(val) => {
                // Extract the payload field from action results so WASM plugins
                // get clean data, not the JSON wrapper.
                // snapshot → "text", screenshot → "image", others → full JSON
                for field in &["text", "image", "data", "url", "result"] {
                    if let Some(s) = val.get(field).and_then(|v| v.as_str()) {
                        return Ok(s.to_string());
                    }
                }
                Ok(val.to_string())
            }
            Err(e) => Err(format!("{e:#}")),
        }
    }
}

// ---------------------------------------------------------------------------
// host-desktop trait implementation
// ---------------------------------------------------------------------------

impl rsclaw::plugin::host_desktop::Host for HostState {
    async fn desktop_activate_app(
        &mut self,
        bundle_id: String,
    ) -> wasmtime::Result<Result<String, String>> {
        Ok(self.desktop.activate_app(&bundle_id).await)
    }

    async fn desktop_list_windows(
        &mut self,
        bundle_id: String,
    ) -> wasmtime::Result<Result<String, String>> {
        Ok(self.desktop.list_windows(&bundle_id).await)
    }

    async fn desktop_close_window(
        &mut self,
        bundle_id: String,
        window_idx: u32,
    ) -> wasmtime::Result<Result<String, String>> {
        Ok(self.desktop.close_window(&bundle_id, window_idx).await)
    }

    async fn desktop_get_main_window(
        &mut self,
        bundle_id: String,
    ) -> wasmtime::Result<Result<String, String>> {
        Ok(self.desktop.get_main_window(&bundle_id).await)
    }

    async fn desktop_screenshot_window(
        &mut self,
        bundle_id: String,
    ) -> wasmtime::Result<Result<String, String>> {
        Ok(self.desktop.screenshot_window(&bundle_id).await)
    }

    async fn desktop_ocr_window(
        &mut self,
        bundle_id: String,
    ) -> wasmtime::Result<Result<String, String>> {
        Ok(self.desktop.ocr_window(&bundle_id).await)
    }

    async fn desktop_screenshot_region(
        &mut self,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> wasmtime::Result<Result<String, String>> {
        Ok(self.desktop.screenshot_region(x, y, w, h).await)
    }

    #[allow(clippy::too_many_arguments)]
    async fn desktop_region_has_color(
        &mut self,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        r: u32,
        g: u32,
        b: u32,
        tolerance: u32,
        min_count: u32,
    ) -> wasmtime::Result<Result<String, String>> {
        Ok(self
            .desktop
            .region_has_color(x, y, w, h, r, g, b, tolerance, min_count)
            .await)
    }

    async fn desktop_mouse_move(
        &mut self,
        x: u32,
        y: u32,
    ) -> wasmtime::Result<Result<String, String>> {
        Ok(self.desktop.mouse_move(x, y).await)
    }

    async fn desktop_mouse_click(
        &mut self,
        x: u32,
        y: u32,
    ) -> wasmtime::Result<Result<String, String>> {
        Ok(self.desktop.mouse_click(x, y).await)
    }

    async fn desktop_mouse_double_click(
        &mut self,
        x: u32,
        y: u32,
    ) -> wasmtime::Result<Result<String, String>> {
        Ok(self.desktop.mouse_double_click(x, y).await)
    }

    async fn desktop_mouse_drag(
        &mut self,
        x1: u32,
        y1: u32,
        x2: u32,
        y2: u32,
    ) -> wasmtime::Result<Result<String, String>> {
        Ok(self.desktop.mouse_drag(x1, y1, x2, y2).await)
    }

    async fn desktop_mouse_scroll(
        &mut self,
        clicks: i32,
    ) -> wasmtime::Result<Result<String, String>> {
        Ok(self.desktop.mouse_scroll(clicks).await)
    }

    async fn desktop_key_press(
        &mut self,
        key: String,
        modifiers: Vec<String>,
    ) -> wasmtime::Result<Result<String, String>> {
        Ok(self.desktop.key_press(&key, &modifiers).await)
    }

    async fn desktop_clipboard_set(
        &mut self,
        text: String,
    ) -> wasmtime::Result<Result<String, String>> {
        Ok(self.desktop.clipboard_set(&text).await)
    }

    async fn desktop_clipboard_get(&mut self) -> wasmtime::Result<Result<String, String>> {
        Ok(self.desktop.clipboard_get().await)
    }

    async fn desktop_clipboard_set_file(
        &mut self,
        file_path: String,
    ) -> wasmtime::Result<Result<String, String>> {
        Ok(self.desktop.clipboard_set_file(&file_path).await)
    }

    async fn desktop_clipboard_get_image(&mut self) -> wasmtime::Result<Result<String, String>> {
        Ok(self.desktop.clipboard_get_image().await)
    }

    async fn desktop_mouse_right_click(
        &mut self,
        x: u32,
        y: u32,
    ) -> wasmtime::Result<Result<String, String>> {
        Ok(self.desktop.mouse_right_click(x, y).await)
    }

    async fn desktop_file_dialog_open(
        &mut self,
        title: String,
        filters: Vec<String>,
    ) -> wasmtime::Result<Result<String, String>> {
        Ok(self.desktop.file_dialog_open(&title, &filters).await)
    }
}

// ---------------------------------------------------------------------------
// host-vlm trait implementation
// ---------------------------------------------------------------------------

impl rsclaw::plugin::host_vlm::Host for HostState {
    async fn vlm_parse(
        &mut self,
        image_data_uri: String,
        prompt: String,
        max_tokens: u32,
    ) -> wasmtime::Result<Result<String, String>> {
        let Some(providers) = self.providers.as_ref() else {
            return Ok(Err("vlm_parse: no provider registry configured".to_string()));
        };
        let Some(vision_model) = self.vision_model.as_ref() else {
            return Ok(Err("vlm_parse: no vision model configured".to_string()));
        };

        let (provider_name, model_id) = providers.resolve_model(vision_model);
        let provider = match providers.get(provider_name) {
            Ok(p) => p,
            Err(e) => {
                return Ok(Err(format!(
                    "vlm_parse: provider {provider_name} not found: {e}"
                )));
            }
        };

        // Downscale the screenshot before sending. Full-res window captures
        // (~1834px) encode to ~2744 vision tokens and ship a multi-MB PNG over
        // the Mac→cloud→GPU hop, which dominates per-call latency (~71s observed).
        // Cap the long edge at 1280 and re-encode JPEG q85 — cuts both wire bytes
        // and image tokens. Safe for navigation: the plugin maps VLM boxes in
        // 0-1000 normalized space, independent of pixel dimensions. Any
        // parse/decode/resize failure falls back to the original URI.
        let image_data_uri = {
            let downscaled = image_data_uri.split_once(";base64,").and_then(|(header, b64)| {
                let mime = header.strip_prefix("data:").unwrap_or("image/png").to_string();
                let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
                let (new_bytes, new_mime) =
                    rsclaw_util::downscale_image_for_vision(&bytes, &mime, 256 * 1024, 1280, 85)
                        .ok()?;
                Some(format!(
                    "data:{new_mime};base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(&new_bytes)
                ))
            });
            downscaled.unwrap_or(image_data_uri)
        };

        let messages = vec![rsclaw_provider::Message {
            role: rsclaw_provider::Role::User,
            content: rsclaw_provider::MessageContent::Parts(vec![
                rsclaw_provider::ContentPart::Text { text: prompt },
                rsclaw_provider::ContentPart::Image {
                    url: image_data_uri,
                },
            ]),
            rsclaw_hidden: None,
        }];

        let req = rsclaw_provider::LlmRequest {
            fallback_models: Vec::new(),
            model: format!("{provider_name}/{model_id}"),
            messages,
            tools: Vec::new(),
            system: None,
            max_tokens: Some(max_tokens),
            temperature: Some(0.0),
            frequency_penalty: None,
            thinking_budget: None,
            endpoint: rsclaw_provider::AgentEndpoint::Vision,
            kv_cache_mode: 0,
            session_key: None,
            system_shared: None,
            user_system: None,
            recall: None,
        };

        match provider.stream(req).await {
            Ok(mut stream) => {
                let mut text = String::new();
                let mut reasoning = String::new();
                use futures::StreamExt;
                while let Some(event) = stream.next().await {
                    match event {
                        Ok(rsclaw_provider::StreamEvent::TextDelta(d)) => text.push_str(&d),
                        Ok(rsclaw_provider::StreamEvent::ReasoningDelta(d)) => {
                            reasoning.push_str(&d)
                        }
                        Ok(rsclaw_provider::StreamEvent::Done { .. }) => break,
                        Ok(rsclaw_provider::StreamEvent::ToolCall { .. }) => {}
                        Ok(rsclaw_provider::StreamEvent::Error(e)) => {
                            return Ok(Err(format!("vlm_parse stream error: {e}")));
                        }
                        Err(e) => {
                            return Ok(Err(format!("vlm_parse stream error: {e}")));
                        }
                    }
                }
                let result = if text.trim().is_empty() {
                    reasoning
                } else {
                    text
                };
                Ok(Ok(result))
            }
            Err(e) => Ok(Err(format!("vlm_parse provider error: {e}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// host-android trait implementation
// ---------------------------------------------------------------------------

impl rsclaw::plugin::host_android::Host for HostState {
    async fn android_tap(&mut self, x: u32, y: u32) -> HostTrapResult<Result<String, String>> {
        let serial = self.android_serial.clone();
        // Prefer u2 /actions — no `adb shell input` child per tap, so no screen
        // flash and no adb spawn storm in tight monitor loops. Fall back to adb
        // only if the u2 server can't be reached.
        if u2_tap(serial.as_deref(), x, y).await.is_ok() {
            return Ok(Ok("tapped".to_string()));
        }
        let (xs, ys) = (x.to_string(), y.to_string());
        Ok(
            adb_run_str(serial.as_deref(), &["shell", "input", "tap", &xs, &ys])
                .await
                .map(|_| "tapped".to_string()),
        )
    }

    async fn android_swipe(
        &mut self,
        x1: u32,
        y1: u32,
        x2: u32,
        y2: u32,
        duration_ms: u32,
    ) -> HostTrapResult<Result<String, String>> {
        let serial = self.android_serial.clone();
        if u2_swipe(serial.as_deref(), x1, y1, x2, y2, duration_ms).await.is_ok() {
            return Ok(Ok("swiped".to_string()));
        }
        let (s1, s2, s3, s4, s5) = (
            x1.to_string(),
            y1.to_string(),
            x2.to_string(),
            y2.to_string(),
            duration_ms.to_string(),
        );
        Ok(adb_run_str(
            serial.as_deref(),
            &["shell", "input", "swipe", &s1, &s2, &s3, &s4, &s5],
        )
        .await
        .map(|_| "swiped".to_string()))
    }

    async fn android_type(&mut self, text: String) -> HostTrapResult<Result<String, String>> {
        let serial = self.android_serial.clone();
        // Types into the currently FOCUSED field (caller taps to focus first).
        // For non-ASCII (CJK), prefer ADBKeyboard: u2 element/setValue can
        // "succeed" (HTTP 200) yet only commit a stray char on Flutter-backed
        // EditTexts (e.g. the Xianyu chat input), so it must NOT run first for CJK.
        if !text.is_ascii() {
            match adbkb_type(serial.as_deref(), &text).await {
                Ok(()) => return Ok(Ok("typed".to_string())),
                Err(e) => {
                    if u2_type_focused(serial.as_deref(), &text).await.is_ok() {
                        return Ok(Ok("typed".to_string()));
                    }
                    return Ok(Err(format!("CJK input needs ADBKeyboard/u2: {e}")));
                }
            }
        }
        // ASCII: u2 element/setValue is fast on native EditTexts.
        if u2_type_focused(serial.as_deref(), &text).await.is_ok() {
            return Ok(Ok("typed".to_string()));
        }
        // ADBKeyboard broadcast — IME-level, works on Flutter inputs too.
        match adbkb_type(serial.as_deref(), &text).await {
            Ok(()) => return Ok(Ok("typed".to_string())),
            Err(e) => {
                tracing::warn!("ADBKeyboard unavailable ({e}); falling back to adb input text");
            }
        }
        // 3) adb input text — ASCII only, last resort.
        if let Some(bad) = text.chars().find(|c| ADB_INPUT_REFUSED_CHARS.contains(c)) {
            return Ok(Err(format!(
                "android_type: refusing text with shell metachar '{}' (strip and retry)",
                bad.escape_debug()
            )));
        }
        let escaped = text.replace(' ', "%s");
        Ok(
            adb_run_str(serial.as_deref(), &["shell", "input", "text", &escaped])
                .await
                .map(|_| "typed".to_string()),
        )
    }

    async fn android_clipboard_set(
        &mut self,
        text: String,
    ) -> HostTrapResult<Result<String, String>> {
        if text.contains('\0') {
            return Ok(Err(
                "android_clipboard_set: refusing text containing NUL".to_string()
            ));
        }
        const MAX_CLIPBOARD_TEXT_BYTES: usize = 256 * 1024;
        if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
            return Ok(Err(format!(
                "android_clipboard_set: text too large ({} bytes, max {})",
                text.len(),
                MAX_CLIPBOARD_TEXT_BYTES
            )));
        }
        let serial = self.android_serial.clone();
        let output = match adb_run_str(
            serial.as_deref(),
            &["shell", "cmd", "clipboard", "set", "text", "rsclaw", &text],
        )
        .await
        {
            Ok(output) => output,
            Err(err) => return Ok(Err(err)),
        };
        if output.contains("No shell command implementation") {
            return Ok(Err(
                "android_clipboard_set: device does not implement `cmd clipboard`".to_string(),
            ));
        }
        Ok(Ok("clipboard_set".to_string()))
    }

    async fn android_paste(&mut self) -> HostTrapResult<Result<String, String>> {
        let serial = self.android_serial.clone();
        match adb_run_str(
            serial.as_deref(),
            &["shell", "input", "keyevent", "KEYCODE_PASTE"],
        )
        .await
        {
            Ok(_) => Ok(Ok("pasted".to_string())),
            Err(first) => Ok(adb_run_str(
                serial.as_deref(),
                &["shell", "input", "keyevent", "279"],
            )
            .await
            .map(|_| "pasted".to_string())
            .map_err(|second| format!("android_paste failed: {first}; fallback failed: {second}"))),
        }
    }

    async fn android_press(&mut self, key: String) -> HostTrapResult<Result<String, String>> {
        // Pseudo-key (not a keyevent): make ADBKeyboard the active IME. Needed to
        // prime the IME BEFORE a long-press→全选 text-replace on Flutter inputs —
        // switching IME after a selection drops it, so the plugin primes first,
        // selects, then types (adbkb_ensure caches, so the later type won't
        // re-switch and clobber the selection).
        if matches!(key.to_lowercase().as_str(), "ime-adbkb" | "ime_adbkb") {
            let serial = self.android_serial.clone();
            return Ok(adbkb_ensure(serial.as_deref())
                .await
                .map(|_| "ime adbkb".to_string()));
        }
        let kc = match key.to_lowercase().as_str() {
            "back" => "KEYCODE_BACK",
            "home" => "KEYCODE_HOME",
            "menu" => "KEYCODE_MENU",
            "enter" | "return" => "KEYCODE_ENTER",
            "tab" => "KEYCODE_TAB",
            "delete" | "del" => "KEYCODE_DEL",
            "forward-del" | "forward_del" => "KEYCODE_FORWARD_DEL",
            "move-end" | "move_end" => "KEYCODE_MOVE_END",
            "move-home" | "move_home" => "KEYCODE_MOVE_HOME",
            "space" => "KEYCODE_SPACE",
            "escape" | "esc" => "KEYCODE_ESCAPE",
            "search" => "KEYCODE_SEARCH",
            "recent" | "recents" | "app-switch" => "KEYCODE_APP_SWITCH",
            "power" => "KEYCODE_POWER",
            "volume-up" | "vol-up" => "KEYCODE_VOLUME_UP",
            "volume-down" | "vol-down" => "KEYCODE_VOLUME_DOWN",
            "volume-mute" | "vol-mute" => "KEYCODE_VOLUME_MUTE",
            "media-play" | "play" => "KEYCODE_MEDIA_PLAY",
            "media-pause" | "pause" => "KEYCODE_MEDIA_PAUSE",
            "media-play-pause" => "KEYCODE_MEDIA_PLAY_PAUSE",
            "media-next" | "next" => "KEYCODE_MEDIA_NEXT",
            "media-previous" | "media-prev" | "prev" => "KEYCODE_MEDIA_PREVIOUS",
            "page-up" => "KEYCODE_PAGE_UP",
            "page-down" => "KEYCODE_PAGE_DOWN",
            other => {
                return Ok(Err(format!(
                    "android_press: unknown key '{other}'; supported: \
                     back/home/menu/enter/tab/delete/space/escape/search/recent/power/\
                     volume-up/volume-down/volume-mute/media-play/media-pause/\
                     media-play-pause/media-next/media-previous/page-up/page-down"
                )));
            }
        };
        let serial = self.android_serial.clone();
        Ok(
            adb_run_str(serial.as_deref(), &["shell", "input", "keyevent", kc])
                .await
                .map(|_| format!("pressed {key}")),
        )
    }

    async fn android_get_ui_xml(
        &mut self,
        compressed: bool,
    ) -> HostTrapResult<Result<String, String>> {
        let serial = self.android_serial.clone();
        Ok(adb_ui_xml(serial.as_deref(), compressed).await)
    }

    async fn android_current_activity(&mut self) -> HostTrapResult<Result<String, String>> {
        let serial = self.android_serial.clone();
        // Try the focused window first (works on most Android versions
        // and matches what the user sees on screen). Fall back to the
        // resumed activity from `dumpsys activity activities` when the
        // window service doesn't expose mCurrentFocus in the expected
        // shape — happens on some single-user images and during ANR.
        if let Ok(out) = adb_run_str(
            serial.as_deref(),
            &["shell", "dumpsys", "window", "windows"],
        )
        .await
            && let Some(activity) = parse_current_focus_activity(&out)
        {
            return Ok(Ok(activity));
        }
        match adb_run_str(
            serial.as_deref(),
            &["shell", "dumpsys", "activity", "activities"],
        )
        .await
        {
            Ok(out) => match parse_resumed_activity(&out) {
                Some(activity) => Ok(Ok(activity)),
                None => Ok(Err(
                    "could not determine current activity (neither mCurrentFocus nor \
                     mResumedActivity matched in dumpsys output)"
                        .to_string(),
                )),
            },
            Err(e) => Ok(Err(format!("dumpsys activity activities failed: {e}"))),
        }
    }

    async fn android_launch_app(&mut self, pkg: String) -> HostTrapResult<Result<String, String>> {
        let serial = self.android_serial.clone();
        Ok(adb_run_str(
            serial.as_deref(),
            &[
                "shell",
                "monkey",
                "-p",
                &pkg,
                "-c",
                "android.intent.category.LAUNCHER",
                "1",
            ],
        )
        .await
        .map(|_| format!("launched {pkg}")))
    }

    async fn android_stop_app(&mut self, pkg: String) -> HostTrapResult<Result<String, String>> {
        let serial = self.android_serial.clone();
        Ok(
            adb_run_str(serial.as_deref(), &["shell", "am", "force-stop", &pkg])
                .await
                .map(|_| format!("stopped {pkg}")),
        )
    }

    async fn android_screenshot(&mut self) -> HostTrapResult<Result<String, String>> {
        let serial = self.android_serial.clone();
        // Prefer u2 /screenshot (reuses the persistent instrumentation
        // connection — no per-frame `adb exec-out screencap` child, which on
        // some mirror/scrcpy setups visibly flashes the display).
        if let Ok(b64) = u2_screenshot_b64(serial.as_deref()).await {
            return Ok(Ok(format!("data:image/png;base64,{b64}")));
        }
        let png_bytes =
            match adb_run_bytes(serial.as_deref(), &["exec-out", "screencap", "-p"]).await {
                Ok(b) => b,
                Err(e) => return Ok(Err(e)),
            };
        if png_bytes.len() < 24 {
            return Ok(Err(
                "android_screenshot: screencap returned empty/truncated data".to_string(),
            ));
        }
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
        Ok(Ok(format!("data:image/png;base64,{b64}")))
    }

    async fn android_find_elements(
        &mut self,
        selector_type: String,
        selector_value: String,
    ) -> HostTrapResult<Result<String, String>> {
        let serial = self.android_serial.clone();
        let xml = match adb_ui_xml(serial.as_deref(), false).await {
            Ok(x) => x,
            Err(e) => return Ok(Err(e)),
        };
        let elements = adb_match_elements(&xml, &selector_type, &selector_value);
        Ok(Ok(
            serde_json::to_string(&elements).unwrap_or_else(|_| "[]".to_string())
        ))
    }

    async fn android_tap_element(
        &mut self,
        selector_type: String,
        selector_value: String,
    ) -> HostTrapResult<Result<String, String>> {
        let serial = self.android_serial.clone();
        let xml = match adb_ui_xml(serial.as_deref(), false).await {
            Ok(x) => x,
            Err(e) => return Ok(Err(e)),
        };
        let elements = adb_match_elements(&xml, &selector_type, &selector_value);
        let el = match elements.first() {
            Some(e) => e.clone(),
            None => {
                return Ok(Err(format!(
                    "element not found: {selector_type}={selector_value}"
                )));
            }
        };
        let cx = el["bounds"]["centerX"].as_i64().unwrap_or(0) as u32;
        let cy = el["bounds"]["centerY"].as_i64().unwrap_or(0) as u32;
        if u2_tap(serial.as_deref(), cx, cy).await.is_ok() {
            return Ok(Ok("tapped".to_string()));
        }
        let (xs, ys) = (cx.to_string(), cy.to_string());
        Ok(
            adb_run_str(serial.as_deref(), &["shell", "input", "tap", &xs, &ys])
                .await
                .map(|_| "tapped".to_string()),
        )
    }

    async fn android_get_element_text(
        &mut self,
        selector_type: String,
        selector_value: String,
    ) -> HostTrapResult<Result<String, String>> {
        let serial = self.android_serial.clone();
        let xml = match adb_ui_xml(serial.as_deref(), false).await {
            Ok(x) => x,
            Err(e) => return Ok(Err(e)),
        };
        let elements = adb_match_elements(&xml, &selector_type, &selector_value);
        match elements.first() {
            Some(el) => Ok(Ok(el["text"].as_str().unwrap_or("").to_string())),
            None => Ok(Err(format!(
                "element not found: {selector_type}={selector_value}"
            ))),
        }
    }

    async fn android_set_element_text(
        &mut self,
        selector_type: String,
        selector_value: String,
        text: String,
    ) -> HostTrapResult<Result<String, String>> {
        // NB: no early metachar rejection here — the u2 path (below) sets text
        // via instrumentation, not the shell, so CJK/punctuation are fine. The
        // adb fallback re-checks metachars before it runs.
        let serial = self.android_serial.clone();
        // Find element and get center coords.
        let xml = match adb_ui_xml(serial.as_deref(), false).await {
            Ok(x) => x,
            Err(e) => return Ok(Err(e)),
        };
        let elements = adb_match_elements(&xml, &selector_type, &selector_value);
        let el = match elements.first() {
            Some(e) => e.clone(),
            None => {
                return Ok(Err(format!(
                    "element not found: {selector_type}={selector_value}"
                )));
            }
        };
        let cx = el["bounds"]["centerX"].as_i64().unwrap_or(0) as u32;
        let cy = el["bounds"]["centerY"].as_i64().unwrap_or(0) as u32;
        let (xs, ys) = (cx.to_string(), cy.to_string());

        // Single tap → double tap → triple tap to select all existing text.
        for _ in 0..3u8 {
            if let Err(e) =
                adb_run_str(serial.as_deref(), &["shell", "input", "tap", &xs, &ys]).await
            {
                return Ok(Err(format!("tap to focus failed: {e}")));
            }
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // The field is now focused. CJK → ADBKeyboard (works on Flutter too);
        // else u2 element/setValue; else adb input text (ASCII).
        if !text.is_ascii() {
            match adbkb_type(serial.as_deref(), &text).await {
                Ok(()) => return Ok(Ok("set".to_string())),
                Err(e) => {
                    if let Ok(()) = u2_type_focused(serial.as_deref(), &text).await {
                        return Ok(Ok("set".to_string()));
                    }
                    return Ok(Err(format!("CJK input failed: {e}")));
                }
            }
        }
        if u2_type_focused(serial.as_deref(), &text).await.is_ok() {
            return Ok(Ok("set".to_string()));
        }
        if let Some(bad) = text.chars().find(|c| ADB_INPUT_REFUSED_CHARS.contains(c)) {
            return Ok(Err(format!(
                "android_set_element_text: refusing text with shell metachar '{}'",
                bad.escape_debug()
            )));
        }
        let escaped = text.replace(' ', "%s");
        Ok(
            adb_run_str(serial.as_deref(), &["shell", "input", "text", &escaped])
                .await
                .map(|_| "set".to_string()),
        )
    }

    async fn android_element_exists(
        &mut self,
        selector_type: String,
        selector_value: String,
    ) -> HostTrapResult<Result<bool, String>> {
        let serial = self.android_serial.clone();
        let xml = match adb_ui_xml(serial.as_deref(), false).await {
            Ok(x) => x,
            Err(e) => return Ok(Err(e)),
        };
        let elements = adb_match_elements(&xml, &selector_type, &selector_value);
        Ok(Ok(!elements.is_empty()))
    }

    async fn android_wait_for_element(
        &mut self,
        selector_type: String,
        selector_value: String,
        timeout_ms: u32,
    ) -> HostTrapResult<Result<String, String>> {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(u64::from(timeout_ms));
        let serial = self.android_serial.clone();
        // Surface ADB failure after a small streak — otherwise a
        // disconnected device or a wedged uiautomator service silently
        // burns the entire timeout returning "timeout" instead of the
        // real error (much harder to diagnose from a plugin caller).
        const MAX_CONSECUTIVE_ADB_FAILURES: u8 = 3;
        let mut consecutive_failures: u8 = 0;
        let mut last_err: Option<String> = None;
        loop {
            match adb_ui_xml(serial.as_deref(), false).await {
                Ok(xml) => {
                    consecutive_failures = 0;
                    if !adb_match_elements(&xml, &selector_type, &selector_value).is_empty() {
                        return Ok(Ok("found".to_string()));
                    }
                }
                Err(e) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    last_err = Some(e);
                    if consecutive_failures >= MAX_CONSECUTIVE_ADB_FAILURES {
                        return Ok(Err(format!(
                            "android_wait_for_element: adb ui dump failed {} times in a row: {}",
                            consecutive_failures,
                            last_err.unwrap_or_default()
                        )));
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                let suffix = match last_err {
                    Some(e) => format!(" (last error: {e})"),
                    None => String::new(),
                };
                return Ok(Err(format!(
                    "timeout waiting for {selector_type}={selector_value} after {timeout_ms}ms{suffix}"
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    async fn android_tap_yellow_button(
        &mut self,
        y_min: u32,
        y_max: u32,
    ) -> HostTrapResult<Result<String, String>> {
        let serial = self.android_serial.clone();
        // Raw screencap (RGBA, no PNG decode): 12- or 16-byte header (w,h,format
        // [,colorspace]) then width*height*4 bytes.
        let raw = match adb_run_bytes(serial.as_deref(), &["exec-out", "screencap"]).await {
            Ok(b) => b,
            Err(e) => return Ok(Err(format!("screencap: {e}"))),
        };
        if raw.len() < 16 {
            return Ok(Err("screencap: data too small".to_string()));
        }
        let rd = |i: usize| {
            u32::from_le_bytes([raw[i], raw[i + 1], raw[i + 2], raw[i + 3]]) as usize
        };
        let (w, h) = (rd(0), rd(4));
        if w == 0 || h == 0 || w > 20000 || h > 20000 {
            return Ok(Err(format!("screencap: bad header {w}x{h}")));
        }
        let body = w * h * 4;
        let hdr = if raw.len() >= 16 + body {
            16
        } else if raw.len() >= 12 + body {
            12
        } else {
            return Ok(Err("screencap: truncated body".to_string()));
        };
        let data = &raw[hdr..hdr + body];
        let ymin = y_min as usize;
        let ymax = if y_max == 0 || (y_max as usize) > h {
            h
        } else {
            y_max as usize
        };
        // Centroid of brand-yellow pixels (R~255,G~215,B~0). Sample every 2px.
        let (mut sx, mut sy, mut n): (u64, u64, u64) = (0, 0, 0);
        let mut y = ymin;
        while y < ymax {
            let row = y * w * 4;
            let mut x = 0;
            while x < w {
                let p = row + x * 4;
                let (r, g, b) = (data[p] as i32, data[p + 1] as i32, data[p + 2] as i32);
                if r > 230 && (185..235).contains(&g) && b < 95 {
                    sx += x as u64;
                    sy += y as u64;
                    n += 1;
                }
                x += 2;
            }
            y += 2;
        }
        if n < 80 {
            return Ok(Err("android_tap_yellow_button: no yellow button found".to_string()));
        }
        let (cx, cy) = ((sx / n) as u32, (sy / n) as u32);
        match adb_run_str(
            serial.as_deref(),
            &["shell", "input", "tap", &cx.to_string(), &cy.to_string()],
        )
        .await
        {
            Ok(_) => Ok(Ok(format!("tapped:{cx},{cy}"))),
            Err(e) => Ok(Err(format!("tap failed: {e}"))),
        }
    }

    async fn android_reset_uiautomator(&mut self) -> HostTrapResult<Result<String, String>> {
        let serial = self.android_serial.clone();
        let sref = serial.as_deref();
        // force-stop is best-effort: the packages may already be dead/wedged
        // in a state where the command itself errors, which is fine — the
        // point is just to make sure nothing stale is left running before we
        // relaunch via u2_ensure below.
        for pkg in [
            "io.appium.uiautomator2.server",
            "io.appium.uiautomator2.server.test",
        ] {
            let _ = adb_run_str(sref, &["shell", "am", "force-stop", pkg]).await;
        }
        // Drop the cached connection so u2_ensure does a full relaunch rather
        // than reusing (now-dead) state; this also drops the held Child
        // handle, whose kill_on_drop reaps the old instrumentation process.
        let key = sref.unwrap_or("").to_string();
        u2_conns().lock().await.remove(&key);
        Ok(u2_ensure(sref).await.map(|_| "reset".to_string()))
    }
}

// ---------------------------------------------------------------------------
// host-ios trait implementation (WebDriverAgent)
// ---------------------------------------------------------------------------

impl rsclaw::plugin::host_ios::Host for HostState {
    async fn ios_connect(
        &mut self,
        bundle_id: Option<String>,
    ) -> HostTrapResult<Result<String, String>> {
        let base = std::env::var("RSCLAW_IOS_WDA_URL")
            .unwrap_or_else(|_| "http://localhost:8100".to_string());
        
        // Reuse existing session if available
        if let Some(ref existing_url) = self.wda_url {
            if existing_url.starts_with(&base) {
                return Ok(Ok(base));
            }
        }
        
        let cli = match host_http_client() {
            Ok(c) => c,
            Err(e) => return Ok(Err(e)),
        };
        let resp = match cli.get(format!("{base}/status")).send().await {
            Ok(r) => r,
            Err(e) => return Ok(Err(format!("WDA status: {e}"))),
        };
        if !resp.status().is_success() {
            return Ok(Err(format!("WDA status returned {}", resp.status())));
        }
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return Ok(Err(format!("WDA status decode: {e}"))),
        };
        let session_id = body
            .pointer("/value/currentSession")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !session_id.is_empty() {
            self.wda_url = Some(format!("{base}/session/{session_id}"));
        } else {
            // Create a new session (W3C WebDriver format)
            let payload = serde_json::json!({
                "capabilities": {
                    "alwaysMatch": {
                        "bundleId": bundle_id.as_deref().unwrap_or("com.apple.springboard"),
                    }
                }
            });
            let r = match cli
                .post(format!("{base}/session"))
                .json(&payload)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => return Ok(Err(format!("WDA create session: {e}"))),
            };
            if !r.status().is_success() {
                let text = r.text().await.unwrap_or_else(|_| "unknown".to_string());
                return Ok(Err(format!("WDA create session {text}")));
            }
            let session_body: serde_json::Value = match r.json().await {
                Ok(v) => v,
                Err(e) => return Ok(Err(format!("WDA session decode: {e}"))),
            };
            let sid = session_body
                .pointer("/value/sessionId")
                .or_else(|| session_body.pointer("/sessionId"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if sid.is_empty() {
                return Ok(Err(
                    "WDA create session: no sessionId in response".to_string()
                ));
            }
            self.wda_url = Some(format!("{base}/session/{sid}"));
        }
        Ok(Ok(base))
    }

    async fn ios_find_elements(
        &mut self,
        selector_type: String,
        selector_value: String,
    ) -> HostTrapResult<Result<String, String>> {
        let (base, cli) = self.wda_base_and_client();
        let payload = serde_json::json!({"using": selector_type, "value": selector_value});
        let resp = match cli
            .post(format!("{base}/element"))
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return Ok(Err(format!("WDA find: {e}"))),
        };
        if !resp.status().is_success() {
            return Ok(Err(format!("WDA find returned {}", resp.status())));
        }
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return Ok(Err(format!("WDA find decode: {e}"))),
        };
        let elements = body.pointer("/value").cloned().unwrap_or(body);
        Ok(Ok(elements.to_string()))
    }

    async fn ios_tap_element(
        &mut self,
        selector_type: String,
        selector_value: String,
    ) -> HostTrapResult<Result<String, String>> {
        let (base, cli) = self.wda_base_and_client();
        // 1. Find the element
        let payload = serde_json::json!({"using": selector_type, "value": selector_value});
        let resp = match cli
            .post(format!("{base}/element"))
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return Ok(Err(format!("WDA find element: {e}"))),
        };
        if !resp.status().is_success() {
            return Ok(Err(format!("WDA find returned {}", resp.status())));
        }
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return Ok(Err(format!("WDA find decode: {e}"))),
        };
        let elem_id = match body.pointer("/value/ELEMENT").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(Err("element not found".to_string())),
        };
        // 2. Get element rect (this WDA version does not support /element/{id}/click)
        let rect_resp = match cli
            .get(format!("{base}/element/{elem_id}/rect"))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return Ok(Err(format!("WDA rect: {e}"))),
        };
        if !rect_resp.status().is_success() {
            return Ok(Err(format!("WDA rect returned {}", rect_resp.status())));
        }
        let rect_body: serde_json::Value = match rect_resp.json().await {
            Ok(v) => v,
            Err(e) => return Ok(Err(format!("WDA rect decode: {e}"))),
        };
        let x = rect_body["value"]["x"].as_f64().unwrap_or(0.0);
        let y = rect_body["value"]["y"].as_f64().unwrap_or(0.0);
        let w = rect_body["value"]["width"].as_f64().unwrap_or(0.0);
        let h = rect_body["value"]["height"].as_f64().unwrap_or(0.0);
        let cx = x + w / 2.0;
        let cy = y + h / 2.0;
        // 3. Tap via coordinate-based wda/tap (works on this WDA version)
        let tap_resp = match cli
            .post(format!("{base}/wda/tap"))
            .json(&serde_json::json!({"x": cx, "y": cy}))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return Ok(Err(format!("WDA tap: {e}"))),
        };
        if tap_resp.status().is_success() {
            Ok(Ok("tapped".to_string()))
        } else {
            Ok(Err(format!("WDA tap returned {}", tap_resp.status())))
        }
    }

    async fn ios_tap(
        &mut self,
        x: f64,
        y: f64,
    ) -> HostTrapResult<Result<String, String>> {
        let (base, cli) = self.wda_base_and_client();
        // Use the sessionless `/wda/tap` with the coordinates in the JSON body —
        // the `/wda/tap/{x}/{y}` path form returns 404 on this WDA build.
        let payload = serde_json::json!({"x": x, "y": y});
        let resp = match cli
            .post(format!("{base}/wda/tap"))
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return Ok(Err(format!("WDA tap: {e}"))),
        };
        if resp.status().is_success() {
            Ok(Ok("tapped".to_string()))
        } else {
            Ok(Err(format!("WDA tap returned {}", resp.status())))
        }
    }

    async fn ios_type(
        &mut self,
        text: String,
    ) -> HostTrapResult<Result<String, String>> {
        let (base, cli) = self.wda_base_and_client();
        let payload = serde_json::json!({"value": [text]});
        let resp = match cli
            .post(format!("{base}/wda/keys"))
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return Ok(Err(format!("WDA type: {e}"))),
        };
        if resp.status().is_success() {
            Ok(Ok("typed".to_string()))
        } else {
            Ok(Err(format!("WDA type returned {}", resp.status())))
        }
    }

    async fn ios_swipe(
        &mut self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        duration_ms: u32,
    ) -> HostTrapResult<Result<String, String>> {
        let (base, cli) = self.wda_base_and_client();
        let payload = serde_json::json!({
            "fromX": x1, "fromY": y1,
            "toX": x2, "toY": y2,
            "duration": duration_ms as f64 / 1000.0,
        });
        let resp = match cli
            .post(format!("{base}/wda/dragfromtoforduration"))
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return Ok(Err(format!("WDA drag: {e}"))),
        };
        if resp.status().is_success() {
            Ok(Ok("swiped".to_string()))
        } else {
            Ok(Err(format!("WDA drag returned {}", resp.status())))
        }
    }

    async fn ios_get_labels(&mut self) -> HostTrapResult<Result<String, String>> {
        let (base, cli) = self.wda_base_and_client();
        let resp = match cli
            .get(format!("{base}/source"))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return Ok(Err(format!("WDA source: {e}"))),
        };
        if !resp.status().is_success() {
            return Ok(Err(format!("WDA source returned {}", resp.status())));
        }
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return Ok(Err(format!("WDA source decode: {e}"))),
        };
        let xml = body
            .pointer("/value")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        Ok(Ok(xml.to_string()))
    }

    async fn ios_screenshot(&mut self) -> HostTrapResult<Result<String, String>> {
        let (base, cli) = self.wda_base_and_client();
        let resp = match cli.get(format!("{base}/screenshot")).send().await {
            Ok(r) => r,
            Err(e) => return Ok(Err(format!("WDA screenshot: {e}"))),
        };
        if !resp.status().is_success() {
            return Ok(Err(format!("WDA screenshot returned {}", resp.status())));
        }
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return Ok(Err(format!("WDA screenshot decode: {e}"))),
        };
        let png_b64 = body
            .pointer("/value")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        Ok(Ok(format!("data:image/png;base64,{png_b64}")))
    }

    async fn ios_screen_size(&mut self) -> HostTrapResult<Result<String, String>> {
        let (base, cli) = self.wda_base_and_client();
        let resp = match cli.get(format!("{base}/window/size")).send().await {
            Ok(r) => r,
            Err(e) => return Ok(Err(format!("WDA window size: {e}"))),
        };
        if !resp.status().is_success() {
            return Ok(Err(format!("WDA window size returned {}", resp.status())));
        }
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return Ok(Err(format!("WDA size decode: {e}"))),
        };
        Ok(Ok(body.pointer("/value").cloned().unwrap_or(body).to_string()))
    }

    async fn ios_press_button(
        &mut self,
        name: String,
    ) -> HostTrapResult<Result<String, String>> {
        let (base, cli) = self.wda_base_and_client();
        let payload = serde_json::json!({"name": name});
        let resp = match cli
            .post(format!("{base}/wda/pressButton"))
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return Ok(Err(format!("WDA pressButton: {e}"))),
        };
        if resp.status().is_success() {
            Ok(Ok("pressed".to_string()))
        } else {
            Ok(Err(format!("WDA pressButton returned {}", resp.status())))
        }
    }

    async fn ios_current_app(&mut self) -> HostTrapResult<Result<String, String>> {
        let (base, cli) = self.wda_base_and_client();
        let resp = match cli
            .get(format!("{base}/wda/activeAppInfo"))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return Ok(Err(format!("WDA activeApp: {e}"))),
        };
        if !resp.status().is_success() {
            return Ok(Err(format!("WDA activeApp returned {}", resp.status())));
        }
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return Ok(Err(format!("WDA activeApp decode: {e}"))),
        };
        let bundle = body
            .pointer("/value/bundleId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        Ok(Ok(bundle.to_string()))
    }

    async fn ios_launch_app(
        &mut self,
        bundle_id: String,
    ) -> HostTrapResult<Result<String, String>> {
        let (base, cli) = self.wda_base_and_client();
        let payload = serde_json::json!({"bundleId": bundle_id});
        let resp = match cli
            .post(format!("{base}/wda/apps/launch"))
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return Ok(Err(format!("WDA launch: {e}"))),
        };
        if resp.status().is_success() {
            Ok(Ok("launched".to_string()))
        } else {
            Ok(Err(format!("WDA launch returned {}", resp.status())))
        }
    }

    async fn ios_terminate_app(
        &mut self,
        bundle_id: String,
    ) -> HostTrapResult<Result<String, String>> {
        let (base, cli) = self.wda_base_and_client();
        let payload = serde_json::json!({"bundleId": bundle_id});
        let resp = match cli
            .post(format!("{base}/wda/apps/terminate"))
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return Ok(Err(format!("WDA terminate: {e}"))),
        };
        if resp.status().is_success() {
            Ok(Ok("terminated".to_string()))
        } else {
            Ok(Err(format!("WDA terminate returned {}", resp.status())))
        }
    }
}

impl HostState {
    fn wda_base_and_client(&self) -> (String, reqwest::Client) {
        let base = self
            .wda_url
            .as_ref()
            .cloned()
            .unwrap_or_else(|| "http://localhost:8100".to_string());
        let cli = host_http_client().unwrap_or_else(|_| {
            reqwest::Client::builder()
                .build()
                .expect("failed to build reqwest client")
        });
        (base, cli)
    }
}

/// Parse `mCurrentFocus=Window{xxxx [u0 ]<package>/<Activity>}` out of
/// `dumpsys window windows`. Returns the `<package>/<Activity>` slug
/// without the surrounding `Window{...}` envelope. Handles both the
/// multi-user (`u0 `) and single-user (no `u0`) shapes.
fn parse_current_focus_activity(dumpsys_output: &str) -> Option<String> {
    for line in dumpsys_output.lines() {
        if !line.contains("mCurrentFocus") {
            continue;
        }
        let open = line.find('{')?;
        let close = line[open..].find('}').map(|r| open + r)?;
        let inside = &line[open + 1..close];
        // The activity is the last whitespace-separated token; on multi-
        // user images it's preceded by a `u<N>` marker, on single-user
        // images it follows the hash directly. Both shapes resolve by
        // taking the trailing token that contains `/`.
        let tok = inside
            .split_whitespace()
            .rfind(|t| t.contains('/'))
            .map(str::trim)
            .filter(|s| !s.is_empty())?;
        return Some(tok.to_string());
    }
    None
}

/// Parse `mResumedActivity: ActivityRecord{xxxx u0 <package>/<Activity> ...}`
/// out of `dumpsys activity activities`. Used as a fallback when
/// `mCurrentFocus` parsing didn't resolve.
fn parse_resumed_activity(dumpsys_output: &str) -> Option<String> {
    for line in dumpsys_output.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("mResumedActivity") {
            continue;
        }
        let open = trimmed.find('{')?;
        let close = trimmed[open..].find('}').map(|r| open + r)?;
        let inside = &trimmed[open + 1..close];
        let tok = inside
            .split_whitespace()
            .find(|t| t.contains('/'))
            .map(str::trim)
            .filter(|s| !s.is_empty())?;
        return Some(tok.to_string());
    }
    None
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Build a `Linker<HostState>` with all host functions registered.
fn build_linker(engine: &Engine) -> Result<Linker<HostState>> {
    let mut linker = Linker::new(engine);
    // Add WASI interfaces (io, filesystem, etc.) required by wasm32-wasip2
    // components.
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)
        .map_err(|e| anyhow::anyhow!("failed to add WASI linker interfaces: {e}"))?;
    // Add our custom host interfaces.
    rsclaw::plugin::host_browser::add_to_linker::<
        HostState,
        wasmtime::component::HasSelf<HostState>,
    >(&mut linker, |state: &mut HostState| state)
    .map_err(|e| anyhow::anyhow!("failed to add host-browser linker interfaces: {e}"))?;
    rsclaw::plugin::host_runtime::add_to_linker::<
        HostState,
        wasmtime::component::HasSelf<HostState>,
    >(&mut linker, |state: &mut HostState| state)
    .map_err(|e| anyhow::anyhow!("failed to add host-runtime linker interfaces: {e}"))?;
    rsclaw::plugin::host_config::add_to_linker::<
        HostState,
        wasmtime::component::HasSelf<HostState>,
    >(&mut linker, |state: &mut HostState| state)
    .map_err(|e| anyhow::anyhow!("failed to add host-config linker interfaces: {e}"))?;
    rsclaw::plugin::host_context::add_to_linker::<
        HostState,
        wasmtime::component::HasSelf<HostState>,
    >(&mut linker, |state: &mut HostState| state)
    .map_err(|e| anyhow::anyhow!("failed to add host-context linker interfaces: {e}"))?;
    rsclaw::plugin::host_http::add_to_linker::<
        HostState,
        wasmtime::component::HasSelf<HostState>,
    >(&mut linker, |state: &mut HostState| state)
    .map_err(|e| anyhow::anyhow!("failed to add host-http linker interfaces: {e}"))?;
    rsclaw::plugin::host_kv::add_to_linker::<
        HostState,
        wasmtime::component::HasSelf<HostState>,
    >(&mut linker, |state: &mut HostState| state)
    .map_err(|e| anyhow::anyhow!("failed to add host-kv linker interfaces: {e}"))?;
    rsclaw::plugin::host_device::add_to_linker::<
        HostState,
        wasmtime::component::HasSelf<HostState>,
    >(&mut linker, |state: &mut HostState| state)
    .map_err(|e| anyhow::anyhow!("failed to add host-device linker interfaces: {e}"))?;
    rsclaw::plugin::host_background::add_to_linker::<
        HostState,
        wasmtime::component::HasSelf<HostState>,
    >(&mut linker, |state: &mut HostState| state)
    .map_err(|e| anyhow::anyhow!("failed to add host-background linker interfaces: {e}"))?;
    rsclaw::plugin::host_storage::add_to_linker::<
        HostState,
        wasmtime::component::HasSelf<HostState>,
    >(&mut linker, |state: &mut HostState| state)
    .map_err(|e| anyhow::anyhow!("failed to add host-storage linker interfaces: {e}"))?;
    rsclaw::plugin::host_media::add_to_linker::<
        HostState,
        wasmtime::component::HasSelf<HostState>,
    >(&mut linker, |state: &mut HostState| state)
    .map_err(|e| anyhow::anyhow!("failed to add host-media linker interfaces: {e}"))?;
    rsclaw::plugin::host_desktop::add_to_linker::<
        HostState,
        wasmtime::component::HasSelf<HostState>,
    >(&mut linker, |state: &mut HostState| state)
    .map_err(|e| anyhow::anyhow!("failed to add host-desktop linker interfaces: {e}"))?;
    rsclaw::plugin::host_vlm::add_to_linker::<HostState, wasmtime::component::HasSelf<HostState>>(
        &mut linker,
        |state: &mut HostState| state,
    )
    .map_err(|e| anyhow::anyhow!("failed to add host-vlm linker interfaces: {e}"))?;
    rsclaw::plugin::host_android::add_to_linker::<
        HostState,
        wasmtime::component::HasSelf<HostState>,
    >(&mut linker, |state: &mut HostState| state)
    .map_err(|e| anyhow::anyhow!("failed to add host-android linker interfaces: {e}"))?;
    rsclaw::plugin::host_ios::add_to_linker::<
        HostState,
        wasmtime::component::HasSelf<HostState>,
    >(&mut linker, |state: &mut HostState| state)
    .map_err(|e| anyhow::anyhow!("failed to add host-ios linker interfaces: {e}"))?;
    Ok(linker)
}

/// Load a WASM plugin from a `PluginManifest`.
///
/// The manifest's `entry` field points to the `.wasm` file relative to the
/// plugin directory. We compile the component and pre-build the linker, but
/// do *not* instantiate — tools come from `plugin.json5`, which is the single
/// source of truth.
pub async fn load_wasm_plugin(
    manifest: &super::manifest::PluginManifest,
    engine: &Engine,
    browser: Arc<Mutex<Option<BrowserSession>>>,
    providers: Option<Arc<rsclaw_provider::registry::ProviderRegistry>>,
    vision_model: Option<String>,
) -> Result<WasmPlugin> {
    let path = manifest.entry_path();
    let wasm_bytes = std::fs::read(&path)
        .with_context(|| format!("failed to read WASM file: {}", path.display()))?;
    verify_wasm_integrity(manifest.integrity.as_deref(), &wasm_bytes)
        .with_context(|| format!("WASM integrity check failed: {}", path.display()))?;

    let component = Component::new(engine, &wasm_bytes).map_err(|e| {
        anyhow::anyhow!("failed to compile WASM component: {}: {e}", path.display())
    })?;

    let linker = build_linker(engine)?;

    let tools = manifest
        .tools
        .iter()
        .map(|t| WasmToolDef {
            name: t.name.clone(),
            description: t.description.clone(),
            parameters: t.input_schema.clone().unwrap_or(json!({"type": "object"})),
            headline: t.headline,
            group: t.group.clone(),
        })
        .collect();

    Ok(WasmPlugin {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        description: manifest.description.clone(),
        summary: manifest.summary.clone(),
        common_tools: manifest.common_tools.clone(),
        tools,
        tool_groups: manifest.tool_groups.clone(),
        wasm_path: path.to_path_buf(),
        engine: engine.clone(),
        component,
        linker,
        browser,
        browser_cdn_rules: manifest.browser_cdn.download_rules.clone(),
        plugin_config: resolve_plugin_config(&manifest.config),
        capabilities: manifest.capabilities.clone(),
        slash_commands: manifest.slash_commands.clone(),
        tool_aliases: manifest.tool_aliases.clone(),
        min_call_interval: Duration::from_millis(u64::from(manifest.min_call_interval_ms)),
        last_call: Mutex::new(None),
        providers,
        vision_model,
    })
}

fn verify_wasm_integrity(integrity: Option<&str>, bytes: &[u8]) -> Result<()> {
    let Some(raw) = integrity.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    let expected = raw
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow::anyhow!("unsupported integrity format `{raw}`"))?;
    let actual = sha256_hex(bytes);
    if !expected.eq_ignore_ascii_case(&actual) {
        anyhow::bail!("sha256 mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

// ---------------------------------------------------------------------------
// Tool dispatch
// ---------------------------------------------------------------------------

impl WasmPlugin {
    /// Dispatch a tool call to this WASM plugin.
    ///
    /// The tool name must match one of the plugin's declared tools.
    /// Arguments are passed as a JSON value and the result is returned
    /// as a JSON value.
    /// Convenience: dispatch without a notify routing context (e.g. when
    /// invoked via /api/v1/tools/execute for debugging). `host::notify`
    /// calls fall back to trace logging only.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.call_tool_with_ctx(tool_name, args, None).await
    }

    pub async fn call_tool_with_ctx(
        &self,
        tool_name: &str,
        args: serde_json::Value,
        notify_ctx: Option<WasmNotifyCtx>,
    ) -> Result<serde_json::Value> {
        // Verify the tool exists in this plugin's manifest.
        let _tool_def = self
            .tools
            .iter()
            .find(|t| t.name == tool_name)
            .with_context(|| {
                format!(
                    "tool '{}' not found in WASM plugin '{}'",
                    tool_name, self.name
                )
            })?;

        debug!(plugin = %self.name, tool = tool_name, "dispatching WASM tool call");

        // Host-side rate limit: hold off until the configured interval has
        // elapsed since the previous call. Replaces per-plugin sleeps in
        // dispatch code.
        if !self.min_call_interval.is_zero() {
            let mut last = self.last_call.lock().await;
            if let Some(t) = *last {
                let elapsed = t.elapsed();
                if elapsed < self.min_call_interval {
                    tokio::time::sleep(self.min_call_interval - elapsed).await;
                }
            }
            *last = Some(Instant::now());
        }

        // Fresh store per call for isolation, with memory cap and epoch deadline.
        let mut store = new_sandboxed_store(
            &self.engine,
            Arc::clone(&self.browser),
            notify_ctx,
            self.browser_cdn_rules.clone(),
            self.name.clone(),
            self.plugin_config.clone(),
            self.providers.clone(),
            self.vision_model.clone(),
        );

        let instance = self
            .linker
            .instantiate_async(&mut store, &self.component)
            .await
            .map_err(|e| anyhow::anyhow!("failed to instantiate component for tool call: {e}"))?;

        // Drill into the plugin-api interface to find handle-tool.
        let iface_idx = instance
            .get_export_index(&mut store, None, "rsclaw:plugin/plugin-api")
            .with_context(|| "plugin-api interface not found")?;

        let handle_tool_idx = instance
            .get_export_index(&mut store, Some(&iface_idx), "handle-tool")
            .with_context(|| "handle-tool export not found")?;

        let handle_tool_fn = instance
            .get_typed_func::<(&str, &str), (Result<String, String>,)>(&mut store, &handle_tool_idx)
            .map_err(|e| anyhow::anyhow!("handle-tool has unexpected type: {e}"))?;

        let args_json =
            serde_json::to_string(&args).context("failed to serialize tool arguments")?;

        let (result,) = handle_tool_fn
            .call_async(&mut store, (tool_name, &args_json))
            .await
            .map_err(|e| anyhow::anyhow!("handle-tool call failed for '{tool_name}': {e}"))?;

        match result {
            Ok(json_str) => {
                let value: serde_json::Value =
                    serde_json::from_str(&json_str).with_context(|| {
                        format!("invalid JSON result from tool '{tool_name}': {json_str}")
                    })?;
                Ok(value)
            }
            Err(err_str) => {
                bail!(
                    "WASM plugin '{}' tool '{}' returned error: {}",
                    self.name,
                    tool_name,
                    err_str
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — pure parsing helpers (no device required)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod android_helper_tests {
    use super::*;

    #[test]
    fn host_http_tls_provider_init_is_idempotent() {
        ensure_host_http_tls_provider().expect("first TLS provider init");
        ensure_host_http_tls_provider().expect("second TLS provider init");
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }

    #[test]
    fn host_http_client_builds_with_rustls_roots() {
        let client = host_http_client().expect("host HTTP client");
        drop(client);
    }

    #[test]
    fn wasm_integrity_accepts_matching_sha256() {
        let bytes = b"rsclaw plugin";
        let integrity = format!("sha256:{}", sha256_hex(bytes));
        verify_wasm_integrity(Some(&integrity), bytes).expect("matching integrity");
        verify_wasm_integrity(None, bytes).expect("missing integrity stays optional");
    }

    #[test]
    fn wasm_integrity_rejects_mismatch_and_unknown_format() {
        let bytes = b"rsclaw plugin";
        assert!(verify_wasm_integrity(Some("sha256:deadbeef"), bytes).is_err());
        assert!(verify_wasm_integrity(Some("sha512:deadbeef"), bytes).is_err());
    }

    #[test]
    fn plugin_sql_policy_allows_basic_safe_shapes() {
        assert!(validate_plugin_sql("select code, price from quotes where code = ?1", PluginSqlKind::Query).is_ok());
        assert!(validate_plugin_sql("with ranked as (select code from quotes) select * from ranked", PluginSqlKind::Query).is_ok());
        assert!(validate_plugin_sql("create table if not exists quotes (code text primary key, price real)", PluginSqlKind::Execute).is_ok());
        assert!(validate_plugin_sql("insert into quotes (code, price) values (?1, ?2)", PluginSqlKind::Execute).is_ok());
        assert!(validate_plugin_sql("update quotes set price = ?2 where code = ?1", PluginSqlKind::Execute).is_ok());
        assert!(validate_plugin_sql("delete from quotes where code = ?1", PluginSqlKind::Execute).is_ok());
    }

    #[test]
    fn plugin_sql_policy_ignores_blocked_words_inside_literals() {
        assert!(validate_plugin_sql("select 'drop table kv; attach database x' as text", PluginSqlKind::Query).is_ok());
        assert!(validate_plugin_sql("insert into notes (body) values ('pragma kv attach')", PluginSqlKind::Execute).is_ok());
    }

    #[test]
    fn plugin_sql_policy_blocks_dangerous_shapes() {
        for sql in [
            "select * from kv",
            "drop table quotes",
            "attach database '/tmp/x.db' as x",
            "pragma writable_schema = on",
            "select * from quotes; drop table quotes",
            "with x as (select 1) delete from quotes",
        ] {
            assert!(validate_plugin_sql(sql, PluginSqlKind::Query).is_err(), "{sql}");
        }
        for sql in [
            "delete from kv where key = ?1",
            "create index idx_quotes_code on quotes(code)",
            "alter table quotes add column x text",
            "vacuum",
        ] {
            assert!(validate_plugin_sql(sql, PluginSqlKind::Execute).is_err(), "{sql}");
        }
    }

    #[tokio::test]
    async fn host_http_url_allows_public_http_ip_literals() {
        assert!(validate_host_http_url("https://8.8.8.8/path").await.is_ok());
        assert!(validate_host_http_url("http://1.1.1.1:8080/path").await.is_ok());
    }

    #[tokio::test]
    async fn host_http_url_rejects_ssrf_ip_literals() {
        for url in [
            "http://127.0.0.1:18888/api/v1/health",
            "http://10.0.0.1/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
            "http://[fc00::1]/",
            "http://[fe80::1]/",
            "http://[::ffff:127.0.0.1]/",
        ] {
            assert!(validate_host_http_url(url).await.is_err(), "{url}");
        }
    }

    #[tokio::test]
    async fn host_http_url_rejects_unsafe_shapes_before_request() {
        for url in [
            "file:///etc/passwd",
            "ftp://example.com/file",
            "https://user:pass@example.com/",
            "http://localhost/",
            "http://api.localhost/",
        ] {
            assert!(validate_host_http_url(url).await.is_err(), "{url}");
        }
    }

    #[test]
    fn browser_upload_path_is_limited_to_allowed_roots() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rsclaw-browser-upload-path-test-{}-{unique}",
            std::process::id()
        ));
        let workspace = root.join("workspace");
        let plugin_var = root.join("var").join("plugins").join("sample");
        let downloads_rsclaw = root.join("Downloads").join("rsclaw");
        let outside = root.join(".ssh");
        std::fs::create_dir_all(&workspace).expect("workspace dir");
        std::fs::create_dir_all(&plugin_var).expect("plugin var dir");
        std::fs::create_dir_all(&downloads_rsclaw).expect("downloads dir");
        std::fs::create_dir_all(&outside).expect("outside dir");

        let workspace_file = workspace.join("upload.txt");
        let plugin_file = plugin_var.join("upload.txt");
        let downloads_file = downloads_rsclaw.join("upload.txt");
        let outside_file = outside.join("id_rsa");
        std::fs::write(&workspace_file, "workspace").expect("workspace file");
        std::fs::write(&plugin_file, "plugin").expect("plugin file");
        std::fs::write(&downloads_file, "download").expect("download file");
        std::fs::write(&outside_file, "secret").expect("outside file");

        let roots = [workspace.clone(), plugin_var, downloads_rsclaw];
        assert_eq!(
            canonicalize_existing_file_in_roots("upload.txt", &workspace, &roots, "browser_upload")
                .expect("workspace upload"),
            std::fs::canonicalize(&workspace_file).expect("workspace canonical")
        );
        assert!(
            canonicalize_existing_file_in_roots(
                plugin_file.to_string_lossy().as_ref(),
                &workspace,
                &roots,
                "browser_upload"
            )
            .is_ok()
        );
        assert!(
            canonicalize_existing_file_in_roots(
                downloads_file.to_string_lossy().as_ref(),
                &workspace,
                &roots,
                "browser_upload"
            )
            .is_ok()
        );
        assert!(
            canonicalize_existing_file_in_roots(
                outside_file.to_string_lossy().as_ref(),
                &workspace,
                &roots,
                "browser_upload"
            )
            .is_err()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn browser_upload_path_rejects_symlink_escape() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rsclaw-browser-upload-symlink-test-{}-{unique}",
            std::process::id()
        ));
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&workspace).expect("workspace dir");
        std::fs::create_dir_all(&outside).expect("outside dir");
        let outside_file = outside.join("secret.txt");
        let link_path = workspace.join("linked-secret.txt");
        std::fs::write(&outside_file, "secret").expect("outside file");
        std::os::unix::fs::symlink(&outside_file, &link_path).expect("symlink");

        let roots = [workspace.clone()];
        assert!(
            canonicalize_existing_file_in_roots(
                "linked-secret.txt",
                &workspace,
                &roots,
                "browser_upload"
            )
            .is_err()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn xml_unescape_handles_common_entities() {
        assert_eq!(adb_xml_unescape("plain"), "plain");
        assert_eq!(adb_xml_unescape("Don&apos;t"), "Don't");
        assert_eq!(adb_xml_unescape("a&amp;b"), "a&b");
        assert_eq!(
            adb_xml_unescape("quote &quot; lt &lt; gt &gt;"),
            "quote \" lt < gt >"
        );
        assert_eq!(adb_xml_unescape("line1&#10;line2"), "line1\nline2");
        assert_eq!(adb_xml_unescape("line1&#xA;line2"), "line1\nline2");
    }

    #[test]
    fn xml_attr_extracts_quoted_value() {
        let node =
            r#"<node text="hello world" resource-id="com.x:id/foo" bounds="[0,0][100,200]">"#;
        assert_eq!(adb_xml_attr(node, "text"), "hello world");
        assert_eq!(adb_xml_attr(node, "resource-id"), "com.x:id/foo");
        assert_eq!(adb_xml_attr(node, "bounds"), "[0,0][100,200]");
        assert_eq!(adb_xml_attr(node, "missing"), "");
    }

    #[test]
    fn bounds_center_handles_typical_shape() {
        assert_eq!(adb_bounds_center("[0,0][100,200]"), (50, 100));
        assert_eq!(adb_bounds_center("[10,20][50,60]"), (30, 40));
    }

    #[test]
    fn bounds_center_handles_malformed() {
        // Missing one number → defaults to (0,0) rather than panicking.
        assert_eq!(adb_bounds_center("[0,0]"), (0, 0));
        assert_eq!(adb_bounds_center(""), (0, 0));
        assert_eq!(adb_bounds_center("garbage"), (0, 0));
    }

    #[test]
    fn match_elements_decodes_text_attribute() {
        let xml = concat!(
            "<?xml version='1.0' encoding='UTF-8' standalone='yes'?>",
            "<hierarchy rotation=\"0\">",
            "<node text=\"Don&apos;t panic\" resource-id=\"id1\" content-desc=\"\" ",
            "class=\"android.widget.TextView\" bounds=\"[0,0][100,40]\" clickable=\"false\"/>",
            "</hierarchy>"
        );
        // text-contains should hit on the decoded apostrophe form, not the
        // raw &apos; sequence — the bug-fix case for the unescape change.
        let hits = adb_match_elements(xml, "text-contains", "Don't");
        assert_eq!(hits.len(), 1, "expected one match, got: {hits:?}");
        assert_eq!(hits[0]["text"].as_str(), Some("Don't panic"));
        // And the raw escaped form should NOT match anymore.
        let no_hits = adb_match_elements(xml, "text-contains", "Don&apos;t");
        assert!(no_hits.is_empty());
    }

    #[test]
    fn match_elements_resource_id_exact() {
        let xml = concat!(
            "<hierarchy>",
            "<node text=\"A\" resource-id=\"com.x:id/btn\" content-desc=\"\" class=\"X\" bounds=\"[0,0][10,10]\" clickable=\"true\"/>",
            "<node text=\"B\" resource-id=\"com.x:id/btn2\" content-desc=\"\" class=\"X\" bounds=\"[10,10][20,20]\" clickable=\"true\"/>",
            "</hierarchy>"
        );
        let hits = adb_match_elements(xml, "resource-id", "com.x:id/btn");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["text"].as_str(), Some("A"));
    }

    #[test]
    fn parse_current_focus_handles_multi_user_shape() {
        let dump = "  mCurrentFocus=Window{abcd u0 com.example.app/com.example.app.MainActivity}";
        assert_eq!(
            parse_current_focus_activity(dump).as_deref(),
            Some("com.example.app/com.example.app.MainActivity")
        );
    }

    #[test]
    fn parse_current_focus_handles_single_user_shape() {
        // Some images omit `u0 `; pick the trailing `/`-bearing token.
        let dump = "  mCurrentFocus=Window{abcd com.example.app/com.example.app.MainActivity}";
        assert_eq!(
            parse_current_focus_activity(dump).as_deref(),
            Some("com.example.app/com.example.app.MainActivity")
        );
    }

    #[test]
    fn parse_current_focus_returns_none_when_null() {
        let dump = "  mCurrentFocus=null";
        assert_eq!(parse_current_focus_activity(dump), None);
    }

    #[test]
    fn parse_resumed_activity_typical_shape() {
        let dump = concat!(
            "ACTIVITY MANAGER ACTIVITIES (dumpsys activity activities)\n",
            "  mResumedActivity: ActivityRecord{1234 u0 com.example.foo/.MainActivity t42}\n",
        );
        assert_eq!(
            parse_resumed_activity(dump).as_deref(),
            Some("com.example.foo/.MainActivity")
        );
    }

    #[test]
    fn adb_input_refused_includes_newlines() {
        // Regression guard: the refusal list MUST include \n/\r/\0 so a
        // malicious text payload can't smuggle a second command past
        // `adb shell input text`.
        for c in ['\n', '\r', '\0', ';', '&', '|', '`', '$'] {
            assert!(
                ADB_INPUT_REFUSED_CHARS.contains(&c),
                "expected '{}' (\\u{{{:x}}}) to be refused for adb input text",
                c.escape_debug(),
                c as u32
            );
        }
    }
}
