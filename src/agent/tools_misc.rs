//! Miscellaneous tool handlers — TTS, messaging, gateway, pairing, doc/pdf,
//! memory, install, channel actions.
//!
//! Image generation lives in `tools_image.rs`; video generation lives in
//! `tools_video.rs`. All compile as split impl blocks against the same
//! `AgentRuntime` struct.

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::platform::powershell_hidden;
use super::runtime::{AgentRuntime, RunContext};

impl AgentRuntime {
    // -----------------------------------------------------------------------
    // TTS (text-to-speech)
    // -----------------------------------------------------------------------

    /// Generate TTS audio from text. Prefers sherpa-onnx, falls back to system TTS.
    /// Returns the path to the generated audio file.
    pub(crate) async fn generate_tts_audio(&self, text: &str) -> Result<String> {
        // Truncate long text for TTS (avoid very long audio).
        let tts_text = if text.chars().count() > 500 {
            let idx = text.char_indices().nth(500).map(|(i, _)| i).unwrap_or(text.len());
            &text[..idx]
        } else {
            text
        };

        let out_path = std::env::temp_dir().join(format!(
            "rsclaw_tts_{}.wav",
            chrono::Utc::now().timestamp_millis()
        ));
        let out_str = out_path.to_string_lossy().to_string();

        // Try sherpa-onnx first. Binary lives under
        // `<base>/tools/sherpa-onnx/bin/` (installed via
        // `rsclaw tools install sherpa-onnx`); models live separately
        // under `<base>/models/vits-*/` (installed via
        // `rsclaw models download vits-theresa`). Earlier code looked
        // for the model under tools/sherpa-onnx/models/tts/, which is
        // where models would never actually land — so the binary check
        // passed but the model check always failed and TTS silently
        // fell through to system `say` / SAPI / espeak.
        let sherpa_bin = crate::config::loader::base_dir()
            .join("tools")
            .join("sherpa-onnx")
            .join("bin")
            .join(if cfg!(target_os = "windows") { "sherpa-onnx-offline-tts.exe" } else { "sherpa-onnx-offline-tts" });

        if sherpa_bin.exists()
            && let Some(vits) = find_vits_model()
        {
            // sherpa-onnx 1.13 enforces `--flag=value` (single argv
            // slot). Separate `--flag value` argv slots get rejected by
            // parse-options.cc with "option format is --x=y".
            let mut cmd = tokio::process::Command::new(&sherpa_bin);
            cmd.arg(format!("--vits-model={}", vits.model.display()));
            cmd.arg(format!("--vits-tokens={}", vits.tokens.display()));
            if let Some(lex) = vits.lexicon.as_ref() {
                cmd.arg(format!("--vits-lexicon={}", lex.display()));
            }
            // `--vits-data-dir` is sherpa-onnx's espeak-ng dict path, NOT
            // the jieba `dict/` shipped with the Chinese theresa bundle.
            // Passing the latter here breaks model load. The jieba dict
            // is consumed implicitly via the lexicon.txt path. Keep this
            // hook for English bundles that DO use espeak-ng.
            if let Some(data) = vits.data_dir.as_ref() {
                let path_str = data.to_string_lossy();
                let looks_like_jieba = path_str.ends_with("/dict")
                    || path_str.ends_with("\\dict");
                if !looks_like_jieba {
                    cmd.arg(format!("--vits-data-dir={path_str}"));
                }
            }
            // Flag is `--tts-rule-fsts`, not `--vits-rule-fsts` (the
            // latter doesn't exist on sherpa-onnx 1.13+ and triggers
            // "Invalid option" parse-options error).
            if let Some(rule) = vits.rule_fsts.as_ref() {
                cmd.arg(format!("--tts-rule-fsts={rule}"));
            }
            cmd.arg(format!("--output-filename={out_str}"));
            cmd.arg("--vits-length-scale=1.0");
            cmd.arg(tts_text);
            #[cfg(target_os = "windows")]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x08000000);
            }
            let output = cmd.output().await;
            if let Ok(o) = output
                && o.status.success()
                && out_path.exists()
            {
                return Ok(out_str);
            }
            // Fall through to system TTS if sherpa-onnx failed.
        }

