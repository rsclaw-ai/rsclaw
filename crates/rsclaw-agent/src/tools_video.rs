//! Video generation tool — `tool_video` dispatcher for Seedance / MiniMax /
//! Kling.
//!
//! All providers are async HTTP APIs: this file only resolves config and
//! credentials, calls the matching `submit_*` in `rsclaw_jobs`, and
//! persists an `ExternalJob`. The gateway worker handles polling, download,
//! and channel delivery.
//!
//! Split from `tools_misc.rs` for maintainability. Methods live in
//! `impl AgentRuntime` via the split-impl pattern.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

impl super::runtime::AgentRuntime {
    /// Generate a video from a text prompt.
    ///
    /// Supports Seedance (ByteDance ARK), MiniMax (Hailuo), and Kling
    /// (Kuaishou). Returns immediately after submit; the artifact is
    /// pushed back through the original channel when polling finishes.
    pub(crate) async fn tool_video(
        &self,
        args: Value,
        ctx: &super::runtime::RunContext,
    ) -> Result<Value> {
        let prompt = args["prompt"]
            .as_str()
            .ok_or_else(|| {
                anyhow!("video_gen: `prompt` is missing or not a string — pass the video description as a string in `prompt`")
            })?;
        let duration = args["duration"].as_u64().unwrap_or(5);
        let aspect_ratio = args["aspect_ratio"].as_str().unwrap_or("16:9");

        // Optional first-frame / reference image(s) for image-to-video.
        // Accept a single string or an array. Each entry may be a public
        // http(s) URL, an existing `data:image/...;base64,...` Data URI, or a
        // LOCAL FILE PATH — the common case, since a freshly generated image
        // has no public URL to point at. Local paths are read and encoded to
        // a base64 Data URI here so the provider gets self-contained input.
        // Only providers whose submit adapter supports it (currently agnes)
        // consume these; others ignore.
        let raw_images: Vec<String> = match &args["image"] {
            Value::String(s) if !s.is_empty() => vec![s.clone()],
            Value::Array(a) => a
                .iter()
                .filter_map(|v| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        };
        let mut images: Vec<String> = Vec::with_capacity(raw_images.len());
        for img in raw_images {
            if img.starts_with("http://")
                || img.starts_with("https://")
                || img.starts_with("data:")
            {
                images.push(img);
                continue;
            }
            // Treat as a local file path → base64 Data URI.
            match tokio::fs::read(&img).await {
                Ok(bytes) => {
                    use base64::Engine;
                    let mime = match std::path::Path::new(&img)
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.to_ascii_lowercase())
                        .as_deref()
                    {
                        Some("jpg") | Some("jpeg") => "image/jpeg",
                        Some("webp") => "image/webp",
                        Some("gif") => "image/gif",
                        _ => "image/png",
                    };
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    images.push(format!("data:{mime};base64,{b64}"));
                }
                Err(e) => {
                    tracing::warn!(path = %img, error = %e, "video_gen: image not readable, skipping");
                }
            }
        }

        // Resolve the configured video chain (head + optional fallbacks)
        // from `agents.defaults.model.video` or the per-agent handle
        // override. StringOrVec collapses single string + array into the
        // same chain shape.
        let mut video_chain: Vec<String> = self
            .handle
            .config
            .model
            .as_ref()
            .map(|m| m.video_chain())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| {
                self.config
                    .agents
                    .defaults
                    .model
                    .as_ref()
                    .map(|m| m.video_chain())
                    .unwrap_or_default()
            })
            .into_iter()
            .map(|s| s.to_owned())
            .collect();

        // No explicit video model configured? Default to the primary LLM
        // provider's first-party video model when it has one (agnes →
        // agnes-video-v2.0, rsclaw → rsclaw-video-v1). Same opt-in-by-
        // primary-provider rule as the image tool.
        if video_chain.is_empty()
            && let Some(def) = self
                .primary_provider()
                .as_deref()
                .and_then(super::tools_image::default_video_model)
        {
            video_chain.push(def.to_owned());
        }

