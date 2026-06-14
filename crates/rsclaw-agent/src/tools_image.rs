//! Image generation tool — `tool_image` plus per-provider HTTP code
//! (Doubao / OpenAI / Qwen / MiniMax / Gemini).
//!
//! Split from `tools_misc.rs` for maintainability. Methods live in
//! `impl AgentRuntime` via the split-impl pattern (same struct,
//! different file).

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

/// Marker error: a 2xx came back but we couldn't extract the artifact
/// (parse failed, no image data, download of the returned URL failed,
/// base64 didn't decode). Per the cost-gate contract, the provider has
/// already been billed for this attempt, so the chain MUST NOT advance
/// to another provider — that would double-bill the user. The outer
/// `tool_image` loop downcasts to detect this and bails immediately.
#[derive(Debug)]
struct PostBillingError(String);

impl std::fmt::Display for PostBillingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "image: post-billing failure: {}", self.0)
    }
}

impl std::error::Error for PostBillingError {}

fn post_billing<T: Into<String>>(msg: T) -> anyhow::Error {
    anyhow::Error::new(PostBillingError(msg.into()))
}

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
    let kind = rsclaw_channel::kind_from_extension(ext);
    let category = rsclaw_channel::category_for_kind(kind);
    let save_dir = dirs_next::download_dir()
        .unwrap_or_else(|| {
            dirs_next::home_dir()
                .unwrap_or_else(rsclaw_config::loader::base_dir)
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

/// Default image-gen model for a primary LLM provider, used only when the
/// user hasn't configured `agents.defaults.model.image` explicitly. Keeps
/// the "pick a sane gen model that matches my chat provider" behaviour
/// scoped to providers that actually ship a first-party image model.
pub(crate) fn default_image_model(provider: &str) -> Option<&'static str> {
    match provider {
        "agnes" => Some("agnes/agnes-image-2.1-flash"),
        "rsclaw" => Some("rsclaw/rsclaw-image-v1"),
        _ => None,
    }
}

/// Snap a requested image size to one Agnes Image 2.0/2.1 actually
/// supports (1024x1024 / 1024x768 / 768x1024). Anything larger (e.g. the
/// model loves to ask for 2048x2048) hangs the upstream until the client
/// times out, so we always clamp by aspect ratio rather than trust the
/// caller's `size`.
pub(crate) fn agnes_image_size(requested: &str) -> &'static str {
    let (w, h) = requested
        .split_once('x')
        .and_then(|(a, b)| Some((a.trim().parse::<f32>().ok()?, b.trim().parse::<f32>().ok()?)))
        .unwrap_or((1024.0, 1024.0));
    let ratio = w / h.max(1.0);
    if ratio > 1.15 {
        "1024x768"
    } else if ratio < 0.87 {
        "768x1024"
    } else {
        "1024x1024"
    }
}

/// Default video-gen model for a primary LLM provider — same opt-in-by-
/// primary-provider rule as [`default_image_model`].
pub(crate) fn default_video_model(provider: &str) -> Option<&'static str> {
    match provider {
        "agnes" => Some("agnes/agnes-video-v2.0"),
        "rsclaw" => Some("rsclaw/rsclaw-video-v1"),
        _ => None,
    }
}

impl super::runtime::AgentRuntime {
    /// Resolve the primary LLM provider name (e.g. `"agnes"`, `"rsclaw"`)
    /// from the per-agent handle override, falling back to
    /// `agents.defaults.model.primary`. Used to pick a default image/video
    /// gen model when the user left those chains empty.
    pub(crate) fn primary_provider(&self) -> Option<String> {
        let head = self
            .handle
            .config
            .model
            .as_ref()
            .and_then(|m| m.primary_head())
            .or_else(|| {
                self.config
                    .agents
                    .defaults
                    .model
                    .as_ref()
                    .and_then(|m| m.primary_head())
            })?;
        Some(
            rsclaw_provider::registry::ProviderRegistry::parse_model(head)
                .0
                .to_owned(),
        )
    }

