//! Host method registry — the dispatcher for plugin-initiated requests.
//!
//! When a shell-bridge plugin writes a JSON-RPC request with a negative id
//! to its stdout, the reader task in `shell_bridge.rs` calls
//! `HostMethodRegistry::handle(method, params)`. Each method below mirrors a
//! host function exposed to WASM plugins via the host-runtime / host-browser /
//! host-storage WIT interfaces, so a Node plugin and a wasm plugin see the
//! same capability surface.

use std::sync::Arc;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};
use tracing::{debug, warn};

use crate::browser::BrowserSession;
use crate::channel::OutboundMessage;

/// All dependencies a host method might need. Cloned cheaply (everything is
/// behind Arc) and shared across plugin spawns.
#[derive(Clone)]
pub struct HostMethodRegistry {
    pub notify_tx: Option<broadcast::Sender<OutboundMessage>>,
    pub browser:   Arc<Mutex<Option<BrowserSession>>>,
}

impl HostMethodRegistry {
    /// Create a new registry with the given notification sender and browser session.
    pub fn new(
        notify_tx: Option<broadcast::Sender<OutboundMessage>>,
        browser:   Arc<Mutex<Option<BrowserSession>>>,
    ) -> Self {
        Self { notify_tx, browser }
    }

    /// Dispatch one plugin-initiated request.
    pub async fn handle(&self, method: &str, params: Value) -> Result<Value> {
        debug!(method, "host method dispatch");
        match method {
            "notify"                    => self.host_notify(params).await,
            "log"                       => self.host_log(params).await,
            "browser_open"              => self.host_browser_open(params).await,
            "browser_eval"              => self.host_browser_eval(params).await,
            "browser_eval_with_args"    => self.host_browser_eval_with_args(params).await,
            "browser_click"             => self.host_browser_click(params).await,
            "browser_click_at"          => self.host_browser_click_at(params).await,
            "browser_fill"              => self.host_browser_fill(params).await,
            "browser_snapshot"          => self.host_browser_snapshot(params).await,
            "browser_download"          => self.host_browser_download(params).await,
            "sleep"                     => self.host_sleep(params).await,
            "storage_allocate_artifact" => self.host_storage_allocate_artifact(params).await,
            other => bail!("unknown host method: {other}"),
        }
    }

    // ---- A1 methods ----

    /// Send a notification to the user's IM channel.
    ///
    /// Mirrors the wasm `notify` host function. Requires `text` and `_ctx`
    /// (with `target_id` and `channel`) in `params`.
    async fn host_notify(&self, params: Value) -> Result<Value> {
        let text = params["text"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("notify: `text` required"))?
            .to_owned();
        let ctx = params.get("_ctx")
            .ok_or_else(|| anyhow::anyhow!("notify: `_ctx` required"))?;
        let target_id = ctx["target_id"].as_str().unwrap_or("").to_owned();
        let channel   = ctx["channel"].as_str().unwrap_or("").to_owned();

        tracing::info!(target: "shell_plugin_notify", "{text}");

        let Some(tx) = &self.notify_tx else {
            warn!("notify called but notify_tx is not configured (plugin not in agent ctx); logged only");
            return Ok(json!({ "status": "logged_only" }));
        };

        let msg = OutboundMessage {
            target_id,
            text,
            channel: if channel.is_empty() { None } else { Some(channel) },
            ..Default::default()
        };
        match tx.send(msg) {
            Ok(_)  => Ok(json!({ "status": "dispatched" })),
            Err(_) => Ok(json!({ "status": "no_receivers" })),
        }
    }

