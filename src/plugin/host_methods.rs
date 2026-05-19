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
    pub browser: Arc<Mutex<Option<BrowserSession>>>,
}

impl HostMethodRegistry {
    /// Create a new registry with the given notification sender and browser session.
    pub fn new(
        notify_tx: Option<broadcast::Sender<OutboundMessage>>,
        browser: Arc<Mutex<Option<BrowserSession>>>,
    ) -> Self {
        Self { notify_tx, browser }
    }

    /// Dispatch one plugin-initiated request.
    pub async fn handle(&self, method: &str, params: Value) -> Result<Value> {
        debug!(method, "host method dispatch");
        match method {
            "notify" => self.host_notify(params).await,
            "notify_with_image" => self.host_notify_with_image(params).await,
            "log" => self.host_log(params).await,
            "browser_open" => self.host_browser_open(params).await,
            "browser_eval" => self.host_browser_eval(params).await,
            "browser_eval_with_args" => self.host_browser_eval_with_args(params).await,
            "browser_click" => self.host_browser_click(params).await,
            "browser_click_at" => self.host_browser_click_at(params).await,
            "browser_fill" => self.host_browser_fill(params).await,
            "browser_snapshot" => self.host_browser_snapshot(params).await,
            "browser_screenshot" => self.host_browser_screenshot(params).await,
            "browser_download" => self.host_browser_download(params).await,
            "sleep" => self.host_sleep(params).await,
            "storage_allocate_artifact" => self.host_storage_allocate_artifact(params).await,
            "extract_audio" => self.host_extract_audio(params).await,
            "transcribe" => self.host_transcribe(params).await,
            "extract_keyframes" => self.host_extract_keyframes(params).await,
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
        let ctx = params
            .get("_ctx")
            .ok_or_else(|| anyhow::anyhow!("notify: `_ctx` required"))?;
        let target_id = ctx["target_id"].as_str().unwrap_or("").to_owned();
        let channel = ctx["channel"].as_str().unwrap_or("").to_owned();

        tracing::info!(target: "shell_plugin_notify", "{text}");

        let Some(tx) = &self.notify_tx else {
            warn!(
                "notify called but notify_tx is not configured (plugin not in agent ctx); logged only"
            );
            return Ok(json!({ "status": "logged_only" }));
        };

        let msg = OutboundMessage {
            target_id,
            text,
            channel: if channel.is_empty() {
                None
            } else {
                Some(channel)
            },
            ..Default::default()
        };
        match tx.send(msg) {
            Ok(_) => Ok(json!({ "status": "dispatched" })),
            Err(_) => Ok(json!({ "status": "no_receivers" })),
        }
    }

    /// Notify the user with an inline image (e.g. login QR, captcha screenshot).
    ///
    /// Mirrors `wasm_runtime.rs::notify_with_image`. Requires `text`,
    /// `image_data_uri` (a `data:image/...;base64,...` URI — what the
    /// `browser_screenshot` host method returns in its `image` field), and
    /// `_ctx` (with `target_id` + `channel`).
    async fn host_notify_with_image(&self, params: Value) -> Result<Value> {
        let text = params["text"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("notify_with_image: `text` required"))?
            .to_owned();
        let image = params["image_data_uri"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("notify_with_image: `image_data_uri` required"))?
            .to_owned();
        let ctx = params
            .get("_ctx")
            .ok_or_else(|| anyhow::anyhow!("notify_with_image: `_ctx` required"))?;
        let target_id = ctx["target_id"].as_str().unwrap_or("").to_owned();
        let channel = ctx["channel"].as_str().unwrap_or("").to_owned();

        tracing::info!(target: "shell_plugin_notify", "{text}");

        let Some(tx) = &self.notify_tx else {
            warn!(
                "notify_with_image called but notify_tx is not configured (plugin not in agent ctx); logged only"
            );
            return Ok(json!({ "status": "logged_only" }));
        };

        let msg = OutboundMessage {
            target_id,
            text,
            channel: if channel.is_empty() {
                None
            } else {
                Some(channel)
            },
            images: vec![image],
            ..Default::default()
        };
        match tx.send(msg) {
            Ok(_) => Ok(json!({ "status": "dispatched" })),
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
        let text = params["text"].as_str().unwrap_or("");
        match level {
            "error" => tracing::error!(target: "shell_plugin", plugin_log = true, "{text}"),
            "warn" => tracing::warn!(target: "shell_plugin",  plugin_log = true, "{text}"),
            "debug" => tracing::debug!(target: "shell_plugin", plugin_log = true, "{text}"),
            _ => tracing::info!(target: "shell_plugin",  plugin_log = true, "{text}"),
        }
        Ok(Value::Null)
    }

    // ---- A2 browser helper ----

    /// Lock the shared browser session, auto-starting Chrome on first use,
    /// and dispatch the action. Returns the raw JSON the browser produced.
    ///
    /// The profile name MUST match `SHARED_BROWSER_PROFILE` in
    /// `wasm_runtime.rs` so login state persists across runtimes.
    async fn browser_call_raw(&self, action: &str, args: Value) -> Result<Value> {
        const PROFILE: &str = "rsclaw"; // MUST match wasm_runtime.rs::SHARED_BROWSER_PROFILE

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
        session
            .execute(action, &args)
            .await
            .map_err(|e| anyhow::anyhow!("{e:#}"))
    }

    /// Like `browser_call_raw`, but extracts a single payload field the way
    /// wasm plugins see results.
    ///
    /// Mirrors `wasm_runtime.rs::HostState::browser_action`. The two runtimes
    /// MUST share this code path so a shell plugin and a wasm plugin see
    /// byte-identical browser results — except `screenshot`, which uses
    /// `browser_call_raw` directly to keep the auto-saved `image_path`.
    async fn browser_call(&self, action: &str, args: Value) -> Result<Value> {
        let val = self.browser_call_raw(action, args).await?;
        for field in &["text", "image", "data", "url", "result"] {
            if let Some(s) = val.get(field).and_then(|v| v.as_str()) {
                return Ok(Value::String(s.to_string()));
            }
        }
        Ok(val)
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
        self.browser_call("click", json!({"ref": element_ref}))
            .await
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
        self.browser_call("fill", json!({"ref": element_ref, "text": text}))
            .await
    }

    /// Capture an accessibility snapshot of the current page in the shared browser session.
    ///
    /// Params: `{}` (none required). Mirrors wasm `browser_snapshot`.
    async fn host_browser_snapshot(&self, _params: Value) -> Result<Value> {
        self.browser_call("snapshot", json!({})).await
    }

    /// Capture a viewport screenshot of the current page in the shared browser session.
    ///
    /// Returns the full JSON response `{image, image_path, mime}` (instead of
    /// the single-field extraction `browser_call` does), so shell plugins can
    /// notify the user with the on-disk path the host auto-saved to without
    /// re-decoding the base64 data URI. wasm plugins go through `browser_call`
    /// and only see `image`; the two surfaces deliberately differ here.
    async fn host_browser_screenshot(&self, _params: Value) -> Result<Value> {
        self.browser_call_raw("screenshot", json!({})).await
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

    // ---- Media methods ----

    /// Extract audio from a video/audio file using ffmpeg.
    /// Params: `{ "input_path": "<path>" }`. Mirrors wasm `extract_audio`.
    async fn host_extract_audio(&self, params: Value) -> Result<Value> {
        let input_path = params["input_path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("extract_audio: `input_path` required"))?;

        let ffmpeg_bin = match crate::agent::platform::detect_ffmpeg() {
            Some(p) => p,
            None => return Ok(json!({"error": "ffmpeg not found. Run: rsclaw tools install ffmpeg"})),
        };

        let out_path = match crate::plugin::wasm_runtime::allocate_dl_paths("audio.wav", 1) {
            Ok(mut p) => p.pop().unwrap_or_default(),
            Err(e) => return Ok(json!({"error": e})),
        };

        let output = tokio::process::Command::new(&ffmpeg_bin)
            .args([
                "-y", "-i", input_path,
                "-vn", "-acodec", "pcm_s16le",
                "-ar", "16000", "-ac", "1",
                &out_path,
            ])
            .output()
            .await;

        match output {
            Ok(o) if o.status.success() => Ok(json!({"path": out_path})),
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                Ok(json!({"error": format!("ffmpeg failed: {stderr}")}))
            }
            Err(e) => Ok(json!({"error": format!("ffmpeg spawn error: {e}")})),
        }
    }

    /// Transcribe audio to text using the host's STT engine.
    /// Params: `{ "audio_path": "<path>", "language": "zh-CN" }`. Mirrors wasm `transcribe`.
    async fn host_transcribe(&self, params: Value) -> Result<Value> {
        let audio_path = params["audio_path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("transcribe: `audio_path` required"))?;
        let _language = params["language"].as_str().unwrap_or("zh-CN");

        let bytes = match tokio::fs::read(audio_path).await {
            Ok(b) => b,
            Err(e) => return Ok(json!({"error": format!("read audio file failed: {e}")})),
        };

        let mime = if audio_path.to_lowercase().ends_with(".wav") {
            "audio/wav"
        } else {
            "audio/mpeg"
        };

        let client = reqwest::Client::new();
        match crate::channel::transcription::transcribe_audio(&client, &bytes, audio_path, mime).await {
            Ok(text) => Ok(json!({"text": text})),
            Err(e) => Ok(json!({"error": format!("transcription failed: {e:#}")})),
        }
    }

    /// Extract keyframes from a video file using ffmpeg.
    /// Params: `{ "video_path": "<path>", "count": 5 }`. Mirrors wasm `extract_keyframes`.
    async fn host_extract_keyframes(&self, params: Value) -> Result<Value> {
        let video_path = params["video_path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("extract_keyframes: `video_path` required"))?;
        let count = params["count"].as_u64().unwrap_or(5).max(1).min(20) as usize;

        let ffmpeg_bin = match crate::agent::platform::detect_ffmpeg() {
            Some(p) => p,
            None => return Ok(json!({"error": "ffmpeg not found. Run: rsclaw tools install ffmpeg"})),
        };

        let out_paths = match crate::plugin::wasm_runtime::allocate_dl_paths("frame.png", count) {
            Ok(p) => p,
            Err(e) => return Ok(json!({"error": e})),
        };

        // Get video duration.
        let duration_secs: f64 = {
            let probe = tokio::process::Command::new(&ffmpeg_bin)
                .args(["-v", "error", "-show_entries", "format=duration",
                       "-of", "default=noprint_wrappers=1:nokey=1", video_path])
                .output()
                .await;
            match probe {
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout).trim().parse().unwrap_or(0.0)
                }
                _ => 0.0,
            }
        };

        if duration_secs <= 0.0 {
            return Ok(json!({"error": "could not determine video duration"}));
        }

        let interval = duration_secs / count as f64;
        let out_pattern = out_paths[0].replace(".png", "_%03d.png");

        let output = tokio::process::Command::new(&ffmpeg_bin)
            .args([
                "-y", "-i", video_path,
                "-vf", &format!("fps=1/{interval},scale=480:-1"),
                &out_pattern,
            ])
            .output()
            .await;

        match output {
            Ok(o) if o.status.success() => Ok(json!({"paths": out_paths})),
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                Ok(json!({"error": format!("ffmpeg failed: {stderr}")}))
            }
            Err(e) => Ok(json!({"error": format!("ffmpeg spawn error: {e}")})),
        }
    }
}
