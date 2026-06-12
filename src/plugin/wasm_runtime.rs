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
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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

use crate::browser::BrowserSession;

// ---------------------------------------------------------------------------
// WIT bindgen — generates host trait and typed export accessors
// ---------------------------------------------------------------------------

bindgen!({
    path: "src/plugin/wit/world.wit",
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
    browser_cdn_rules: Vec<crate::plugin::manifest::CdnDownloadRule>,
    /// Minimum gap between successive `call_tool` invocations on this plugin
    /// (host-enforced rate limit). 0 disables throttling.
    min_call_interval: Duration,
    /// Last `call_tool` start time, used to compute the throttle delay.
    last_call: Mutex<Option<Instant>>,
    /// Optional provider registry for host-vlm interface.
    providers: Option<Arc<crate::provider::registry::ProviderRegistry>>,
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
    pub tx: tokio::sync::broadcast::Sender<crate::channel::OutboundMessage>,
    pub target_id: String,
    pub channel: String,
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
    cdn_rules: Vec<crate::plugin::manifest::CdnDownloadRule>,
    /// Plugin name — used to scope per-plugin resources (SQLite DB path, etc.).
    plugin_name: String,
    /// Desktop session for host-desktop interface (input synthesis,
    /// screenshots).
    desktop: Box<dyn crate::desktop::DesktopSession>,
    /// Optional provider registry for host-vlm interface.
    providers: Option<Arc<crate::provider::registry::ProviderRegistry>>,
    /// Default vision model name for host-vlm interface.
    vision_model: Option<String>,
    /// ADB device serial (`RSCLAW_ANDROID_SERIAL` env var). Passed `-s
    /// <serial>` to every adb invocation; `None` uses the single attached
    /// device (adb default).
    android_serial: Option<String>,
}

fn new_host_state(
    browser: Arc<Mutex<Option<BrowserSession>>>,
    notify_ctx: Option<WasmNotifyCtx>,
    cdn_rules: Vec<crate::plugin::manifest::CdnDownloadRule>,
    plugin_name: String,
    providers: Option<Arc<crate::provider::registry::ProviderRegistry>>,
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
        desktop: crate::desktop::create_session(),
        providers,
        vision_model,
        android_serial: std::env::var("RSCLAW_ANDROID_SERIAL").ok(),
    }
}

