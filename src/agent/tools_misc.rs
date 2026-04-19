//! Miscellaneous tool handlers — image generation, TTS, messaging, gateway,
//! pairing, and memory consolidated dispatch.
//!
//! These are `impl AgentRuntime` methods extracted from `runtime.rs` for
//! maintainability. They compile as a split impl block.

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::platform::powershell_hidden;
use super::runtime::{AgentRuntime, RunContext};

impl AgentRuntime {
    // -----------------------------------------------------------------------
    // Image generation
    // -----------------------------------------------------------------------

    pub(crate) async fn tool_image(&self, args: Value) -> Result<Value> {
        let prompt = args["prompt"]
            .as_str()
            .ok_or_else(|| anyhow!("image: `prompt` required"))?;

        // Check user-configured image model: agents.defaults.model.image
        let user_image_model = self
            .handle
            .config
            .model
            .as_ref()
            .and_then(|m| m.image.as_deref())
            .or_else(|| {
                self.config
                    .agents
                    .defaults
                    .model
                    .as_ref()
                    .and_then(|m| m.image.as_deref())
            })
            .map(|s| s.to_owned());

        // Resolve provider — from image model config or current chat model
        let resolve_model = user_image_model.clone().unwrap_or_else(|| self.resolve_model_name());
        let (prov_name, user_model_id) = {
            crate::provider::registry::ProviderRegistry::parse_model(&resolve_model)
        };
        let (base_url, _auth_style) = crate::provider::defaults::resolve_base_url(prov_name);

        let default_size = match prov_name {
            _ => "2048x2048",
        };
        let size = args["size"].as_str().unwrap_or(default_size);

        // Also check provider config for api_key and base_url overrides
        let cfg_key = self
            .config
            .model
            .models
            .as_ref()
            .and_then(|m| m.providers.get(prov_name))
            .and_then(|p| p.api_key.as_ref())
            .and_then(|k| k.as_plain().map(str::to_owned));
        let cfg_url = self
            .config
            .model
            .models
            .as_ref()
            .and_then(|m| m.providers.get(prov_name))
            .and_then(|p| p.base_url.clone());

        // Providers with image generation support
        let image_providers = ["doubao", "bytedance", "openai", "qwen", "minimax", "gemini"];
        let (img_url, img_key, img_prov) = if image_providers.contains(&prov_name) {
            let url = cfg_url.unwrap_or(base_url);
            let key = cfg_key
                .or_else(|| std::env::var(format!("{}_API_KEY", prov_name.to_uppercase())).ok())
                .or_else(|| std::env::var("OPENAI_API_KEY").ok());
            (url, key, prov_name)
        } else {
            // Current provider doesn't support images — try doubao, qwen, openai
            let fallback = [("doubao", "ARK_API_KEY"), ("qwen", "DASHSCOPE_API_KEY"), ("minimax", "MINIMAX_API_KEY"), ("gemini", "GEMINI_API_KEY"), ("openai", "OPENAI_API_KEY")];
            let mut found = None;
            for (fb_prov, fb_env) in fallback {
                let fb_cfg = self
                    .config
                    .model
                    .models
                    .as_ref()
                    .and_then(|m| m.providers.get(fb_prov));
                let fb_key = fb_cfg
                    .and_then(|p| p.api_key.as_ref())
                    .and_then(|k| k.as_plain().map(str::to_owned))
                    .or_else(|| std::env::var(fb_env).ok());
                if let Some(key) = fb_key {
                    let fb_url = fb_cfg
                        .and_then(|p| p.base_url.clone())
                        .unwrap_or_else(|| crate::provider::defaults::resolve_base_url(fb_prov).0);
                    found = Some((fb_url, Some(key), fb_prov));
                    break;
                }
            }
            found.unwrap_or_else(|| (cfg_url.unwrap_or(base_url), None, prov_name))
        };
        let Some(api_key) = img_key else {
            return Ok(json!({
                "error": "AI image generation requires doubao, qwen, minimax, gemini, or openai provider with API key. No image-capable provider configured."
            }));
        };

        let image_model = args["model"].as_str()
            .or_else(|| if !user_model_id.is_empty() { Some(user_model_id) } else { None })
            .unwrap_or_else(|| match img_prov {
                "doubao" | "bytedance" => "doubao-seedream-5-0-260128",
                "openai" => "dall-e-3",
                "qwen" => "qwen-image-2.0-pro",
                "minimax" => "image-01",
                "gemini" => "gemini-3-pro-image-preview",
                _ => "dall-e-3",
            });

        // Resolve User-Agent: provider config -> gateway config -> default
        let img_ua = self.config.model.models.as_ref()
            .and_then(|m| m.providers.get(img_prov))
            .and_then(|p| p.user_agent.as_deref())
            .or_else(|| self.config.gateway.user_agent.as_deref())
            .unwrap_or(crate::provider::DEFAULT_USER_AGENT);
        let client = reqwest::Client::builder()
            .user_agent(img_ua)
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .unwrap_or_default();

        tracing::info!(provider = img_prov, model = image_model, size = size, ua = img_ua, "tool_image: generating");

        // Provider-specific API formats
        let is_qwen = img_prov == "qwen";
        let is_minimax = img_prov == "minimax";
        let is_gemini = img_prov == "gemini";
        let (resp_status, resp_body) = if is_qwen {
            let qwen_size = size.replace('x', "*");
            let resp = client
                .post("https://dashscope.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation")
                .header("Authorization", format!("Bearer {api_key}"))
                .json(&json!({
                    "model": image_model,
                    "input": { "messages": [{ "role": "user", "content": [{ "text": prompt }] }] },
                    "parameters": { "size": qwen_size, "n": 1, "watermark": false }
                }))
                .send().await
                .map_err(|e| anyhow!("image: request failed: {e}"))?;
            let st = resp.status();
            let body: Value = resp
                .json()
                .await
                .map_err(|e| anyhow!("image: parse error: {e}"))?;
            (st, body)
        } else if is_minimax {
            // Minimax: /v1/image_generation, aspect_ratio instead of size
            // Supported: "1:1", "16:9", "9:16", "4:3", "3:4", "2:3", "3:2"
            let aspect = if size.contains('x') {
                let parts: Vec<&str> = size.split('x').collect();
                if parts.len() == 2 {
                    let w = parts[0].parse::<f32>().unwrap_or(1024.0);
                    let h = parts[1].parse::<f32>().unwrap_or(1024.0);
                    let ratio = w / h.max(1.0);
                    let candidates = [
                        (1.0_f32, "1:1"),
                        (16.0 / 9.0, "16:9"),
                        (9.0 / 16.0, "9:16"),
                        (4.0 / 3.0, "4:3"),
                        (3.0 / 4.0, "3:4"),
                        (3.0 / 2.0, "3:2"),
                        (2.0 / 3.0, "2:3"),
                    ];
                    candidates
                        .iter()
                        .min_by(|a, b| {
                            (a.0 - ratio)
                                .abs()
                                .partial_cmp(&(b.0 - ratio).abs())
                                .unwrap()
                        })
                        .map(|c| c.1)
                        .unwrap_or("1:1")
                        .to_owned()
                } else {
                    "1:1".to_owned()
                }
            } else {
                "1:1".to_owned()
            };
            let url = format!("{}/image_generation", img_url.trim_end_matches('/'));
            let resp = client.post(&url)
                .header("Authorization", format!("Bearer {api_key}"))
                .json(&json!({ "model": image_model, "prompt": prompt, "aspect_ratio": aspect, "response_format": "url" }))
                .send().await
                .map_err(|e| anyhow!("image: request failed: {e}"))?;
            let st = resp.status();
            let body: Value = resp
                .json()
                .await
                .map_err(|e| anyhow!("image: parse error: {e}"))?;
            (st, body)
        } else if is_gemini {
            // Gemini: generateContent with responseModalities: ["IMAGE"]
            // Map size to aspect ratio for Gemini
            let aspect = if size.contains('x') {
                let parts: Vec<&str> = size.split('x').collect();
                if parts.len() == 2 {
                    let w = parts[0].parse::<u32>().unwrap_or(2048);
                    let h = parts[1].parse::<u32>().unwrap_or(2048);
                    if w == h { "1:1" } else if w > h { "16:9" } else { "9:16" }
                } else { "1:1" }
            } else { "1:1" };
            let gemini_base = img_url.trim_end_matches('/');
            let url = format!("{gemini_base}/models/{image_model}:generateContent?key={api_key}");
            let resp = client.post(&url)
                .json(&json!({
                    "contents": [{ "parts": [{ "text": prompt }] }],
                    "generationConfig": {
                        "responseModalities": ["TEXT", "IMAGE"],
                        "imageConfig": { "aspectRatio": aspect }
                    }
                }))
                .send().await
                .map_err(|e| anyhow!("image: gemini request failed: {e}"))?;
            let st = resp.status();
            let body: Value = resp.json().await.map_err(|e| anyhow!("image: gemini parse error: {e}"))?;
            (st, body)
        } else {
            let url = format!("{}/images/generations", img_url.trim_end_matches('/'));
            let resp = client.post(&url)
                .header("Authorization", format!("Bearer {api_key}"))
                .json(&json!({ "model": image_model, "prompt": prompt, "size": size, "n": 1, "response_format": "url" }))
                .send().await
                .map_err(|e| anyhow!("image: request failed: {e}"))?;
            let st = resp.status();
            let body: Value = resp
                .json()
                .await
                .map_err(|e| anyhow!("image: parse error: {e}"))?;
            (st, body)
        };

        if !resp_status.is_success() {
            let err_msg = resp_body["error"]["message"]
                .as_str()
                .or_else(|| resp_body["message"].as_str())
                .unwrap_or("unknown error");
            return Err(anyhow!("image: API error: {err_msg}"));
        }

        // Extract image URL/base64 — different response formats per provider
        // Gemini returns inline base64 directly, others return URLs
        if is_gemini {
            // Gemini: candidates[0].content.parts[] — find the inlineData part
            #[allow(unused_imports)]
            use base64::Engine;
            let parts = resp_body.pointer("/candidates/0/content/parts")
                .and_then(|v| v.as_array());
            if let Some(parts) = parts {
                for part in parts {
                    if let Some(inline) = part.get("inlineData") {
                        let mime = inline.get("mimeType").and_then(|v| v.as_str()).unwrap_or("image/png");
                        if let Some(b64_data) = inline.get("data").and_then(|v| v.as_str()) {
                            let data_uri = format!("data:{mime};base64,{b64_data}");
                            return Ok(json!({
                                "url": data_uri,
                                "revised_prompt": prompt
                            }));
                        }
                    }
                }
            }
            return Err(anyhow!("image: no image data in Gemini response"));
        }

        let img_url_str = if is_qwen {
            resp_body
                .pointer("/output/choices/0/message/content/0/image")
                .and_then(|v| v.as_str())
        } else if is_minimax {
            // minimax: data.image_base64[0] (base64) or data.image_urls[0] (url)
            resp_body.pointer("/data/image_urls/0").and_then(|v| v.as_str())
                .or_else(|| resp_body.pointer("/data/image_base64/0").and_then(|v| v.as_str()))
        } else {
            resp_body.pointer("/data/0/url").and_then(|v| v.as_str())
        };

        let Some(img_url_str) = img_url_str else {
            return Err(anyhow!("image: no image URL in response"));
        };

        // Download image and convert to data URI
        use base64::Engine;
        let image_result = match reqwest::Client::new()
            .get(img_url_str)
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => match r.bytes().await {
                Ok(bytes) => {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    format!("data:image/png;base64,{b64}")
                }
                Err(e) => return Err(anyhow!("image: download failed: {e}")),
            },
            Ok(r) => return Err(anyhow!("image: download returned {}", r.status())),
            Err(e) => return Err(anyhow!("image: download error: {e}")),
        };

