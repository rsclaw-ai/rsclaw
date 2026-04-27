//! Image generation tool — `tool_image` plus per-provider HTTP code
//! (Doubao / OpenAI / Qwen / MiniMax / Gemini).
//!
//! Split from `tools_misc.rs` for maintainability. Methods live in
//! `impl AgentRuntime` via the split-impl pattern (same struct,
//! different file).

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

/// Persist generated image bytes to `~/Downloads/rsclaw/images/` with the
/// canonical `dl_i_<YYYYMMDDHHmm><abc>.<ext>` filename and return the
/// absolute path. Avoids shipping multi-MB base64 over the WebSocket — the
/// desktop UI loads via Tauri's asset protocol; non-WS channels rehydrate
/// to a data URL at the AgentReply boundary (`image_ref_to_data_url`).
async fn save_generated_image_bytes(bytes: &[u8], mime: &str) -> Result<String> {
    let ext = match mime {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    };
    let kind = crate::channel::kind_from_extension(ext);
    let category = crate::channel::category_for_kind(kind);
    let save_dir = dirs_next::download_dir()
        .unwrap_or_else(|| {
            dirs_next::home_dir()
                .unwrap_or_else(crate::config::loader::base_dir)
                .join("Downloads")
        })
        .join("rsclaw")
        .join(category);
    tokio::fs::create_dir_all(&save_dir)
        .await
        .map_err(|e| anyhow!("image: create_dir: {e}"))?;
    let ts = chrono::Local::now().format("%Y%m%d%H%M").to_string();
    let abc: String = (0..3)
        .map(|_| (rand::random::<u8>() % 26 + b'a') as char)
        .collect();
    let save_path = save_dir.join(format!("dl_{kind}_{ts}{abc}.{ext}"));
    tokio::fs::write(&save_path, bytes)
        .await
        .map_err(|e| anyhow!("image: write: {e}"))?;
    Ok(save_path.to_string_lossy().into_owned())
}

impl super::runtime::AgentRuntime {
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
                            let bytes = base64::engine::general_purpose::STANDARD
                                .decode(b64_data)
                                .map_err(|e| anyhow!("image: gemini base64 decode: {e}"))?;
                            let path = save_generated_image_bytes(&bytes, mime).await?;
                            return Ok(json!({
                                "image_path": path,
                                "mime": mime,
                                "revised_prompt": prompt
                            }));
                        }
                    }
                }
            }
            return Err(anyhow!("image: no image data in Gemini response"));
        }

        // Each provider may return either a fetchable URL or inline base64.
        // We normalise both into raw bytes + mime, then save to disk.
        let img_ref = if is_qwen {
            resp_body
                .pointer("/output/choices/0/message/content/0/image")
                .and_then(|v| v.as_str())
        } else if is_minimax {
            // minimax: data.image_urls[0] (url) or data.image_base64[0] (base64)
            resp_body.pointer("/data/image_urls/0").and_then(|v| v.as_str())
                .or_else(|| resp_body.pointer("/data/image_base64/0").and_then(|v| v.as_str()))
        } else {
            // OpenAI/Doubao/etc: prefer url, fall back to b64_json (b64_json is what
            // OpenAI's response_format=b64_json returns, and some compatible
            // providers return it even when url is requested).
            resp_body.pointer("/data/0/url").and_then(|v| v.as_str())
                .or_else(|| resp_body.pointer("/data/0/b64_json").and_then(|v| v.as_str()))
        };

        let Some(img_ref) = img_ref else {
            return Err(anyhow!("image: no image data in response"));
        };

        // Resolve `img_ref` → bytes + mime.  Three shapes are accepted:
        //   * `data:image/...;base64,<b64>`   inline data URL (Gemini-style)
        //   * `http(s)://...`                  download via reqwest
        //   * `<raw base64>`                   minimax `image_base64`, OpenAI `b64_json`
        use base64::Engine as _;
        let (bytes, mime): (Vec<u8>, &str) = if let Some(rest) = img_ref.strip_prefix("data:") {
            // data:<mime>;base64,<b64>
            let (header, b64) = rest.split_once(',').unwrap_or(("image/png;base64", rest));
            let mime = header.split(';').next().unwrap_or("image/png");
            let mime_static: &str = match mime {
                "image/jpeg" | "image/jpg" => "image/jpeg",
                "image/webp" => "image/webp",
                "image/gif" => "image/gif",
                _ => "image/png",
            };
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .map_err(|e| anyhow!("image: base64 decode: {e}"))?;
            (bytes, mime_static)
        } else if img_ref.starts_with("http://") || img_ref.starts_with("https://") {
            let resp = reqwest::Client::new()
                .get(img_ref)
                .timeout(std::time::Duration::from_secs(60))
                .send()
                .await
                .map_err(|e| anyhow!("image: download error: {e}"))?;
            if !resp.status().is_success() {
                return Err(anyhow!("image: download returned {}", resp.status()));
            }
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| anyhow!("image: download failed: {e}"))?
                .to_vec();
            let mime: &str = if img_ref.ends_with(".jpg") || img_ref.ends_with(".jpeg") {
                "image/jpeg"
            } else if img_ref.ends_with(".webp") {
                "image/webp"
            } else {
                "image/png"
            };
            (bytes, mime)
        } else {
            // Treat as raw base64 (no `data:` prefix) — minimax image_base64 /
            // OpenAI b64_json fall through here.
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(img_ref.trim())
                .map_err(|e| anyhow!("image: raw base64 decode: {e}"))?;
            (bytes, "image/png")
        };
        let image_path = save_generated_image_bytes(&bytes, mime).await?;

        let revised = resp_body
            .pointer("/data/0/revised_prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        Ok(json!({
            "image_path": image_path,
            "mime": mime,
            "revised_prompt": revised,
            "size": size,
            "model": image_model
        }))
    }
}