    /// Forward a plugin log line into the gateway's tracing spans.
    ///
    /// Accepts `level` (`error` | `warn` | `debug` | `info`) and `text`.
    /// The `plugin_log = true` field lets log filters distinguish plugin
    /// output from gateway logs — mirrors the wasm side's pattern.
    async fn host_log(&self, params: Value) -> Result<Value> {
        let level = params["level"].as_str().unwrap_or("info");
        let text  = params["text"].as_str().unwrap_or("");
        match level {
            "error" => tracing::error!(target: "shell_plugin", plugin_log = true, "{text}"),
            "warn"  => tracing::warn!(target: "shell_plugin",  plugin_log = true, "{text}"),
            "debug" => tracing::debug!(target: "shell_plugin", plugin_log = true, "{text}"),
            _       => tracing::info!(target: "shell_plugin",  plugin_log = true, "{text}"),
        }
        Ok(Value::Null)
    }

    // ---- A2 browser helper ----

    /// Lock the shared browser session, auto-starting Chrome on first use,
    /// dispatch the action via `BrowserSession::execute`, and extract a
    /// payload string the way wasm plugins see results.
    ///
    /// Mirrors `wasm_runtime.rs::HostState::browser_action`. The two runtimes
    /// MUST share this code path so a shell plugin and a wasm plugin see
    /// byte-identical browser results.
    async fn browser_call(&self, action: &str, args: Value) -> Result<Value> {
        // The profile name MUST match `SHARED_BROWSER_PROFILE` in
        // `wasm_runtime.rs` (currently "jimeng"). Both runtimes share the
        // same Chrome profile so login state persists across runtimes.
        // Making that constant `pub` would couple wasm_runtime to this
        // module; a future cleanup task can do that if the value ever changes.
        const PROFILE: &str = "jimeng"; // MUST match wasm_runtime.rs::SHARED_BROWSER_PROFILE

        let mut guard = self.browser.lock().await;

        if guard.is_none() {
            tracing::info!("shell plugin: auto-starting browser session");
            let chrome_path = crate::agent::platform::ensure_chrome()
                .await
                .map_err(|e| anyhow::anyhow!("failed to obtain Chrome: {e:#}"))?;
            let session = BrowserSession::start(&chrome_path, true, Some(PROFILE))
                .await
                .map_err(|e| anyhow::anyhow!("failed to start Chrome: {e:#}"))?;
            *guard = Some(session);
        }

        let session = guard.as_mut().expect("browser session just initialized");
        match session.execute(action, &args).await {
            Ok(val) => {
                for field in &["text", "image", "data", "url", "result"] {
                    if let Some(s) = val.get(field).and_then(|v| v.as_str()) {
                        return Ok(Value::String(s.to_string()));
                    }
                }
                Ok(val)
            }
            Err(e) => Err(anyhow::anyhow!("{e:#}")),
        }
    }

    // ---- A2 stubs (filled in Tasks 11–15) ----

    /// Open a URL in the shared browser session.
    ///
    /// Params: `{ "url": "<url>" }`. Mirrors wasm `browser_open`.
    async fn host_browser_open(&self, params: Value) -> Result<Value> {
        let url = params["url"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("browser_open: `url` required"))?;
        self.browser_call("open", json!({"url": url})).await
    }

    /// Evaluate a JavaScript snippet in the shared browser session.
    ///
    /// Params: `{ "script": "<js>" }`. Mirrors wasm `browser_eval`.
    async fn host_browser_eval(&self, params: Value) -> Result<Value> {
        let code = params["script"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("browser_eval: `script` required"))?;
        self.browser_call("evaluate", json!({"js": code})).await
    }