        let revised = resp_body
            .pointer("/data/0/revised_prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        Ok(json!({
            "url": image_result,
            "revised_prompt": revised,
            "size": size,
            "model": image_model
        }))
    }

    // -----------------------------------------------------------------------
    // Video generation
    // -----------------------------------------------------------------------

    /// Generate a video from a text prompt.
    ///
    /// Supports Seedance (ByteDance ARK), MiniMax (Hailuo), and Kling (Kuaishou).
    /// All three use async task-based APIs: submit → poll → download.
    pub(crate) async fn tool_video(&self, args: Value) -> Result<Value> {
        let prompt = args["prompt"]
            .as_str()
            .ok_or_else(|| anyhow!("video_gen: `prompt` required"))?;
        let duration = args["duration"].as_u64().unwrap_or(5);
        let aspect_ratio = args["aspect_ratio"].as_str().unwrap_or("16:9");

        // Resolve configured video model (agents.defaults.model.video or handle override).
        let user_video_model = self
            .handle
            .config
            .model
            .as_ref()
            .and_then(|m| m.video.as_deref())
            .or_else(|| {
                self.config
                    .agents
                    .defaults
                    .model
                    .as_ref()
                    .and_then(|m| m.video.as_deref())
            })
            .map(|s| s.to_owned());

        // Allow caller to override model hint.
        let model_hint = args["model"].as_str().map(|s| s.to_lowercase());

        // Helper: resolve API key from provider config, then fallback to env var.
        let resolve_key = |prov: &str, env_name: &str| -> Option<String> {
            self.config
                .model
                .models
                .as_ref()
                .and_then(|m| m.providers.get(prov))
                .and_then(|p| p.api_key.as_ref())
                .and_then(|k| k.as_plain().map(str::to_owned))
                .or_else(|| std::env::var(env_name).ok())
        };

        // Determine provider from configured model or model_hint.
        let provider = if let Some(hint) = &model_hint {
            if hint.contains("llama") || hint.contains("local") {
                "llamacpp"
            } else if hint.contains("kling") || hint.contains("kuaishou") {
                "kling"
            } else if hint.contains("minimax") || hint.contains("hailuo") {
                "minimax"
            } else {
                "doubao"
            }
        } else if let Some(ref vm) = user_video_model {
            let vm = vm.to_lowercase();
            if vm.contains("llama") || vm.contains("local") || vm.starts_with("http://127.")
                || vm.starts_with("http://localhost")
            {
                "llamacpp"
            } else if vm.contains("kling") {
                "kling"
            } else if vm.contains("minimax") || vm.contains("hailuo") {
                "minimax"
            } else {
                "doubao"
            }
        } else {
            // Auto-detect: check provider config first, then env vars.
            let has_local = std::env::var("LLAMA_VIDEO_URL").is_ok();
            let has_ark = resolve_key("doubao", "ARK_API_KEY").is_some();
            let has_minimax = resolve_key("minimax", "MINIMAX_API_KEY").is_some();
            let has_kling = resolve_key("kling", "KLING_ACCESS_KEY").is_some()
                || std::env::var("KLING_ACCESS_KEY").is_ok();
            if has_local {
                "llamacpp"
            } else if has_ark {
                "doubao"
            } else if has_minimax {
                "minimax"
            } else if has_kling {
                "kling"
            } else {
                return Ok(json!({
                    "error": "No video provider configured. Configure a provider with API key in rsclaw.json5, or set env vars: LLAMA_VIDEO_URL, ARK_API_KEY, MINIMAX_API_KEY, KLING_ACCESS_KEY+KLING_SECRET_KEY."
                }));
            }
        };

        // Resolve API key for the selected provider from config -> env var.
        let api_key = match provider {
            "doubao" => resolve_key("doubao", "ARK_API_KEY"),
            "minimax" => resolve_key("minimax", "MINIMAX_API_KEY"),
            "kling" => None, // Kling uses access_key + secret_key pair, resolved inside video_gen_kling
            "llamacpp" => None, // local, no key needed
            _ => None,
        };

        // For Kling, resolve the key pair from config -> env var.
        let kling_keys = if provider == "kling" {
            let ak = resolve_key("kling", "KLING_ACCESS_KEY");
            let sk = self.config.model.models.as_ref()
                .and_then(|m| m.providers.get("kling"))
                .and_then(|p| {
                    // Secret key stored in a second field or as part of api_key "ak:sk" format
                    p.api_key.as_ref().and_then(|k| k.as_plain().map(str::to_owned))
                })
                .or_else(|| std::env::var("KLING_SECRET_KEY").ok());
            Some((ak, sk))
        } else {
            None
        };

        let ua = self
            .config
            .gateway
            .user_agent
            .as_deref()
            .unwrap_or(crate::provider::DEFAULT_USER_AGENT);
        let client = reqwest::Client::builder()
            .user_agent(ua)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default();

        let prompt_preview: String = prompt.chars().take(80).collect();
        tracing::info!(provider, prompt = prompt_preview, duration, aspect_ratio, "tool_video: starting");

        let video_url = match provider {
            "llamacpp" => {
                video_gen_llamacpp(&client, prompt, duration, aspect_ratio, user_video_model.as_deref()).await?
            }
            "doubao" => {
                let key = api_key.ok_or_else(|| anyhow!("video_gen: no API key for doubao/Seedance"))?;
                video_gen_seedance(&client, &key, prompt, duration, aspect_ratio, user_video_model.as_deref()).await?
            }
            "minimax" => {
                let key = api_key.ok_or_else(|| anyhow!("video_gen: no API key for MiniMax"))?;
                video_gen_minimax(&client, &key, prompt, duration, aspect_ratio, user_video_model.as_deref()).await?
            }
            "kling" => {
                let (ak, sk) = kling_keys.unwrap_or((None, None));
                let access = ak.ok_or_else(|| anyhow!("video_gen: KLING_ACCESS_KEY not configured"))?;
                let secret = sk.ok_or_else(|| anyhow!("video_gen: KLING_SECRET_KEY not configured"))?;
                video_gen_kling(&client, &access, &secret, prompt, duration, aspect_ratio, user_video_model.as_deref()).await?
            }
            _ => bail!("video_gen: unknown provider {provider}"),
        };

        // Resolve video to a local temp file (download URL, copy local path, or decode base64).
        let ext = if video_url.ends_with(".gif") { "gif" } else { "mp4" };
        let tmp_path = std::env::temp_dir().join(format!(
            "rsclaw_video_{}.{ext}",
            chrono::Utc::now().timestamp_millis()
        ));

        if video_url.starts_with("data:") {
            // base64 data URI: data:video/mp4;base64,<data>
            use base64::Engine;
            let b64 = video_url
                .splitn(2, ',')
                .nth(1)
                .ok_or_else(|| anyhow!("video_gen: malformed data URI"))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| anyhow!("video_gen: base64 decode failed: {e}"))?;
            std::fs::write(&tmp_path, &bytes)
                .map_err(|e| anyhow!("video_gen: write temp file failed: {e}"))?;
        } else if video_url.starts_with('/') || video_url.starts_with("./") {
            // Local file path returned by llama.cpp server — copy to our temp path.
            std::fs::copy(&video_url, &tmp_path)
                .map_err(|e| anyhow!("video_gen: copy local file failed: {e}"))?;
        } else {
            // HTTP URL — download.
            let dl_client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_default();
            let video_bytes = dl_client
                .get(&video_url)
                .send()
                .await
                .map_err(|e| anyhow!("video_gen: download failed: {e}"))?
                .bytes()
                .await
                .map_err(|e| anyhow!("video_gen: read bytes failed: {e}"))?;
            std::fs::write(&tmp_path, &video_bytes)
                .map_err(|e| anyhow!("video_gen: write temp file failed: {e}"))?;
        }

        let filename = format!("video_{}.{ext}", chrono::Utc::now().format("%Y%m%d_%H%M%S"));
        let file_size = std::fs::metadata(&tmp_path).map(|m| m.len()).unwrap_or(0);
        tracing::info!(path = %tmp_path.display(), bytes = file_size, "tool_video: done");

        Ok(json!({
            "__send_file": true,
            "path": tmp_path.to_string_lossy(),
            "filename": filename,
            "mime_type": if ext == "gif" { "image/gif" } else { "video/mp4" }
        }))
    }

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

        // Try sherpa-onnx first (installed via `rsclaw tools install sherpa-onnx`).
        let sherpa_bin = crate::config::loader::base_dir()
            .join("tools")
            .join("sherpa-onnx")
            .join("bin")
            .join(if cfg!(target_os = "windows") { "sherpa-onnx-offline-tts.exe" } else { "sherpa-onnx-offline-tts" });

        if sherpa_bin.exists() {
            let model_dir = crate::config::loader::base_dir()
                .join("tools")
                .join("sherpa-onnx")
                .join("models")
                .join("tts");
            // Look for any VITS model config.
            let model_config = model_dir.join("model.onnx");
            if model_config.exists() {
                let mut cmd = tokio::process::Command::new(&sherpa_bin);
                cmd.args([
                    "--vits-model", model_config.to_str().unwrap_or(""),
                    "--vits-tokens", model_dir.join("tokens.txt").to_str().unwrap_or(""),
                    "--output-filename", &out_str,
                    "--vits-length-scale", "1.0",
                    tts_text,
                ]);
                #[cfg(target_os = "windows")]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x08000000);
                }
                let output = cmd.output().await;
                if let Ok(o) = output {
                    if o.status.success() && out_path.exists() {
                        return Ok(out_str);
                    }
                }
                // Fall through to system TTS if sherpa-onnx failed.
            }
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

        // Auto-detect Chinese text and use Chinese voice on macOS.
        let has_cjk = text.chars().any(|c| {
            matches!(c, '\u{4e00}'..='\u{9fff}' | '\u{3400}'..='\u{4dbf}')
        });
        let effective_voice = if voice == "default" && has_cjk {
            "Tingting" // macOS Chinese (Mandarin) voice
        } else {
            voice
        };

        let ts = chrono::Utc::now().timestamp_millis();
        let tmp_dir = std::env::temp_dir();
        // Output mp3 (most compatible). Feishu converts to opus at send time.
        let out_path = tmp_dir.join(format!("rsclaw_tts_{ts}.mp3"));
        let out_path_str = out_path.to_string_lossy().to_string();

        let is_macos = cfg!(target_os = "macos");
        let is_windows = cfg!(target_os = "windows");

        if is_macos {
            let aiff_path = tmp_dir.join(format!("rsclaw_tts_{ts}.aiff"));
            let aiff_str = aiff_path.to_string_lossy().to_string();
            let mut cmd = tokio::process::Command::new("say");
            if effective_voice != "default" {
                cmd.args(["-v", effective_voice]);
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
            // Convert aiff to mp3 via ffmpeg (most compatible format).
            // Feishu converts to opus at send time.
            let ffmpeg = tokio::process::Command::new("ffmpeg")
                .args(["-i", &aiff_str, "-y", "-q:a", "4", &out_path_str])
                .output()
                .await;
            match ffmpeg {
                Ok(o) if o.status.success() => {
                    let _ = std::fs::remove_file(&aiff_path);
                }
                _ => {
                    tracing::warn!("tts: ffmpeg not available, using aiff");
                    let _ = std::fs::rename(&aiff_path, &out_path);
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

        // Return with __send_file so the file is auto-sent to the user
        // without requiring the LLM to call send_file separately.
        let filename = std::path::Path::new(&out_path_str)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "tts.mp3".to_owned());
        Ok(json!({
            "__send_file": true,
            "path": out_path_str,
            "filename": filename,
            "audio_file": out_path_str,
            "voice": effective_voice,
            "chars": text.len(),
            "auto_sent": true,
            "note": "Audio file has been auto-sent to the user. Do NOT call send_file again."
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
        let action = args["action"].as_str().unwrap_or("search");
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

        Ok(json!({
            "name": name,
            "success": output.status.success(),
            "output": if stdout.is_empty() { &stderr } else { &stdout },
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
}

// ---------------------------------------------------------------------------
// Video generation — per-provider free functions
// ---------------------------------------------------------------------------

/// Generate video via a local llama.cpp-compatible video model server.
///
/// Uses the same OpenAI-compatible streaming protocol as regular llama.cpp.
/// Configure with `LLAMA_VIDEO_URL` (default: `http://127.0.0.1:8080`) and
/// optionally `LLAMA_VIDEO_MODEL`.
///
/// The server is expected to stream the video data and return either:
/// - A local file path in the response text (e.g. `/tmp/output.mp4`)
/// - A `data:video/mp4;base64,...` URI
/// - A `http://127.x.x.x/...` URL pointing to the generated file
async fn video_gen_llamacpp(
    client: &reqwest::Client,
    prompt: &str,
    duration: u64,
    aspect_ratio: &str,
    model_override: Option<&str>,
) -> anyhow::Result<String> {
    let base = std::env::var("LLAMA_VIDEO_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_owned());
    let base = base.trim_end_matches('/');
    let model = model_override
        .or_else(|| std::env::var("LLAMA_VIDEO_MODEL").ok().as_deref().map(|_| ""))
        .unwrap_or("default");
    let model = if model.is_empty() {
        std::env::var("LLAMA_VIDEO_MODEL").unwrap_or_else(|_| "default".to_owned())
    } else {
        model.to_owned()
    };

    let system = format!(
        "You are a video generation model. Generate a {duration}-second video with aspect ratio {aspect_ratio}. \
         Output the generated video as a file path or base64 data URI."
    );

    // Use OpenAI-compatible chat completions with streaming.
    use futures::StreamExt;
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({
            "model": model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": prompt}
            ],
            "stream": true,
            "max_tokens": 4096
        }))
        .timeout(std::time::Duration::from_secs(600))
        .send()
        .await
        .map_err(|e| anyhow!("llamacpp video: request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("llamacpp video: server returned {status}: {body}");
    }

    // Collect the full streamed text response.
    let mut full_text = String::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| anyhow!("llamacpp video: stream error: {e}"))?;
        let text = String::from_utf8_lossy(&chunk);
        // SSE lines: "data: {...}\n\n" or "data: [DONE]\n\n"
        for line in text.lines() {
            let line = line.trim();
            if !line.starts_with("data: ") {
                continue;
            }
            let payload = line.strip_prefix("data: ").unwrap_or(line);
            if payload == "[DONE]" {
                break;
            }
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(payload) {
                if let Some(delta) = val.pointer("/choices/0/delta/content").and_then(|v| v.as_str()) {
                    full_text.push_str(delta);
                }
            }
        }
    }

    let result = full_text.trim().to_owned();
    if result.is_empty() {
        bail!("llamacpp video: empty response from server");
    }

    tracing::info!(len = result.len(), "llamacpp video: got response");

    // The result is either a file path, a URL, or a base64 data URI —
    // return it directly; tool_video will handle download/copy.
    Ok(result)
}

/// Generate video via ByteDance ARK Seedance 2.0.
///
/// API: POST /api/v3/contents/generations/tasks → task_id
/// Poll: GET /api/v3/contents/generations/tasks/{id} until status == "succeeded"
async fn video_gen_seedance(
    client: &reqwest::Client,
    api_key: &str,
    prompt: &str,
    duration: u64,
    aspect_ratio: &str,
    model_override: Option<&str>,
) -> anyhow::Result<String> {
    let model = model_override.unwrap_or("doubao-seedance-2-0-260128");
    let base = "https://ark.cn-beijing.volces.com/api/v3";

    // Submit task.
    let body = json!({
        "model": model,
        "content": [{"type": "text", "text": prompt}],
        "ratio": aspect_ratio,
        "duration": duration,
        "watermark": false
    });
    let resp: serde_json::Value = client
        .post(format!("{base}/contents/generations/tasks"))
        .bearer_auth(&api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("seedance: submit failed: {e}"))?
        .json()
        .await
        .map_err(|e| anyhow!("seedance: submit parse failed: {e}"))?;

    let task_id = resp["id"]
        .as_str()
        .ok_or_else(|| anyhow!("seedance: no task id in response: {resp}"))?
        .to_owned();

    tracing::info!(task_id, "seedance: task submitted, polling");

    // Poll until done (max 10 min, every 5s).
    for _ in 0..120 {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let poll: serde_json::Value = client
            .get(format!("{base}/contents/generations/tasks/{task_id}"))
            .bearer_auth(&api_key)
            .send()
            .await
            .map_err(|e| anyhow!("seedance: poll failed: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow!("seedance: poll parse failed: {e}"))?;

        let status = poll["status"].as_str().unwrap_or("unknown");
        tracing::debug!(task_id, status, "seedance: poll");

        match status {
            "succeeded" => {
                // ARK content generation response:
                // {content: [{type:"video_url", video_url:{url:"..."}}]}
                // Actual API returns {content: {video_url: "https://..."}}
                // (not array format from docs).
                let url = poll
                    .pointer("/content/video_url")
                    .or_else(|| poll.pointer("/content/0/video_url/url"))
                    .or_else(|| poll.pointer("/content/0/url"))
                    .or_else(|| poll.pointer("/result/video_url/url"))
                    .or_else(|| poll.pointer("/output/url"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("seedance: no video URL in result: {poll}"))?
                    .to_owned();
                return Ok(url);
            }
            "failed" | "cancelled" => {
                let msg = poll["error"]["message"]
                    .as_str()
                    .or_else(|| poll["message"].as_str())
                    .unwrap_or("task failed");
                bail!("seedance: task {task_id} {status}: {msg}");
            }
            _ => continue,
        }
    }
    bail!("seedance: task {task_id} timed out after 10 minutes")
}

/// Generate video via MiniMax (Hailuo) video generation API.
///
/// API: POST /v1/video_generation → task_id
/// Poll: GET /v1/query/video_generation?task_id={id}
/// File: GET /v1/files/retrieve?file_id={id} → download_url
async fn video_gen_minimax(
    client: &reqwest::Client,
    api_key: &str,
    prompt: &str,
    duration: u64,
    aspect_ratio: &str,
    model_override: Option<&str>,
) -> anyhow::Result<String> {
    let model = model_override.unwrap_or("video-01-director");
    let base = "https://api.minimaxi.com/v1";

    let resolution = match aspect_ratio {
        "9:16" => "720x1280",
        "1:1" => "720x720",
        _ => "1280x720",
    };

    // Submit.
    let resp: serde_json::Value = client
        .post(format!("{base}/video_generation"))
        .bearer_auth(&api_key)
        .json(&json!({
            "prompt": prompt,
            "model": model,
            "duration": duration,
            "resolution": resolution
        }))
        .send()
        .await
        .map_err(|e| anyhow!("minimax: submit failed: {e}"))?
        .json()
        .await
        .map_err(|e| anyhow!("minimax: submit parse failed: {e}"))?;

    let task_id = resp["task_id"]
        .as_str()
        .ok_or_else(|| anyhow!("minimax: no task_id in response: {resp}"))?
        .to_owned();

    tracing::info!(task_id, "minimax: task submitted, polling");

    // Poll until done.
    for _ in 0..120 {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let poll: serde_json::Value = client
            .get(format!("{base}/query/video_generation"))
            .bearer_auth(&api_key)
            .query(&[("task_id", task_id.as_str())])
            .send()
            .await
            .map_err(|e| anyhow!("minimax: poll failed: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow!("minimax: poll parse failed: {e}"))?;

        let status = poll
            .pointer("/task/status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        tracing::debug!(task_id, status, "minimax: poll");

        match status {
            "Success" => {
                // file_id → retrieve download URL
                let file_id = poll
                    .pointer("/task/file_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("minimax: no file_id in result: {poll}"))?
                    .to_owned();
                let file_resp: serde_json::Value = client
                    .get(format!("{base}/files/retrieve"))
                    .bearer_auth(&api_key)
                    .query(&[("file_id", file_id.as_str())])
                    .send()
                    .await
                    .map_err(|e| anyhow!("minimax: file retrieve failed: {e}"))?
                    .json()
                    .await
                    .map_err(|e| anyhow!("minimax: file retrieve parse failed: {e}"))?;
                let url = file_resp
                    .pointer("/file/download_url")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("minimax: no download_url: {file_resp}"))?
                    .to_owned();
                return Ok(url);
            }
            "Fail" => {
                bail!("minimax: task {task_id} failed: {poll}");
            }
            _ => continue,
        }
    }
    bail!("minimax: task {task_id} timed out after 10 minutes")
}

/// Generate video via Kling (Kuaishou) API.
///
/// Auth uses JWT (HS256) signed with KLING_ACCESS_KEY + KLING_SECRET_KEY.
/// API: POST /v1/videos/text2video → task_id
/// Poll: GET /v1/videos/text2video/{task_id}
async fn video_gen_kling(
    client: &reqwest::Client,
    access_key: &str,
    secret_key: &str,
    prompt: &str,
    duration: u64,
    aspect_ratio: &str,
    model_override: Option<&str>,
) -> anyhow::Result<String> {
    let model = model_override.unwrap_or("kling-v2-master");
    let base = "https://api.klingai.com";

    let jwt = kling_jwt(&access_key, &secret_key)?;

    let duration_str = duration.to_string();
    let resp: serde_json::Value = client
        .post(format!("{base}/v1/videos/text2video"))
        .bearer_auth(&jwt)
        .json(&json!({
            "model_name": model,
            "prompt": prompt,
            "duration": duration_str,
            "aspect_ratio": aspect_ratio
        }))
        .send()
        .await
        .map_err(|e| anyhow!("kling: submit failed: {e}"))?
        .json()
        .await
        .map_err(|e| anyhow!("kling: submit parse failed: {e}"))?;

    let task_id = resp
        .pointer("/data/task_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("kling: no task_id in response: {resp}"))?
        .to_owned();

    tracing::info!(task_id, "kling: task submitted, polling");

    for _ in 0..120 {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        // Refresh JWT (expires in 30 min, but refresh each poll to be safe for long videos).
        let jwt = kling_jwt(&access_key, &secret_key)?;
        let poll: serde_json::Value = client
            .get(format!("{base}/v1/videos/text2video/{task_id}"))
            .bearer_auth(&jwt)
            .send()
            .await
            .map_err(|e| anyhow!("kling: poll failed: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow!("kling: poll parse failed: {e}"))?;

        let status = poll
            .pointer("/data/task_status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        tracing::debug!(task_id, status, "kling: poll");

        match status {
            "succeed" => {
                let url = poll
                    .pointer("/data/task_result/videos/0/url")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("kling: no video URL in result: {poll}"))?
                    .to_owned();
                return Ok(url);
            }
            "failed" => {
                let msg = poll
                    .pointer("/data/task_status_msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("task failed");
                bail!("kling: task {task_id} failed: {msg}");
            }
            _ => continue,
        }
    }
    bail!("kling: task {task_id} timed out after 10 minutes")
}

/// Build a short-lived JWT for Kling API authentication (HS256).
fn kling_jwt(access_key: &str, secret_key: &str) -> anyhow::Result<String> {
    use base64::Engine;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let now = chrono::Utc::now().timestamp();
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(r#"{"alg":"HS256","typ":"JWT"}"#);
    let payload_json = format!(
        r#"{{"iss":"{access_key}","exp":{},"nbf":{}}}"#,
        now + 1800,
        now - 5
    );
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&payload_json);
    let signing_input = format!("{header}.{payload}");

    let mut mac = Hmac::<Sha256>::new_from_slice(secret_key.as_bytes())
        .map_err(|e| anyhow!("kling_jwt: invalid key: {e}"))?;
    mac.update(signing_input.as_bytes());
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());

    Ok(format!("{signing_input}.{sig}"))
}