        // Cost gate: a single Seedance / MiniMax Hailuo / Kling clip costs
        // 0.1–1+ USD and runs minutes long. Force explicit opt-in via
        // `agents.defaults.model.video` so a casual "做个视频" never
        // quietly routes to a paid endpoint. Message is localised.
        if video_chain.is_empty() {
            return Ok(json!({
                "error": rsclaw_i18n::t("video_gen_no_model", rsclaw_i18n::default_lang())
            }));
        }

        // Allow per-call override that bypasses the chain entirely. When
        // the agent explicitly names a model in args (e.g. user said "用
        // 海螺生成"), trust it and don't iterate. Chain retry only kicks
        // in for the configured default chain — overrides are intentional
        // single shots.
        let model_hint = args["model"].as_str().map(|s| s.to_lowercase());

        // Helper: resolve API key from provider config → env var.
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

        // Map a chain entry like `"doubao/doubao-seedance-2-0-260128"` to
        // its short provider name. doubao / minimax / kling are the
        // supported set; everything else collapses to doubao (Ark's
        // unbranded Seedance default).
        fn classify_provider(model: &str) -> &'static str {
            let m = model.to_lowercase();
            if m.contains("kling") {
                "kling"
            } else if m.contains("agnes") {
                "agnes"
            } else if m.contains("minimax") || m.contains("hailuo") {
                "minimax"
            } else if m.starts_with("rsclaw/") || m.contains("rsclaw-video") || m == "rsclaw" {
                "rsclaw"
            } else {
                "doubao"
            }
        }

        let ua = self
            .config
            .gateway
            .user_agent
            .as_deref()
            .unwrap_or(rsclaw_provider::DEFAULT_USER_AGENT);
        let client = reqwest::Client::builder()
            .user_agent(ua)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default();

        // Build the effective ordered list to try. `args["model"]`
        // override goes first (always exactly one attempt); otherwise
        // walk the configured chain in order.
        let attempt_models: Vec<String> = if let Some(hint) = &model_hint {
            vec![hint.clone()]
        } else {
            video_chain.clone()
        };

        let prompt_preview: String = prompt.chars().take(80).collect();

        // Eagerly register health entries for every chain candidate so
        // `/api/v1/models/health` sees the full chain even on first-call
        // success — `record_success` is a no-op when the entry doesn't
        // exist yet.
        self.model_health.ensure(&attempt_models);

        // ── Submit-only chain retry ──────────────────────────────────────
        // For each model in `attempt_models`:
        //   1. Skip if the shared health table has marked it Disabled or
        //      Cooling-not-expired (e.g. a previous tool_video call hit
        //      "AccountOverdueError" on doubao — don't burn another submit
        //      attempt until the operator resets).
        //   2. POST the submit request. On success → record_success +
        //      break (provider has billed; polling stays on this provider
        //      to avoid double-billing the user, even on poll-side
        //      hiccups).
        //   3. On submit failure → classify + record_failure in the
        //      shared health table (Balance / Auth / etc. transitions),
        //      advance to next model.
        let mut last_error: Option<anyhow::Error> = None;
        let mut chosen: Option<(&'static str, String, String)> = None;
        for model_id in &attempt_models {
            // Chain-level health gate. Single-model configs (chain.len()==1)
            // always pass when the table is pristine — back-compat path.
            if !self.model_health.is_callable(model_id) {
                tracing::info!(
                    model = %model_id,
                    "tool_video: skipping (model marked Disabled or Cooling)"
                );
                continue;
            }

            let provider = classify_provider(model_id);
            tracing::info!(
                model = %model_id,
                provider,
                prompt = prompt_preview,
                duration,
                aspect_ratio,
                "tool_video: submitting"
            );

            let submit_result: Result<(&'static str, String)> = match provider {
                "doubao" => match resolve_key("doubao", "ARK_API_KEY") {
                    Some(key) => rsclaw_jobs::submit_seedance(
                        &client,
                        &key,
                        prompt,
                        duration,
                        aspect_ratio,
                        Some(model_id.as_str()),
                    )
                    .await
                    .map(|id| ("seedance", id)),
                    None => Err(anyhow!(
                        "video_gen: no API key for doubao/Seedance. Set `model.models.providers.doubao.apiKey` in rsclaw.json5 or export ARK_API_KEY, then retry — or tell the user the doubao key is missing."
                    )),
                },
                "minimax" => match resolve_key("minimax", "MINIMAX_API_KEY") {
                    Some(key) => rsclaw_jobs::submit_minimax(
                        &client,
                        &key,
                        prompt,
                        duration,
                        aspect_ratio,
                        Some(model_id.as_str()),
                    )
                    .await
                    .map(|id| ("minimax", id)),
                    None => Err(anyhow!(
                        "video_gen: no API key for MiniMax. Set `model.models.providers.minimax.apiKey` in rsclaw.json5 or export MINIMAX_API_KEY, then retry — or tell the user the MiniMax key is missing."
                    )),
                },
                "kling" => {
                    let ak = resolve_key("kling", "KLING_ACCESS_KEY");
                    let sk = self
                        .config
                        .model
                        .models
                        .as_ref()
                        .and_then(|m| m.providers.get("kling"))
                        .and_then(|p| {
                            p.api_key
                                .as_ref()
                                .and_then(|k| k.as_plain().map(str::to_owned))
                        })
                        .or_else(|| std::env::var("KLING_SECRET_KEY").ok());
                    match (ak, sk) {
                        (Some(access), Some(secret)) => {
                            rsclaw_jobs::submit_kling(
                                &client,
                                &access,
                                &secret,
                                prompt,
                                duration,
                                aspect_ratio,
                                Some(model_id.as_str()),
                            )
                            .await
                            .map(|id| ("kling", id))
                        }
                        _ => Err(anyhow!(
                            "video_gen: KLING_ACCESS_KEY + KLING_SECRET_KEY required"
                        )),
                    }
                }
                "agnes" => match resolve_key("agnes", "AGNES_API_KEY") {
                    Some(key) => rsclaw_jobs::submit_agnes(
                        &client,
                        &key,
                        prompt,
                        duration,
                        aspect_ratio,
                        Some(model_id.as_str()),
                        &images,
                    )
                    .await
                    .map(|id| ("agnes", id)),
                    None => Err(anyhow!(
                        "video_gen: no API key for Agnes. Set `model.models.providers.agnes.apiKey` in rsclaw.json5 or export AGNES_API_KEY, then retry — or tell the user the Agnes key is missing."
                    )),
                },
                "rsclaw" => match resolve_key("rsclaw", "RSCLAW_API_KEY") {
                    Some(key) => submit_rsclaw_video(
                        &client,
                        &key,
                        prompt,
                        duration,
                        aspect_ratio,
                        Some(model_id.as_str()),
                    )
                    .await
                    .map(|id| ("rsclaw", id)),
                    None => Err(anyhow!(
                        "video_gen: no API key for rsclaw. Set `model.models.providers.rsclaw.apiKey` in rsclaw.json5 or export RSCLAW_API_KEY, then retry — or tell the user the rsclaw key is missing."
                    )),
                },
                other => Err(anyhow!("video_gen: unsupported provider {other}")),
            };

            match submit_result {
                Ok((provider_key, task_id)) => {
                    self.model_health.record_success(model_id);
                    tracing::info!(
                        model = %model_id,
                        provider = provider_key,
                        task_id,
                        "tool_video: task submitted — polling stays on this provider"
                    );
                    chosen = Some((provider_key, task_id, model_id.clone()));
                    break;
                }
                Err(e) => {
                    let kind = rsclaw_provider::health::classify_error(&e);
                    let body = format!("{e:#}");
                    let truncated = rsclaw_util::truncate_str(&body, 200).to_owned();
                    self.model_health.ensure(&[model_id.clone()]);
                    self.model_health.record_failure(model_id, kind.clone(), truncated);
                    tracing::warn!(
                        model = %model_id,
                        provider,
                        kind = ?kind,
                        error = %e,
                        "tool_video: submit failed — advancing chain"
                    );
                    last_error = Some(e);
                    continue;
                }
            }
        }

        let (provider_key, task_id, _winning_model) = match chosen {
            Some(c) => c,
            None => {
                return Err(match last_error {
                    Some(e) => anyhow!(
                        "video_gen: all {} model(s) failed at submit. Last error: {e:#}",
                        attempt_models.len(),
                    ),
                    // Every candidate was skipped by the health gate — no
                    // submit was attempted at all, so don't claim a
                    // submit failure.
                    None => anyhow!(
                        "video_gen: no submit attempted — all {} candidate model(s) are marked Disabled/Cooling in the model health table from earlier failures (e.g. auth/balance errors). Check GET /api/v1/models/health, fix the provider key/balance or wait for cooldown, then retry.",
                        attempt_models.len(),
                    ),
                });
            }
        };

        let job = rsclaw_types::ExternalJob::new_submitted(
            ctx.session_key.clone(),
            rsclaw_types::ExternalJobDelivery {
                channel: ctx.channel.clone(),
                target_id: if ctx.chat_id.is_empty() {
                    ctx.peer_id.clone()
                } else {
                    ctx.chat_id.clone()
                },
                is_group: !ctx.chat_id.is_empty() && ctx.chat_id != ctx.peer_id,
                reply_to: None,
            },
            rsclaw_types::ExternalJobOrigin::Agent,
            provider_key,
            &task_id,
            rsclaw_types::ExternalJobKind::VideoGen,
            prompt,
        );
        let job_id = job.id.clone();
        self.store
            .db
            .enqueue_external_job(&job)
            .map_err(|e| anyhow!("video_gen: enqueue external job: {e}"))?;

        Ok(json!({
            "status": "submitted",
            "provider": provider_key,
            "task_id": task_id,
            "job_id": job_id,
            "message": "Video generation submitted. The finished video will be delivered automatically when ready (typically 30s–5min). The user has been informed; do NOT poll or wait — your turn is complete."
        }))
    }
}