        // Fallback: system TTS (same as tool_tts).
        #[cfg(target_os = "macos")]
        {
            let output = tokio::process::Command::new("say")
                .args(["-o", &out_str, tts_text])
                .output()
                .await
                .map_err(|e| anyhow!("auto-tts: say failed: {e}"))?;
            if !output.status.success() {
                return Err(anyhow!("auto-tts: say exit code {}", output.status));
            }
        }
        #[cfg(target_os = "windows")]
        {
            let safe_text = tts_text.replace('\'', "''");
            let script = format!(
                "Add-Type -AssemblyName System.Speech; $s = New-Object System.Speech.Synthesis.SpeechSynthesizer; $s.SetOutputToWaveFile('{}'); $s.Speak('{}')",
                out_str.replace('\'', "''"), safe_text
            );
            let output = powershell_hidden()
                .args(["-Command", &script])
                .output()
                .await
                .map_err(|e| anyhow!("auto-tts: SAPI failed: {e}"))?;
            if !output.status.success() {
                return Err(anyhow!("auto-tts: SAPI exit code {}", output.status));
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let result = tokio::process::Command::new("espeak")
                .args(["-w", &out_str, tts_text])
                .output()
                .await;
            match result {
                Ok(o) if o.status.success() => {}
                _ => {
                    tokio::process::Command::new("pico2wave")
                        .args(["-w", &out_str, "--", tts_text])
                        .output()
                        .await
                        .map_err(|e| anyhow!("auto-tts: no TTS engine available: {e}"))?;
                }
            }
        }

        if out_path.exists() {
            Ok(out_str)
        } else {
            Err(anyhow!("auto-tts: output file not created"))
        }
    }