    pub(crate) async fn tool_image(&self, args: Value) -> Result<Value> {
        let prompt = args["prompt"]
            .as_str()
            .ok_or_else(|| anyhow!("image: `prompt` required"))?;

        // Resolve configured image chain (head + optional fallbacks). Image
        // generation is paid per call, but unlike video there's no polling
        // phase — the response is the artifact. So retry semantics are
        // simpler: pre-2xx failures (network, 4xx, 5xx) haven't been
        // billed, safe to advance the chain. A 2xx that fails to parse
        // means the provider charged but didn't deliver — surface the
        // error rather than double-bill (matches video's submit-only
        // rule for the same reason).
        let mut image_chain: Vec<String> = self
            .handle
            .config
            .model
            .as_ref()
            .map(|m| m.image_chain())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| {
                self.config
                    .agents
                    .defaults
                    .model
                    .as_ref()
                    .map(|m| m.image_chain())
                    .unwrap_or_default()
            })
            .into_iter()
            .map(|s| s.to_owned())
            .collect();

        // No explicit image model configured? If the primary LLM provider
        // ships a first-party image model (agnes → agnes-image-2.1-flash,
        // rsclaw → rsclaw-image-v1), default to it instead of erroring.
        // The primary provider's key is already configured, so this stays
        // within the user's chosen vendor — no surprise cross-billing.
        if image_chain.is_empty()
            && let Some(def) = self.primary_provider().as_deref().and_then(default_image_model)
        {
            image_chain.push(def.to_owned());
        }

        // Cost gate: image generation hits a paid third-party API on every
        // call (doubao seedream / dall-e / qwen / minimax / gemini imagen).
        // Refuse to auto-fall back — the user must opt in by setting
        // `agents.defaults.model.image`. Message is localised — surfaced
        // through the channel directly to the end user.
        if image_chain.is_empty() {
            return Ok(json!({
                "error": rsclaw_i18n::t("image_gen_no_model", rsclaw_i18n::default_lang())
            }));
        }

        // args["model"] override → exactly one attempt (no chain retry —
        // explicit user intent). An explicit model is also FORCED: it
        // bypasses the health breaker (Cooling/Disabled), matching the
        // "pass an explicit `model` argument to force one attempt" hint we
        // surface when the whole chain is cooling.
        let explicit_model = args
            .get("model")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let forced = explicit_model.is_some();
        let attempt_models: Vec<String> = match explicit_model {
            Some(m) => vec![m],
            None => image_chain.clone(),
        };

        // ── Pre-submit chain retry ──────────────────────────────────────
        // Eagerly register health entries for every chain candidate so
        // `/api/v1/models/health` (and the UI dots) see the full chain
        // even on a first-call success — `record_success` is a no-op
        // when the entry doesn't exist yet.
        self.model_health.ensure(&attempt_models);
        let mut last_error: Option<anyhow::Error> = None;
        for chain_model in &attempt_models {
            if !forced && !self.model_health.is_callable(chain_model) {
                tracing::info!(
                    model = %chain_model,
                    "tool_image: skipping (model cooling; pass explicit `model` to force)"
                );
                continue;
            }
            match self.try_image_for_model(prompt, chain_model, &args).await {
                Ok(v) => {
                    self.model_health.record_success(chain_model);
                    return Ok(v);
                }
                Err(e) => {
                    // Cost gate: the provider already charged for this
                    // attempt (generation succeeded; the failure is in the
                    // download / post-processing leg). The MODEL is healthy
                    // — do NOT record a health failure (that would wrongly
                    // cool a working model). Surface to the user instead of
                    // double-billing through the next chain entry.
                    if e.downcast_ref::<PostBillingError>().is_some() {
                        tracing::warn!(
                            model = %chain_model,
                            error = %e,
                            "tool_image: post-billing failure — NOT advancing chain, not recording health"
                        );
                        return Err(anyhow!(
                            "image_gen: provider {chain_model} was billed but did not return a usable image: {e:#}. Do NOT retry this tool automatically (each attempt is billed) — report the failure to the user and let them decide whether to retry"
                        ));
                    }
                    // Pre-billing failure: classify + cool the model, then
                    // advance the chain.
                    let kind = rsclaw_provider::health::classify_error(&e);
                    let body = format!("{e:#}");
                    let truncated = rsclaw_util::truncate_str(&body, 200).to_owned();
                    self.model_health.record_failure(chain_model, kind.clone(), truncated);
                    tracing::warn!(
                        model = %chain_model,
                        kind = ?kind,
                        error = %e,
                        "tool_image: failed — advancing chain"
                    );
                    last_error = Some(e);
                    continue;
                }
            }
        }