/// Submit a rsclaw text→video task and return the rsclaw `video_<id>`.
///
/// Hits `POST https://api.rsclaw.ai/v1/videos` with the OAI Sora-2 +
/// Seedance superset body. Polling is handled by
/// `rsclaw_jobs::poll_rsclaw` after the caller enqueues the corresponding
/// `ExternalJob{ provider: "rsclaw" }`.
///
/// 307/308 redirects from the LB are followed by
/// `rsclaw_provider::rsclaw_http::post_json` — same protocol as the
/// LLM-side `src/provider/rsclaw.rs::send_following_redirects` (Bearer
/// re-attached on each cross-origin hop, max 5 hops).
async fn submit_rsclaw_video(
    _client: &reqwest::Client,
    api_key: &str,
    prompt: &str,
    duration: u64,
    aspect_ratio: &str,
    model_hint: Option<&str>,
) -> Result<String> {
    // Chain entries may arrive prefixed (`rsclaw/rsclaw-video-v1`); strip
    // the `provider/` segment so the upstream `model` field is the bare id.
    let model = model_hint
        .map(|m| m.rsplit('/').next().unwrap_or(m))
        .filter(|m| !m.is_empty() && *m != "rsclaw")
        .unwrap_or("rsclaw-video-v1");
    let body = json!({
        "model": model,
        "prompt": prompt,
        "resolution": "720p",
        "ratio": aspect_ratio,
        "duration": duration,
    });

    let url = format!(
        "{}/v1/videos",
        rsclaw_provider::rsclaw_http::gen_host_base(None)
    );
    let redirect_client =
        rsclaw_provider::rsclaw_http::build_client(rsclaw_provider::DEFAULT_USER_AGENT, 30)?;
    let resp = rsclaw_provider::rsclaw_http::post_json(&redirect_client, &url, api_key, &body)
        .await?;
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| anyhow!("video_gen: rsclaw read body: {e}"))?;

    if !status.is_success() {
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        // Non-JSON bodies (HTML/text LB error pages on 5xx) are the
        // actual diagnostic — surface a truncated raw snippet instead of
        // a useless "unknown error" literal.
        let raw = String::from_utf8_lossy(&bytes);
        let msg = v
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .or_else(|| v.get("message").and_then(|v| v.as_str()))
            .unwrap_or_else(|| rsclaw_util::truncate_str(&raw, 200));
        return Err(anyhow!("video_gen: rsclaw API {status}: {msg}"));
    }
    let v: Value = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow!("video_gen: rsclaw parse response: {e}"))?;
    let id = v
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("video_gen: rsclaw no `id` in response: {v}"))?
        .to_owned();
    Ok(id)
}