    pub(crate) async fn tool_tts(&self, args: Value) -> Result<Value> {
        let text = args["text"]
            .as_str()
            .ok_or_else(|| anyhow!("tts: `text` required"))?;
        let voice = args["voice"].as_str().unwrap_or("default");

        let ts = chrono::Utc::now().timestamp_millis();
        let tmp_dir = std::env::temp_dir();
        // Final output is always mp3 for IM platform compatibility.
        let out_path = tmp_dir.join(format!("rsclaw_tts_{ts}.mp3"));
        let out_path_str = out_path.to_string_lossy().to_string();

        let is_macos = cfg!(target_os = "macos");
        let is_windows = cfg!(target_os = "windows");

        if is_macos {
            // macOS `say` outputs aiff, then convert to mp3 via ffmpeg.
            let aiff_path = tmp_dir.join(format!("rsclaw_tts_{ts}.aiff"));
            let aiff_str = aiff_path.to_string_lossy().to_string();
            let mut cmd = tokio::process::Command::new("say");
            if voice != "default" {
                cmd.args(["-v", voice]);
            }
            cmd.args(["-o", &aiff_str, text]);
            let output = cmd
                .output()
                .await
                .map_err(|e| anyhow!("tts: `say` command failed: {e}"))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(anyhow!("tts: say failed: {stderr}"));
            }
            // Convert aiff to mp3 via ffmpeg (required for feishu/weixin/etc.)
            let ffmpeg_bin = crate::agent::platform::detect_ffmpeg().unwrap_or_else(|| "ffmpeg".to_owned());
            let ffmpeg = tokio::process::Command::new(&ffmpeg_bin)
                .args(["-i", &aiff_str, "-y", "-q:a", "4", &out_path_str])
                .output()
                .await;
            match ffmpeg {
                Ok(o) if o.status.success() => {
                    let _ = std::fs::remove_file(&aiff_path);
                }
                _ => {
                    // ffmpeg not available — try afconvert (macOS built-in)
                    let afconvert = tokio::process::Command::new("afconvert")
                        .args(["-f", "mp4f", "-d", "aac", &aiff_str, &out_path_str])
                        .output()
                        .await;
                    match afconvert {
                        Ok(o) if o.status.success() => {
                            let _ = std::fs::remove_file(&aiff_path);
                        }
                        _ => {
                            // Fallback: send aiff as-is (some platforms may not play it)
                            tracing::warn!("tts: ffmpeg/afconvert not available, using aiff");
                            let _ = std::fs::rename(&aiff_path, &out_path);
                        }
                    }
                }
            }
        } else if is_windows {
            let script = format!(
                r#"
Add-Type -AssemblyName System.Speech
$synth = New-Object System.Speech.Synthesis.SpeechSynthesizer
$synth.SetOutputToWaveFile('{}')
$synth.Speak('{}')
"#,
                out_path_str, text
            );
            let output = powershell_hidden()
                .args(["-Command", &script])
                .output()
                .await
                .map_err(|e| anyhow!("tts: PowerShell SAPI failed: {e}"))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(anyhow!("tts: SAPI failed: {stderr}"));
            }
        } else {
            let espeak_result = tokio::process::Command::new("espeak")
                .args(["-w", &out_path_str, text])
                .output()
                .await;
            match espeak_result {
                Ok(o) if o.status.success() => {}
                _ => {
                    let output = tokio::process::Command::new("pico2wave")
                        .args(["-w", &out_path_str, "--", text])
                        .output()
                        .await
                        .map_err(|e| anyhow!("tts: neither espeak nor pico2wave available: {e}"))?;
                    if !output.status.success() {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        return Err(anyhow!("tts: pico2wave failed: {stderr}"));
                    }
                }
            }
        }

        Ok(json!({
            "audio_file": out_path_str,
            "voice": voice,
            "chars": text.len()
        }))
    }

    // -------------------------------------------------------------------
    // Messaging
    // -------------------------------------------------------------------

    pub(crate) async fn tool_message(&self, args: Value) -> Result<Value> {
        let target = args["target"]
            .as_str()
            .ok_or_else(|| anyhow!("message: `target` required"))?;
        let text = args["text"]
            .as_str()
            .ok_or_else(|| anyhow!("message: `text` required"))?;
        let channel = args["channel"].as_str().unwrap_or("default");

        // Try to POST to the gateway's own message-send endpoint.
        let port = self.config.gateway.port;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{port}/api/v1/message/send"))
            .json(&json!({
                "channel": channel,
                "target": target,
                "text": text
            }))
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let body: Value = r.json().await.unwrap_or(json!({"ok": true}));
                Ok(json!({
                    "sent": true,
                    "channel": channel,
                    "target": target,
                    "response": body
                }))
            }
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                Err(anyhow!("message: gateway returned {status}: {body}"))
            }
            Err(e) => Err(anyhow!("message: failed to reach gateway: {e}")),
        }
    }

    // -------------------------------------------------------------------
    // AnyCLI — structured web data extraction
    // -------------------------------------------------------------------

    /// Extract structured data from websites using anycli adapters.
    pub(crate) async fn tool_anycli(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow!("anycli: `action` required"))?;

        match action {
            "list" => {
                let registry = anycli::Registry::load()?;
                let adapters: Vec<serde_json::Value> = registry
                    .list()
                    .iter()
                    .map(|a| {
                        json!({
                            "name": a.name,
                            "description": a.description,
                            "commands": a.commands.keys().collect::<Vec<_>>()
                        })
                    })
                    .collect();
                Ok(json!({"adapters": adapters}))
            }
            "info" => {
                let adapter_name = args["adapter"]
                    .as_str()
                    .or_else(|| args["name"].as_str())
                    .ok_or_else(|| anyhow!("anycli info: `adapter` required"))?;
                let registry = anycli::Registry::load()?;
                let adapter = registry.find(adapter_name)?;
                let commands: serde_json::Map<String, serde_json::Value> = adapter
                    .commands
                    .iter()
                    .map(|(name, cmd)| {
                        let params: serde_json::Map<String, serde_json::Value> = cmd
                            .params
                            .iter()
                            .map(|(k, v)| {
                                (k.clone(), json!({
                                    "type": v.param_type,
                                    "required": v.required,
                                    "default": v.default,
                                    "description": v.description,
                                }))
                            })
                            .collect();
                        (name.clone(), json!({"description": cmd.description, "params": params}))
                    })
                    .collect();
                Ok(json!({"name": adapter.name, "description": adapter.description, "base_url": adapter.base_url, "commands": commands}))
            }
            "run" => {
                let adapter_name = args["adapter"]
                    .as_str()
                    .ok_or_else(|| anyhow!("anycli run: `adapter` required"))?;
                let command = args["command"]
                    .as_str()
                    .ok_or_else(|| anyhow!("anycli run: `command` required"))?;
                let registry = anycli::Registry::load()?;
                let adapter = registry.find(adapter_name)?;
                let mut params_vec: Vec<(String, String)> = Vec::new();
                if let Some(obj) = args["params"].as_object() {
                    for (k, v) in obj {
                        let val = match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        params_vec.push((k.clone(), val));
                    }
                }
                let param_refs: Vec<(&str, &str)> = params_vec.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                let result = anycli::Pipeline::execute(adapter, command, &param_refs).await?;
                let fmt_str = args["format"].as_str().unwrap_or("json");
                let fmt: anycli::OutputFormat = fmt_str.parse().unwrap_or(anycli::OutputFormat::Json);
                Ok(json!({"adapter": result.adapter, "command": result.command, "count": result.count, "data": result.format(fmt)?}))
            }
            "search" => {
                let query = args["query"].as_str().ok_or_else(|| anyhow!("anycli search: `query` required"))?;
                let hub = anycli::Hub::new()?;
                let results = hub.search(query).await?;
                let entries: Vec<serde_json::Value> = results.iter().map(|e| json!({"name": e.name, "description": e.description})).collect();
                Ok(json!({"results": entries, "count": entries.len()}))
            }
            "install" => {
                let name = args["name"].as_str().or_else(|| args["adapter"].as_str()).ok_or_else(|| anyhow!("anycli install: `name` required"))?;
                let hub = anycli::Hub::new()?;
                let dir = anycli::hub::default_adapters_dir().ok_or_else(|| anyhow!("cannot determine home directory"))?;
                let path = hub.install(name, &dir).await?;
                Ok(json!({"installed": name, "path": path.display().to_string()}))
            }
            other => Err(anyhow!("anycli: unknown action `{other}`")),
        }
    }

    // -------------------------------------------------------------------
    // Clarify — ask the user a question before proceeding
    // -------------------------------------------------------------------

    /// Present a clarifying question to the user.
    pub(crate) async fn tool_clarify(&self, args: Value) -> Result<Value> {
        let question = args["question"]
            .as_str()
            .ok_or_else(|| anyhow!("clarify: `question` required"))?;
        let mut formatted = String::from(question);
        if let Some(options) = args["options"].as_array() {
            formatted.push('\n');
            for (i, opt) in options.iter().enumerate() {
                if let Some(s) = opt.as_str() {
                    formatted.push_str(&format!("\n{}. {}", i + 1, s));
                }
            }
        }
        Ok(json!({"action": "clarify", "question": formatted, "waiting_for_user": true}))
    }

    // -------------------------------------------------------------------
    // Gateway / pairing tools
    // -------------------------------------------------------------------

    pub(crate) async fn tool_gateway(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow!("gateway: `action` required"))?;

        let port = self.config.gateway.port;
        let version = env!("CARGO_PKG_VERSION");

        match action {
            "status" | "health" => Ok(json!({
                "status": "running",
                "version": version,
                "port": port,
                "agents": self.agents.as_ref().map(|r| r.all().len()).unwrap_or(0),
            })),
            "version" => Ok(json!({
                "version": version,
                "name": "rsclaw",
            })),
            other => Err(anyhow!(
                "gateway: unsupported action `{other}` (status, health, version)"
            )),
        }
    }

    pub(crate) async fn tool_pairing(&self, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow!("pairing: `action` required"))?;

        let port = self.config.gateway.port;
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}/api/v1");
        let auth_token = self
            .config
            .gateway
            .auth_token
            .as_deref()
            .unwrap_or_default();

        let auth_header = if auth_token.is_empty() {
            String::new()
        } else {
            format!("Bearer {auth_token}")
        };

        match action {
            "list" => {
                let mut req = client.get(format!("{base}/channels/pairings"));
                if !auth_header.is_empty() {
                    req = req.header("Authorization", &auth_header);
                }
                let resp = req.send().await?;
                let data: Value = resp.json().await?;
                Ok(data)
            }
            "approve" => {
                let code = args["code"]
                    .as_str()
                    .ok_or_else(|| anyhow!("pairing approve: `code` required"))?;
                let mut req = client
                    .post(format!("{base}/channels/pair"))
                    .json(&json!({"code": code}));
                if !auth_header.is_empty() {
                    req = req.header("Authorization", &auth_header);
                }
                let resp = req.send().await?;
                let data: Value = resp.json().await?;
                Ok(data)
            }
            "revoke" => {
                let channel = args["channel"]
                    .as_str()
                    .ok_or_else(|| anyhow!("pairing revoke: `channel` required"))?;
                let peer_id = args["peerId"]
                    .as_str()
                    .ok_or_else(|| anyhow!("pairing revoke: `peerId` required"))?;
                let mut req = client
                    .post(format!("{base}/channels/unpair"))
                    .json(&json!({"channel": channel, "peerId": peer_id}));
                if !auth_header.is_empty() {
                    req = req.header("Authorization", &auth_header);
                }
                let resp = req.send().await?;
                let data: Value = resp.json().await?;
                Ok(data)
            }
            other => Err(anyhow!(
                "pairing: unsupported action `{other}` (list, approve, revoke)"
            )),
        }
    }

    // -------------------------------------------------------------------
    // Consolidated memory tool handler
    // -------------------------------------------------------------------

    // -------------------------------------------------------------------
    // Document & PDF
    // -------------------------------------------------------------------

    pub(crate) async fn tool_doc(&self, args: Value) -> Result<Value> {
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| anyhow!("doc: `path` required"))?;

        let workspace = self
            .handle
            .config
            .workspace
            .as_deref()
            .or(self.config.agents.defaults.workspace.as_deref())
            .map(super::runtime::expand_tilde)
            .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));

        let pb = std::path::PathBuf::from(path_str);
        let full = if pb.is_absolute() { pb } else { workspace.join(path_str) };
        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        super::doc::handle(&args, &full).await
    }

    pub(crate) async fn tool_pdf(&self, args: Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| anyhow!("pdf: `path` required"))?;

        // If URL, download to temp file first.
        let local_path = if path.starts_with("http://") || path.starts_with("https://") {
            let tmp = std::env::temp_dir().join("rsclaw_pdf_download.pdf");
            let client = reqwest::Client::new();
            let bytes = client
                .get(path)
                .send()
                .await
                .map_err(|e| anyhow!("pdf: download failed: {e}"))?
                .bytes()
                .await
                .map_err(|e| anyhow!("pdf: download read failed: {e}"))?;
            tokio::fs::write(&tmp, &bytes)
                .await
                .map_err(|e| anyhow!("pdf: write temp file failed: {e}"))?;
            tmp
        } else {
            std::path::PathBuf::from(path)
        };

        // Pure Rust PDF extraction, with pdftotext CLI fallback.
        let pdf_bytes = tokio::fs::read(&local_path)
            .await
            .map_err(|e| anyhow!("pdf: read failed: {e}"))?;
        let text = match crate::agent::doc::safe_extract_pdf_from_mem(&pdf_bytes) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("pdf-extract failed ({e}), trying pdftotext CLI");
                let output = tokio::process::Command::new("pdftotext")
                    .args([local_path.to_str().unwrap_or(""), "-"])
                    .output()
                    .await
                    .map_err(|e2| anyhow!("pdf: extraction failed: {e}, pdftotext: {e2}"))?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(anyhow!("pdf: extraction failed: {e}, pdftotext: {stderr}"));
                }
                String::from_utf8_lossy(&output.stdout).into_owned()
            }
        };
        // Truncate to 100k chars to avoid blowing up context.
        let truncated = if text.len() > 100_000 {
            let mut end = 100_000usize;
            while end < text.len() && !text.is_char_boundary(end) {
                end += 1;
            }
            format!("{}...\n[truncated at 100000 chars]", &text[..end])
        } else {
            text
        };

        Ok(json!({
            "path": path,
            "text": truncated,
            "chars": truncated.len()
        }))
    }

    // -------------------------------------------------------------------
    // Consolidated memory tool handler
    // -------------------------------------------------------------------

    pub(crate) async fn tool_memory_consolidated(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        // Trim whitespace/newlines: the rsclaw v1 block protocol shards
        // tool_call input JSON across deltas and occasionally introduces
        // leading/trailing whitespace inside string values (seen in
        // production as e.g. `{"action": "\nsearch\n"}`). A bare match
        // against "search" then fails and the user gets a confusing
        // "unknown action 'search\n'" error.
        let action = args["action"].as_str().unwrap_or("search").trim();
        match action {
            "search" => self.tool_memory_search(args).await,
            "get" => self.tool_memory_get(args).await,
            "put" => self.tool_memory_put(ctx, args).await,
            "delete" => {
                // Memory deletion only allowed from internal channels (meditation/cron).
                // User conversations cannot delete memories — use /memory clear command instead.
                let ch = &ctx.channel;
                if ch != "system" && ch != "cron" && ch != "heartbeat" {
                    bail!("memory delete is not available in conversations. Use the /memory clear command instead.")
                }
                self.tool_memory_delete(args).await
            }
            _ => bail!("memory: unknown action '{action}' (search, get, put, delete)"),
        }
    }

    // -------------------------------------------------------------------
    // Tool installer
    // -------------------------------------------------------------------

    /// Install a tool/runtime via `rsclaw tools install`.
    pub(crate) async fn tool_install(&self, args: Value) -> Result<Value> {
        let name = args["name"]
            .as_str()
            .ok_or_else(|| anyhow!("tool_install: `name` required"))?;

        let exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "rsclaw".to_owned());

        let mut cmd = tokio::process::Command::new(&exe);
        cmd.args(["tools", "install", name])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let output = cmd.output()
            .await
            .map_err(|e| anyhow!("tool_install: failed to run: {e}"))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        // Post-install verification: check that the tool binary actually exists.
        // Prevents reporting success when only an empty directory was created.
        let verified = if output.status.success() {
            match name {
                "chrome" => super::platform::detect_chrome().is_some(),
                "ffmpeg" => super::platform::detect_ffmpeg().is_some(),
                "node" => which::which("node").is_ok()
                    || crate::config::loader::base_dir().join("tools/node/bin/node").exists(),
                "python" => which::which("python3").is_ok()
                    || crate::config::loader::base_dir().join("tools/python/bin/python3").exists(),
                _ => true, // skip verification for unknown tools
            }
        } else {
            false
        };

        Ok(json!({
            "name": name,
            "success": verified,
            "output": if stdout.is_empty() { &stderr } else { &stdout },
            "verified": verified,
        }))
    }

    // -------------------------------------------------------------------
    // Channel consolidated + actions
    // -------------------------------------------------------------------

    pub(crate) async fn tool_channel_consolidated(&self, args: Value) -> Result<Value> {
        let channel_type = args["channel"].as_str().unwrap_or("unknown").to_owned();
        self.tool_channel_actions(&channel_type, args).await
    }

    pub(crate) async fn tool_channel_actions(&self, channel_type: &str, args: Value) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow!("{channel_type}_actions: `action` required"))?;
        let chat_id = args["chatId"]
            .as_str()
            .or_else(|| args["chat_id"].as_str())
            .unwrap_or("");
        let text = args["text"].as_str().unwrap_or("");
        let message_id = args["messageId"]
            .as_str()
            .or_else(|| args["message_id"].as_str())
            .unwrap_or("");

        Ok(json!({
            "channel": channel_type,
            "action": action,
            "chatId": chat_id,
            "text": text,
            "messageId": message_id,
            "status": "stub",
            "note": format!(
                "{channel_type} action `{action}` received. \
                 Channel-specific API integration is not yet wired — \
                 use the `message` tool for basic send operations."
            )
        }))
    }

    /// Format a multi-choice question for the user and return the rendered
    /// text. The agent should use the returned `formatted_text` as its reply
    /// and end the turn; the user's response will arrive as a normal message
    /// on the next turn.
    ///
    /// L1 implementation: text-only rendering that works across all 13
    /// channels uniformly. L2 (per-channel structured cards — Feishu/Discord/
    /// Telegram inline keyboards) is a follow-up PR.
    pub(crate) async fn tool_ask_user(
        &self,
        ctx: &RunContext,
        args: Value,
    ) -> Result<Value> {
        let question = args["question"]
            .as_str()
            .ok_or_else(|| anyhow!("ask_user: `question` required"))?
            .trim()
            .to_owned();
        if question.is_empty() {
            bail!("ask_user: `question` must be non-empty");
        }

        let raw_options = args["options"]
            .as_array()
            .ok_or_else(|| anyhow!("ask_user: `options` array required (2-8 entries)"))?;
        if raw_options.len() < 2 {
            bail!("ask_user: at least 2 options required (a single-choice 'question' isn't a question)");
        }
        if raw_options.len() > 8 {
            bail!("ask_user: at most 8 options allowed — collapse rarely-picked variants into 'Other (free text)'");
        }

        let multi_select = args["multi_select"].as_bool().unwrap_or(false);
        let recommended_index = args["recommended_index"].as_u64().map(|n| n as usize);
        let header = args["header"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);

        // Build structured options once, used for both the WS prompt payload
        // (L2: capable channels render natively) and the formatted text
        // fallback (L1: every channel works).
        let mut options: Vec<crate::events::AskUserOption> = Vec::with_capacity(raw_options.len());
        for (idx, opt) in raw_options.iter().enumerate() {
            let label = opt["label"]
                .as_str()
                .ok_or_else(|| anyhow!("ask_user: option[{idx}].label required"))?
                .trim()
                .to_owned();
            if label.is_empty() {
                bail!("ask_user: option[{idx}].label must be non-empty");
            }
            let description = opt["description"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            options.push(crate::events::AskUserOption { label, description });
        }

        // Render the L1 text fallback. Numbered list with optional
        // recommendation marker and description suffix.
        let mut formatted = String::new();
        if let Some(ref h) = header {
            formatted.push_str(&format!("[{h}] "));
        }
        formatted.push_str("❓ ");
        formatted.push_str(&question);
        formatted.push_str("\n\n");
        for (idx, opt) in options.iter().enumerate() {
            let is_recommended = recommended_index == Some(idx);
            formatted.push_str(&format!("{}) {}", idx + 1, opt.label));
            if is_recommended {
                formatted.push_str(" (Recommended)");
            }
            if let Some(ref d) = opt.description {
                formatted.push_str(&format!(" — {d}"));
            }
            formatted.push('\n');
        }
        formatted.push('\n');
        if multi_select {
            formatted.push_str("请回复一个或多个选项编号 (逗号分隔, e.g. \"1,3\"), 或自由输入。");
        } else {
            formatted.push_str("请回复选项编号 (e.g. \"1\"), 或自由输入。");
        }

        // L2 path: emit a side-channel AgentEvent carrying the structured
        // prompt. Capable subscribers (Desktop, Telegram, Feishu, ...) render
        // native UI; uncapable subscribers ignore the `question` field and
        // fall back to the agent's plain-text reply.
        if let Some(ref bus) = self.event_bus {
            let prompt = crate::events::AskUserPrompt {
                question: question.clone(),
                options,
                multi_select,
                recommended_index,
                header,
            };
            let _ = bus.send(crate::events::AgentEvent {
                session_id: ctx.session_key.clone(),
                agent_id: ctx.agent_id.clone(),
                delta: String::new(),
                done: false,
                files: vec![],
                images: vec![],
                tool_log: vec![],
                question: Some(prompt),
            });
        }

        Ok(json!({
            "ok": true,
            "formatted_text": formatted,
            "option_count": raw_options.len(),
            "multi_select": multi_select,
            "instruction": "Send `formatted_text` as your reply to the user verbatim, then \
                            STOP this turn. The user's answer arrives as a normal message \
                            on the next turn — parse a digit as option index, or treat \
                            free text as 'Other'."
        }))
    }

    /// Validate and stage a structured task outcome declared by the agent.
    ///
    /// Stages the outcome in a session-keyed stash that the task-queue worker
    /// drains before classifying the turn — once drained, it becomes
    /// `TaskOutcome::Structured`, taking precedence over the string classifier.
    pub(crate) async fn tool_task_finish(
        &self,
        ctx: &RunContext,
        args: Value,
    ) -> Result<Value> {
        let outcome: crate::gateway::task_queue::StructuredOutcome =
            serde_json::from_value(args.clone()).map_err(|e| {
                anyhow!(
                    "task_finish: invalid outcome payload: {e}. Required fields: \
                     completion (full|partial|minimal|failed) and recommend \
                     (ship|continue|needs_human|retry|abandon)."
                )
            })?;

        if outcome.verified && outcome.verification_log.is_none() {
            bail!(
                "task_finish: verified=true requires verification_log with command + \
                 output excerpt. Either provide evidence or set verified=false."
            );
        }

        if matches!(
            outcome.completion,
            crate::gateway::task_queue::Completion::Full
        ) && outcome.accomplished.is_empty()
        {
            bail!(
                "task_finish: completion=full requires non-empty `accomplished`. \
                 List the concrete things you did, each mapped to an observable \
                 artifact (file changed, command run, message sent)."
            );
        }

        tracing::info!(
            session_key = %ctx.session_key,
            completion = ?outcome.completion,
            recommend = ?outcome.recommend,
            verified = outcome.verified,
            accomplished_count = outcome.accomplished.len(),
            blocked_count = outcome.blocked_on.len(),
            follow_up_count = outcome.follow_up_tasks.len(),
            "task_finish: agent declared outcome"
        );

        crate::gateway::task_queue::stage_pending_outcome(&ctx.session_key, outcome);

        Ok(json!({
            "ok": true,
            "recorded": true,
            "note": "Outcome staged. Worker will use it instead of string-classifier \
                     fallback when grading this turn."
        }))
    }
}