        Err(match last_error {
            Some(e) => anyhow!(
                "image_gen: all {} model(s) failed. Last error: {e:#}",
                attempt_models.len()
            ),
            // last_error == None means every chain entry was skipped as
            // Disabled/Cooling — nothing was actually attempted, so
            // "all failed" would misdiagnose.
            None => anyhow!(
                "image_gen: all {} configured image model(s) are currently cooling down from recent failures — none were attempted. The cooldown is time-bounded and clears on its own; check /api/v1/models/health, wait for it to expire, or pass an explicit `model` argument to force one attempt now",
                attempt_models.len()
            ),
        })
    }

    /// One attempt for a single configured image model id. Called per
    /// chain entry by `tool_image`. Returns the same JSON shape the
    /// caller-visible tool result expects (image_path / mime /
    /// revised_prompt). All HTTP and parse errors bubble up as `Err` so
    /// `tool_image` can classify them and decide whether to advance the
    /// chain or surface to the user.
    async fn try_image_for_model(
        &self,
        prompt: &str,
        user_model: &str,
        args: &Value,
    ) -> Result<Value> {
        let resolve_model = user_model.to_owned();
        let (prov_name, user_model_id) =
            { rsclaw_provider::registry::ProviderRegistry::parse_model(&resolve_model) };
        let (base_url, _auth_style) = rsclaw_provider::defaults::resolve_base_url(prov_name);

        let default_size = match prov_name {
            // Agnes Image 2.0/2.1 only support up to 1024-tier sizes
            // (1024x1024 / 1024x768 / 768x1024); a 2048x2048 request hangs
            // server-side until the client times out.
            "agnes" => "1024x1024",
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

        // Providers with image generation support. The explicit image model is
        // the cost gate: do not silently fall back to another paid provider
        // just because an API key happens to be configured.
        let image_providers = ["doubao", "bytedance", "openai", "qwen", "minimax", "gemini", "rsclaw", "agnes"];
        let (img_url, img_key, img_prov, img_env_var) = if image_providers.contains(&prov_name) {
            // rsclaw's LLM default in defaults.toml ends in `/v1/agent`;
            // the gen surface lives off the host root. `gen_host_base`
            // normalises both shapes; we append `/v1` for the OAI mount.
            let raw = cfg_url.unwrap_or(base_url);
            let url = if prov_name == "rsclaw" {
                format!(
                    "{}/v1",
                    rsclaw_provider::rsclaw_http::gen_host_base(Some(&raw))
                )
            } else {
                raw
            };
            let env_var = match prov_name {
                "doubao" | "bytedance" => Some("ARK_API_KEY"),
                "qwen" => Some("DASHSCOPE_API_KEY"),
                "minimax" => Some("MINIMAX_API_KEY"),
                "gemini" => Some("GEMINI_API_KEY"),
                "openai" => Some("OPENAI_API_KEY"),
                "rsclaw" => Some("RSCLAW_API_KEY"),
                "agnes" => Some("AGNES_API_KEY"),
                _ => None,
            };
            let env_key = env_var.and_then(|v| std::env::var(v).ok());
            let key = cfg_key.or(env_key);
            (url, key, prov_name, env_var)
        } else {
            return Ok(json!({
                "error": format!(
                    "Configured image model provider `{prov_name}` does not support image generation. Configure agents.defaults.model.image with one of: doubao, qwen, minimax, gemini, openai, rsclaw."
                )
            }));
        };
        let Some(api_key) = img_key else {
            // Return as Err so the outer chain loop classifies + advances
            // to the next configured image model. Without this, a
            // missing-key entry would short-circuit as Ok and silently
            // skip the user's configured fallbacks.
            return Err(anyhow!(
                "image_gen: no API key for {img_prov} — set models.providers.{img_prov}.api_key in config{}, or configure a working fallback in agents.defaults.model.image",
                img_env_var
                    .map(|v| format!(" or export {v}"))
                    .unwrap_or_default()
            ));
        };

        let image_model = args["model"]
            .as_str()
            .or_else(|| {
                if !user_model_id.is_empty() {
                    Some(user_model_id)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| match img_prov {
                "doubao" | "bytedance" => "doubao-seedream-5-0-260128",
                "openai" => "gpt-image-2",
                "qwen" => "qwen-image-2.0-pro",
                "minimax" => "image-01",
                "gemini" => "gemini-3-pro-image-preview",
                "rsclaw" => "rsclaw-image-v1",
                "agnes" => "agnes-image-2.1-flash",
                _ => "gpt-image-2",
            });

        // Resolve User-Agent: provider config -> gateway config -> default
        let img_ua = self
            .config
            .model
            .models
            .as_ref()
            .and_then(|m| m.providers.get(img_prov))
            .and_then(|p| p.user_agent.as_deref())
            .or_else(|| self.config.gateway.user_agent.as_deref())
            .unwrap_or(rsclaw_provider::DEFAULT_USER_AGENT);
        let client = reqwest::Client::builder()
            .user_agent(img_ua)
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .unwrap_or_default();

        tracing::info!(
            provider = img_prov,
            model = image_model,
            size = size,
            ua = img_ua,
            "tool_image: generating"
        );

        // Provider-specific API formats
        let is_qwen = img_prov == "qwen";
        let is_minimax = img_prov == "minimax";
        let is_gemini = img_prov == "gemini";
        let is_agnes = img_prov == "agnes";
        // Read response as raw bytes first, then parse — splitting the
        // two lets us tell "provider rejected before billing" (non-2xx,
        // parse-can-be-anything) from "provider charged but didn't
        // deliver" (2xx with garbled or empty body). The post-billing
        // case becomes `PostBillingError`, which the chain loop sees
        // and refuses to advance on.
        fn parse_response_body(
            status: reqwest::StatusCode,
            bytes: &[u8],
        ) -> Result<Value, anyhow::Error> {
            if status.is_success() {
                serde_json::from_slice::<Value>(bytes).map_err(|e| {
                    let preview: String = String::from_utf8_lossy(bytes).chars().take(200).collect();
                    post_billing(format!("parse error: {e} (body preview: {preview})"))
                })
            } else {
                // Non-2xx with a non-JSON body (HTML error page, plain
                // text from a proxy, …): keep the raw text so the error
                // site can show a preview instead of "unknown error".
                Ok(serde_json::from_slice::<Value>(bytes).unwrap_or_else(|_| {
                    Value::String(String::from_utf8_lossy(bytes).into_owned())
                }))
            }
        }
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
            let body_bytes = resp
                .bytes()
                .await
                .map_err(|e| anyhow!("image: read body: {e}"))?;
            let body = parse_response_body(st, &body_bytes)?;
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
            let body_bytes = resp
                .bytes()
                .await
                .map_err(|e| anyhow!("image: read body: {e}"))?;
            let body = parse_response_body(st, &body_bytes)?;
            (st, body)
        } else if is_gemini {
            // Gemini: generateContent with responseModalities: ["IMAGE"]
            // Map size to aspect ratio for Gemini
            let aspect = if size.contains('x') {
                let parts: Vec<&str> = size.split('x').collect();
                if parts.len() == 2 {
                    let w = parts[0].parse::<u32>().unwrap_or(2048);
                    let h = parts[1].parse::<u32>().unwrap_or(2048);
                    if w == h {
                        "1:1"
                    } else if w > h {
                        "16:9"
                    } else {
                        "9:16"
                    }
                } else {
                    "1:1"
                }
            } else {
                "1:1"
            };
            let gemini_base = img_url.trim_end_matches('/');
            let url = format!("{gemini_base}/models/{image_model}:generateContent?key={api_key}");
            let resp = client
                .post(&url)
                .json(&json!({
                    "contents": [{ "parts": [{ "text": prompt }] }],
                    "generationConfig": {
                        "responseModalities": ["TEXT", "IMAGE"],
                        "imageConfig": { "aspectRatio": aspect }
                    }
                }))
                .send()
                .await
                .map_err(|e| anyhow!("image: gemini request failed: {e}"))?;
            let st = resp.status();
            let body_bytes = resp
                .bytes()
                .await
                .map_err(|e| anyhow!("image: gemini read body: {e}"))?;
            let body = parse_response_body(st, &body_bytes)?;
            (st, body)
        } else if is_agnes {
            // Agnes Image 2.0/2.1 Flash: OAI-shaped /v1/images/generations,
            // but with two quirks from the official docs:
            //   1. `response_format` MUST live under `extra_body` — placing
            //      it at the top level returns a 400.
            //   2. image-to-image input goes in `extra_body.image` (array of
            //      public URLs or `data:image/...;base64,...` Data URIs); no
            //      `tags: ["img2img"]` needed.
            // Output URL is at data[0].url (same parser as the OAI block).
            let url = format!("{}/images/generations", img_url.trim_end_matches('/'));
            let mut extra = json!({ "response_format": "url" });
            // Pass through optional input images for image-to-image / multi-image
            // composition when the caller supplies them.
            if let Some(imgs) = args.get("image").and_then(|v| v.as_array()) {
                extra["image"] = json!(imgs);
            }
            let body = json!({
                "model": image_model,
                "prompt": prompt,
                // Clamp to a supported tier — the model often asks for
                // 2048x2048, which Agnes can't serve and hangs on.
                "size": agnes_image_size(size),
                "extra_body": extra,
            });
            let resp = client
                .post(&url)
                .header("Authorization", format!("Bearer {api_key}"))
                .json(&body)
                .send()
                .await
                .map_err(|e| anyhow!("image: agnes request failed: {e}"))?;
            let st = resp.status();
            let body_bytes = resp
                .bytes()
                .await
                .map_err(|e| anyhow!("image: agnes read body: {e}"))?;
            let body = parse_response_body(st, &body_bytes)?;
            (st, body)
        } else {
            // OAI-compat block: shared by openai (incl. gpt-image-2),
            // doubao seedream, and rsclaw (rsclaw-image-v1). All three
            // accept the OAI `/v1/images/generations` request shape.
            //
            // gpt-image-2 specifics (per developers.openai.com docs,
            // released 2026-04-21): GPT models ALWAYS return b64_json
            // regardless of `response_format`, and accept additional
            // `quality / output_format / output_compression / background
            // / moderation` controls. We pass them through verbatim when
            // the caller sets them in `args`; the existing data[0].url
            // → data[0].b64_json fallback in the response parser handles
            // either return shape transparently.
            let url = format!("{}/images/generations", img_url.trim_end_matches('/'));
            let is_gpt_image = image_model.starts_with("gpt-image");
            let mut body = json!({
                "model": image_model,
                "prompt": prompt,
                "size": size,
                "n": 1,
            });
            // gpt-image-* ignore response_format (always b64_json). Sending
            // it is harmless but cleaner to omit.
            if !is_gpt_image {
                body["response_format"] = json!("url");
            }
            // Pass-through optional gpt-image-2 / rsclaw-image-v1 fields.
            // Applied uniformly — providers that don't recognise them
            // simply ignore (we already pre-validated model→provider mapping).
            for field in ["quality", "output_format", "background", "moderation"] {
                if let Some(v) = args.get(field).and_then(|v| v.as_str()) {
                    body[field] = json!(v);
                }
            }
            if let Some(c) = args.get("output_compression").and_then(|v| v.as_u64()) {
                body["output_compression"] = json!(c);
            }

            if img_prov == "rsclaw" {
                // rsclaw LB may emit 307/308 redirecting to a backend
                // pool with a different origin. reqwest's default policy
                // strips Authorization across origins, so route through
                // the shared `rsclaw_http` helper which manages the loop
                // and re-attaches Bearer on each hop. Mirrors the LLM
                // side's `send_following_redirects`.
                let redirect_client = rsclaw_provider::rsclaw_http::build_client(img_ua, 120)
                    .map_err(|e| anyhow!("image: rsclaw client: {e}"))?;
                let resp =
                    rsclaw_provider::rsclaw_http::post_json(&redirect_client, &url, &api_key, &body)
                        .await?;
                let st = resp.status();
                let body_bytes = resp
                    .bytes()
                    .await
                    .map_err(|e| anyhow!("image: rsclaw read body: {e}"))?;
                let body = parse_response_body(st, &body_bytes)?;
                (st, body)
            } else {
                let resp = client
                    .post(&url)
                    .header("Authorization", format!("Bearer {api_key}"))
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| anyhow!("image: request failed: {e}"))?;
                let st = resp.status();
                let body_bytes = resp
                    .bytes()
                    .await
                    .map_err(|e| anyhow!("image: read body: {e}"))?;
                let body = parse_response_body(st, &body_bytes)?;
                (st, body)
            }
        };

        if !resp_status.is_success() {
            let raw = resp_body.to_string();
            let err_msg = resp_body["error"]["message"]
                .as_str()
                .or_else(|| resp_body["message"].as_str())
                // Non-JSON bodies are preserved as Value::String by
                // parse_response_body; fall back to a raw preview so
                // 401/429/400 are distinguishable.
                .or_else(|| resp_body.as_str().map(|s| rsclaw_util::truncate_str(s, 200)))
                .unwrap_or_else(|| rsclaw_util::truncate_str(&raw, 200));
            return Err(anyhow!("image: API error (HTTP {resp_status}): {err_msg}"));
        }

        // Extract image URL/base64 — different response formats per provider
        // Gemini returns inline base64 directly, others return URLs
        if is_gemini {
            // Gemini: candidates[0].content.parts[] — find the inlineData part
            #[allow(unused_imports)]
            use base64::Engine;
            let parts = resp_body
                .pointer("/candidates/0/content/parts")
                .and_then(|v| v.as_array());
            if let Some(parts) = parts {
                for part in parts {
                    if let Some(inline) = part.get("inlineData") {
                        let mime = inline
                            .get("mimeType")
                            .and_then(|v| v.as_str())
                            .unwrap_or("image/png");
                        if let Some(b64_data) = inline.get("data").and_then(|v| v.as_str()) {
                            let bytes = base64::engine::general_purpose::STANDARD
                                .decode(b64_data)
                                .map_err(|e| post_billing(format!("gemini base64 decode: {e}")))?;
                            let path = save_generated_image_bytes(&bytes, mime)
                                .await
                                .map_err(|e| post_billing(format!("save: {e}")))?;
                            return Ok(json!({
                                "image_path": path,
                                "mime": mime,
                                "revised_prompt": prompt
                            }));
                        }
                    }
                }
            }
            // A 200 with no inlineData is most often a safety block —
            // surface finishReason / blockReason and any text part so the
            // model can tell a refusal from API format drift.
            let finish_reason = resp_body
                .pointer("/candidates/0/finishReason")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    resp_body
                        .pointer("/promptFeedback/blockReason")
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("unknown");
            let refusal_text = parts
                .and_then(|ps| ps.iter().find_map(|p| p.get("text").and_then(|v| v.as_str())))
                .unwrap_or("");
            return Err(post_billing(format!(
                "no image data in Gemini response (finishReason: {finish_reason}, text: {}) — likely safety-filtered; rephrase the prompt to avoid policy-sensitive content",
                rsclaw_util::truncate_str(refusal_text, 200)
            )));
        }

        // Each provider may return either a fetchable URL or inline base64.
        // We normalise both into raw bytes + mime, then save to disk.
        let img_ref = if is_qwen {
            resp_body
                .pointer("/output/choices/0/message/content/0/image")
                .and_then(|v| v.as_str())
        } else if is_minimax {
            // minimax: data.image_urls[0] (url) or data.image_base64[0] (base64)
            resp_body
                .pointer("/data/image_urls/0")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    resp_body
                        .pointer("/data/image_base64/0")
                        .and_then(|v| v.as_str())
                })
        } else {
            // OpenAI/Doubao/etc: prefer url, fall back to b64_json (b64_json is what
            // OpenAI's response_format=b64_json returns, and some compatible
            // providers return it even when url is requested).
            resp_body
                .pointer("/data/0/url")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    resp_body
                        .pointer("/data/0/b64_json")
                        .and_then(|v| v.as_str())
                })
        };

        let Some(img_ref) = img_ref else {
            return Err(post_billing(format!(
                "no image data in response (no url/base64 image field; body preview: {})",
                rsclaw_util::truncate_str(&resp_body.to_string(), 200)
            )));
        };

        // Resolve `img_ref` → bytes + mime.  Three shapes are accepted:
        //   * `data:image/...;base64,<b64>`   inline data URL (Gemini-style)
        //   * `http(s)://...`                  download via reqwest
        //   * `<raw base64>`                   minimax `image_base64`, OpenAI
        //     `b64_json`
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
                .map_err(|e| post_billing(format!("base64 decode: {e}")))?;
            (bytes, mime_static)
        } else if img_ref.starts_with("http://") || img_ref.starts_with("https://") {
            let resp = reqwest::Client::new()
                .get(img_ref)
                .timeout(std::time::Duration::from_secs(60))
                .send()
                .await
                .map_err(|e| post_billing(format!("download error: {e}")))?;
            if !resp.status().is_success() {
                return Err(post_billing(format!("download returned {}", resp.status())));
            }
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| post_billing(format!("download failed: {e}")))?
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
                .map_err(|e| post_billing(format!("raw base64 decode: {e}")))?;
            (bytes, "image/png")
        };
        let image_path = save_generated_image_bytes(&bytes, mime)
            .await
            .map_err(|e| post_billing(format!("save: {e}")))?;

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