/// Build a sandboxed `Store` for one plugin invocation: memory cap + epoch
/// deadline so a buggy plugin can't OOM or hang the gateway.
fn new_sandboxed_store(
    engine: &Engine,
    browser: Arc<Mutex<Option<BrowserSession>>>,
    notify_ctx: Option<WasmNotifyCtx>,
    cdn_rules: Vec<crate::plugin::manifest::CdnDownloadRule>,
    plugin_name: String,
    providers: Option<Arc<crate::provider::registry::ProviderRegistry>>,
    vision_model: Option<String>,
) -> Store<HostState> {
    let mut store = Store::new(
        engine,
        new_host_state(
            browser,
            notify_ctx,
            cdn_rules,
            plugin_name,
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
    let workspace = crate::config::loader::base_dir().join("workspace");
    let canonical = crate::agent::runtime::canonicalize_external_path(input, &workspace);
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
/// `~/.rsclaw/var/plugins/` so plugins can persist databases and config.
fn canonicalize_writable_path(input: &str) -> Result<PathBuf, String> {
    let base = crate::config::loader::base_dir();
    let workspace = base.join("workspace");
    let plugins_var = base.join("var").join("plugins");
    let canonical = crate::agent::runtime::canonicalize_external_path(input, &workspace);
    if canonical.starts_with(&workspace) || canonical.starts_with(&plugins_var) {
        return Ok(canonical);
    }
    Err(format!(
        "writable path '{}' resolves outside allowed dirs (workspace or var/plugins)",
        input
    ))
}

/// Canonicalize a saved plugin artifact path for read-only document
/// extraction. In addition to workspace/plugin-var paths, this permits
/// `~/Downloads/rsclaw`, which is where `allocate-artifact` stores files.
fn canonicalize_plugin_artifact_path(input: &str) -> Result<PathBuf, String> {
    let base = crate::config::loader::base_dir();
    let workspace = base.join("workspace");
    let plugins_var = base.join("var").join("plugins");
    let downloads_rsclaw = dirs_next::download_dir()
        .unwrap_or_else(|| {
            dirs_next::home_dir()
                .unwrap_or_else(crate::config::loader::base_dir)
                .join("Downloads")
        })
        .join("rsclaw");
    let canonical = crate::agent::runtime::canonicalize_external_path(input, &workspace);
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
    match crate::agent::runtime::extract_file_text(&filename, &bytes).await {
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
    let kb = crate::kb::global_service()
        .ok_or_else(|| "knowledge base is not available in this gateway".to_string())?;

    tokio::task::spawn_blocking(move || -> Result<String, String> {
        let find = || -> Result<Option<crate::kb::model::KbCollection>, String> {
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
                Err(crate::kb::KnowledgeError::DuplicateName) => find()?
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
        // Uploads send a file *out* of the host to a remote site. Unlike
        // read_file (which enforces workspace containment to prevent reading
        // /etc/passwd etc.), upload paths are typically user-supplied via
        // the LLM ("upload ~/Downloads/cat.png") so we tolerate any path
        // the user has access to. Just expand `~` and normalize.
        let workspace = crate::config::loader::base_dir().join("workspace");
        let canonical = crate::agent::runtime::canonicalize_external_path(&filepath, &workspace);
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
            let _ = ctx.tx.send(crate::channel::OutboundMessage {
                target_id: ctx.target_id.clone(),
                is_group: false,
                text: message,
                reply_to: None,
                images: vec![],
                files: vec![],
                channel: Some(ctx.channel.clone()),
                account: None,
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
            match ctx.tx.send(crate::channel::OutboundMessage {
                target_id: ctx.target_id.clone(),
                is_group: false,
                text: message,
                reply_to: None,
                images: vec![image_data_uri],
                files: vec![],
                channel: Some(ctx.channel.clone()),
                account: None,
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
            let canonical = match canonicalize_plugin_path(&file_path) {
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
            match ctx.tx.send(crate::channel::OutboundMessage {
                target_id: ctx.target_id.clone(),
                is_group: false,
                text: message,
                reply_to: None,
                images: vec![],
                files: vec![(filename, mime, path_str)],
                channel: Some(ctx.channel.clone()),
                account: None,
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

/// Return the SQLite database path for a given plugin name.
fn plugin_db_path(plugin_name: &str) -> PathBuf {
    crate::config::loader::base_dir()
        .join("var")
        .join("plugins")
        .join(plugin_name)
        .join("plugin.db")
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
        let ffmpeg_bin = match crate::agent::platform::detect_ffmpeg() {
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
        match crate::channel::transcription::transcribe_audio(&client, &bytes, &audio_path, mime)
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
        let ffmpeg_bin = match crate::agent::platform::detect_ffmpeg() {
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
    let kind = crate::channel::kind_from_extension(&ext);
    let category = crate::channel::category_for_kind(kind);
    let dir = dirs_next::download_dir()
        .unwrap_or_else(|| {
            dirs_next::home_dir()
                .unwrap_or_else(crate::config::loader::base_dir)
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
        // Opening tag ends at the first `>` (attribute values use &gt; for literal `>`).
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
    /// Execute a browser action by locking the shared browser session.
    /// Auto-starts Chrome if no session exists.
    async fn browser_action(&mut self, action: &str, args: Value) -> Result<String, String> {
        let mut guard = self.browser.lock().await;

        // Auto-start browser if not initialized.
        if guard.is_none() {
            tracing::info!("WASM plugin: auto-starting browser session");
            let chrome_path = crate::agent::platform::ensure_chrome()
                .await
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

    async fn desktop_screenshot_region(
        &mut self,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> wasmtime::Result<Result<String, String>> {
        Ok(self.desktop.screenshot_region(x, y, w, h).await)
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

        let messages = vec![crate::provider::Message {
            role: crate::provider::Role::User,
            content: crate::provider::MessageContent::Parts(vec![
                crate::provider::ContentPart::Text { text: prompt },
                crate::provider::ContentPart::Image {
                    url: image_data_uri,
                },
            ]),
            rsclaw_hidden: None,
        }];

        let req = crate::provider::LlmRequest {
            fallback_models: Vec::new(),
            model: format!("{provider_name}/{model_id}"),
            messages,
            tools: Vec::new(),
            system: None,
            max_tokens: Some(max_tokens),
            temperature: Some(0.0),
            frequency_penalty: None,
            thinking_budget: None,
            endpoint: crate::provider::AgentEndpoint::Vision,
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
                        Ok(crate::provider::StreamEvent::TextDelta(d)) => text.push_str(&d),
                        Ok(crate::provider::StreamEvent::ReasoningDelta(d)) => {
                            reasoning.push_str(&d)
                        }
                        Ok(crate::provider::StreamEvent::Done { .. }) => break,
                        Ok(crate::provider::StreamEvent::ToolCall { .. }) => {}
                        Ok(crate::provider::StreamEvent::Error(e)) => {
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
        let (xs, ys) = (x.to_string(), y.to_string());
        Ok(adb_run_str(serial.as_deref(), &["shell", "input", "tap", &xs, &ys])
            .await
            .map(|_| "tapped".to_string()))
    }

    async fn android_swipe(
        &mut self,
        x1: u32, y1: u32, x2: u32, y2: u32, duration_ms: u32,
    ) -> HostTrapResult<Result<String, String>> {
        let serial = self.android_serial.clone();
        let (s1, s2, s3, s4, s5) = (
            x1.to_string(), y1.to_string(),
            x2.to_string(), y2.to_string(),
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
        // adb shell input text runs through the device shell. See
        // `ADB_INPUT_REFUSED_CHARS` for the rejection list — includes
        // `\n` / `\r` / `\0` so a malicious payload can't smuggle a
        // second command past `input text`.
        if let Some(bad) = text.chars().find(|c| ADB_INPUT_REFUSED_CHARS.contains(c)) {
            return Ok(Err(format!(
                "android_type: refusing text with shell metachar '{}' (strip and retry)",
                bad.escape_debug()
            )));
        }
        let serial = self.android_serial.clone();
        let escaped = text.replace(' ', "%s");
        Ok(adb_run_str(serial.as_deref(), &["shell", "input", "text", &escaped])
            .await
            .map(|_| "typed".to_string()))
    }

    async fn android_press(&mut self, key: String) -> HostTrapResult<Result<String, String>> {
        let kc = match key.to_lowercase().as_str() {
            "back" => "KEYCODE_BACK",
            "home" => "KEYCODE_HOME",
            "menu" => "KEYCODE_MENU",
            "enter" | "return" => "KEYCODE_ENTER",
            "tab" => "KEYCODE_TAB",
            "delete" | "del" => "KEYCODE_DEL",
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
        Ok(adb_run_str(serial.as_deref(), &["shell", "input", "keyevent", kc])
            .await
            .map(|_| format!("pressed {key}")))
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
        if let Ok(out) =
            adb_run_str(serial.as_deref(), &["shell", "dumpsys", "window", "windows"]).await
            && let Some(activity) = parse_current_focus_activity(&out)
        {
            return Ok(Ok(activity));
        }
        match adb_run_str(serial.as_deref(), &["shell", "dumpsys", "activity", "activities"]).await
        {
            Ok(out) => match parse_resumed_activity(&out) {
                Some(activity) => Ok(Ok(activity)),
                None => Ok(Err(
                    "could not determine current activity (neither mCurrentFocus nor \
                     mResumedActivity matched in dumpsys output)"
                        .to_string(),
                )),
            },
            Err(e) => Ok(Err(format!(
                "dumpsys activity activities failed: {e}"
            ))),
        }
    }

    async fn android_launch_app(&mut self, pkg: String) -> HostTrapResult<Result<String, String>> {
        let serial = self.android_serial.clone();
        Ok(adb_run_str(
            serial.as_deref(),
            &[
                "shell", "monkey", "-p", &pkg,
                "-c", "android.intent.category.LAUNCHER", "1",
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
        Ok(Ok(serde_json::to_string(&elements).unwrap_or_else(|_| "[]".to_string())))
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
        let (xs, ys) = (cx.to_string(), cy.to_string());
        Ok(adb_run_str(serial.as_deref(), &["shell", "input", "tap", &xs, &ys])
            .await
            .map(|_| "tapped".to_string()))
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
        if let Some(bad) = text.chars().find(|c| ADB_INPUT_REFUSED_CHARS.contains(c)) {
            return Ok(Err(format!(
                "android_set_element_text: refusing text with shell metachar '{}'",
                bad.escape_debug()
            )));
        }
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

        let escaped = text.replace(' ', "%s");
        Ok(adb_run_str(serial.as_deref(), &["shell", "input", "text", &escaped])
            .await
            .map(|_| "set".to_string()))
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
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(u64::from(timeout_ms));
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
    providers: Option<Arc<crate::provider::registry::ProviderRegistry>>,
    vision_model: Option<String>,
) -> Result<WasmPlugin> {
    let path = manifest.entry_path();
    let wasm_bytes = std::fs::read(&path)
        .with_context(|| format!("failed to read WASM file: {}", path.display()))?;

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
        min_call_interval: Duration::from_millis(u64::from(manifest.min_call_interval_ms)),
        last_call: Mutex::new(None),
        providers,
        vision_model,
    })
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
    fn xml_unescape_handles_common_entities() {
        assert_eq!(adb_xml_unescape("plain"), "plain");
        assert_eq!(adb_xml_unescape("Don&apos;t"), "Don't");
        assert_eq!(adb_xml_unescape("a&amp;b"), "a&b");
        assert_eq!(adb_xml_unescape("quote &quot; lt &lt; gt &gt;"), "quote \" lt < gt >");
        assert_eq!(adb_xml_unescape("line1&#10;line2"), "line1\nline2");
        assert_eq!(adb_xml_unescape("line1&#xA;line2"), "line1\nline2");
    }

    #[test]
    fn xml_attr_extracts_quoted_value() {
        let node = r#"<node text="hello world" resource-id="com.x:id/foo" bounds="[0,0][100,200]">"#;
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