/// Located VITS TTS model under `<base>/models/vits-*/`.
///
/// `lexicon` / `data_dir` / `rule_fsts` are optional — only the Chinese
/// VITS bundles (e.g. vits-zh-hf-theresa) ship them. Pure-English bundles
/// only need `model` + `tokens`. The auto-TTS caller passes whichever
/// fields are populated as CLI flags so the same logic handles both.
///
/// `rule_fsts` is a comma-joined absolute-path list — the Chinese
/// theresa bundle ships `phone.fst,date.fst,number.fst,new_heteronym.fst`
/// and sherpa-onnx expects all four, otherwise dates / numbers /
/// heteronyms come out garbled.
struct VitsModel {
    model: std::path::PathBuf,
    tokens: std::path::PathBuf,
    lexicon: Option<std::path::PathBuf>,
    data_dir: Option<std::path::PathBuf>,
    rule_fsts: Option<String>,
}

/// Find an installed VITS TTS model under `<base>/models/`.
///
/// Search order picks the first directory matching `vits-*` (today the
/// bundle is `vits-theresa`; future English / multilingual bundles go in
/// the same place). File names inside the directory follow the standard
/// sherpa-onnx VITS layout — `*.onnx` for the model, `tokens.txt`,
/// optional `lexicon.txt`, optional `dict/`, optional `rule.fst`.
fn find_vits_model() -> Option<VitsModel> {
    let models_root = crate::config::loader::base_dir().join("models");
    let entries = std::fs::read_dir(&models_root).ok()?;
    let mut candidates: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("vits-"))
        })
        .collect();

    // Explicit priority: prefer MeloTTS (best quality, ~300MB) over the
    // lightweight HuggingFace community models (theresa et al). Anything
    // not in the priority list falls to alphabetical order. Reordering by
    // hand rather than relying on lexicographic sort means future bundles
    // with arbitrary names don't accidentally win.
    const PRIORITY: &[&str] = &[
        "vits-melo-tts-zh_en",
        "vits-melo-tts-zh",
        "vits-zh-aishell3",
        "vits-theresa",
        "vits-zh-hf-theresa",
    ];
    candidates.sort_by_key(|p| {
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned();
        let pri = PRIORITY
            .iter()
            .position(|n| *n == name.as_str())
            .unwrap_or(usize::MAX);
        (pri, name)
    });
    let candidate_dirs = candidates;

    for dir in candidate_dirs {
        let mut model: Option<std::path::PathBuf> = None;
        let mut tokens: Option<std::path::PathBuf> = None;
        let mut lexicon: Option<std::path::PathBuf> = None;
        let mut fst_paths: Vec<std::path::PathBuf> = Vec::new();
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in files.flatten() {
            let p = entry.path();
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let is_int8 = name.contains(".int8.");
            if name.ends_with(".onnx") && (model.is_none() || !is_int8) {
                model = Some(p.clone());
            } else if name == "tokens.txt" {
                tokens = Some(p.clone());
            } else if name == "lexicon.txt" {
                lexicon = Some(p.clone());
            } else if name.ends_with(".fst") {
                fst_paths.push(p.clone());
            }
        }
        // Stable ordering keeps the comma-joined string deterministic.
        fst_paths.sort();
        let rule_fsts = if fst_paths.is_empty() {
            None
        } else {
            Some(
                fst_paths
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(","),
            )
        };
        let data_dir = {
            let d = dir.join("dict");
            if d.is_dir() { Some(d) } else { None }
        };
        if let (Some(m), Some(t)) = (model, tokens) {
            return Some(VitsModel {
                model: m,
                tokens: t,
                lexicon,
                data_dir,
                rule_fsts,
            });
        }
    }
    None
}