    /// Evaluate a JavaScript function with arguments in the shared browser session.
    ///
    /// Params: `{ "fn": "<async fn source>", "args": <any JSON value> }`.
    /// The function is wrapped in an IIFE matching the wasm `eval_with_args`
    /// wrapper exactly so results are byte-identical between runtimes.
    async fn host_browser_eval_with_args(&self, params: Value) -> Result<Value> {
        let code = params["fn"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("browser_eval_with_args: `fn` required"))?;
        let args = params.get("args").cloned().unwrap_or(Value::Null);
        let args_literal = serde_json::to_string(&args).unwrap_or_else(|_| "null".to_string());
        let wrapped = format!(
            r#"(async function() {{
            const __args = ({args_literal});
            const __fn = ({code});
            const __out = await __fn(__args);
            return typeof __out === "string" ? __out : JSON.stringify(__out);
        }})()"#
        );
        self.browser_call("evaluate", json!({"js": wrapped})).await
    }
    /// Click on a DOM element by accessibility ref in the shared browser session.
    ///
    /// Params: `{ "ref": "<element ref>" }`. Mirrors wasm `browser_click`.
    async fn host_browser_click(&self, params: Value) -> Result<Value> {
        let element_ref = params["ref"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("browser_click: `ref` required"))?;
        self.browser_call("click", json!({"ref": element_ref})).await
    }

    /// Click at a specific viewport coordinate in the shared browser session.
    ///
    /// Params: `{ "x": <u64>, "y": <u64> }`. Mirrors wasm `browser_click_at`.
    async fn host_browser_click_at(&self, params: Value) -> Result<Value> {
        let x = params["x"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("browser_click_at: `x` required"))?;
        let y = params["y"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("browser_click_at: `y` required"))?;
        self.browser_call("click_at", json!({"x": x, "y": y})).await
    }
    /// Fill a form field by accessibility ref in the shared browser session.
    ///
    /// Params: `{ "ref": "<element ref>", "text": "<value>" }`. Mirrors wasm `browser_fill`.
    async fn host_browser_fill(&self, params: Value) -> Result<Value> {
        let element_ref = params["ref"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("browser_fill: `ref` required"))?;
        let text = params["text"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("browser_fill: `text` required"))?;
        self.browser_call("fill", json!({"ref": element_ref, "text": text})).await
    }

    /// Capture an accessibility snapshot of the current page in the shared browser session.
    ///
    /// Params: `{}` (none required). Mirrors wasm `browser_snapshot`.
    async fn host_browser_snapshot(&self, _params: Value) -> Result<Value> {
        self.browser_call("snapshot", json!({})).await
    }
    /// Download a resource (URL or element ref) to a local path in the shared browser session.
    ///
    /// Params: `{ "url": "<url or element ref>", "dest_path": "<local path>" }`.
    /// Optional `"referer"` may be supplied for sites that require it; on the wasm side
    /// referer attachment is automatic via per-plugin CDN rules — Node plugins pass it
    /// explicitly instead.
    /// Mirrors wasm `browser_download`.
    async fn host_browser_download(&self, params: Value) -> Result<Value> {
        let url = params["url"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("browser_download: `url` required"))?;
        let dest = params["dest_path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("browser_download: `dest_path` required"))?;
        let mut args = json!({"ref": url, "path": dest});
        // Optional: plugin can pre-supply a referer for sites that require it.
        if let Some(referer) = params.get("referer").and_then(|v| v.as_str()) {
            args["referer"] = json!(referer);
        }
        self.browser_call("download", args).await
    }
    async fn host_sleep(&self, params: Value) -> Result<Value> {
        let ms = params["ms"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("sleep: `ms` required"))?;
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        Ok(Value::Null)
    }
    async fn host_storage_allocate_artifact(&self, params: Value) -> Result<Value> {
        let filename = params["filename"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("storage_allocate_artifact: `filename` required"))?;
        // Optional: count > 1 → allocate a group of paths sharing one base.
        let count = params
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1) as usize;
        match crate::plugin::wasm_runtime::allocate_dl_paths(filename, count) {
            Ok(paths) => {
                if count == 1 {
                    Ok(serde_json::json!({ "path": paths.into_iter().next().unwrap_or_default() }))
                } else {
                    Ok(serde_json::json!({ "paths": paths }))
                }
            }
            Err(e) => Err(anyhow::anyhow!("{e}")),
        }
    }
}
