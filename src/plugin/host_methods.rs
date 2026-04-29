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

    // ---- A2 stubs (filled in Tasks 11–15) ----

    async fn host_browser_open(&self, _params: Value) -> Result<Value> {
        unimplemented!("filled in Task 11")
    }
    async fn host_browser_eval(&self, _params: Value) -> Result<Value> {
        unimplemented!("filled in Task 11")
    }
    async fn host_browser_eval_with_args(&self, _params: Value) -> Result<Value> {
        unimplemented!("filled in Task 11")
    }
    async fn host_browser_click(&self, _params: Value) -> Result<Value> {
        unimplemented!("filled in Task 12")
    }
    async fn host_browser_click_at(&self, _params: Value) -> Result<Value> {
        unimplemented!("filled in Task 12")
    }
    async fn host_browser_fill(&self, _params: Value) -> Result<Value> {
        unimplemented!("filled in Task 13")
    }
    async fn host_browser_snapshot(&self, _params: Value) -> Result<Value> {
        unimplemented!("filled in Task 13")
    }
    async fn host_browser_download(&self, _params: Value) -> Result<Value> {
        unimplemented!("filled in Task 14")
    }
    async fn host_sleep(&self, _params: Value) -> Result<Value> {
        unimplemented!("filled in Task 15")
    }
    async fn host_storage_allocate_artifact(&self, _params: Value) -> Result<Value> {
        unimplemented!("filled in Task 15")
    }
}
