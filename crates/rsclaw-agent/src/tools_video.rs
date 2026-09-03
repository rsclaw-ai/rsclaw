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

const RSCLAW_NATIVE_VIDEO_TIMEOUT_SECS: i64 = 60 * 60;

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
        tool_call_id: &str,
    ) -> Result<Value> {
        let prompt = args["prompt"]
            .as_str()
            .ok_or_else(|| {
                anyhow!("video_gen: `prompt` is missing or not a string — pass the video description as a string in `prompt`")
            })?;
        let duration = args["duration"].as_u64().unwrap_or(5);
        let aspect_ratio = args["aspect_ratio"].as_str().unwrap_or("16:9");
        let resolution = video_resolution(args.get("resolution"))?;
        let generate_audio = match args.get("generate_audio") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_bool().ok_or_else(|| {
                anyhow!("video_gen: `generate_audio` must be a boolean when provided")
            })?),
        };

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
            if img.starts_with("http://") || img.starts_with("https://") || img.starts_with("data:")
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

        // Optional driving video for v2v. Standard rsclaw video always uses
        // rsclaw-video-v3 and sends this as a typed structure reference. Local
        // path / data-URI / http URL are accepted; local files are normalized
        // to a data URI like reference images.
        let video_assets = normalize_gen_assets(&args["video"]).await;
        let video_ref = video_assets.first().map(|s| s.as_str());

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
        // agnes-video-v2.0, rsclaw → rsclaw-video-v3). Same opt-in-by-
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
        // its short provider name. The supported set is intentionally narrow
        // — doubao (Seedance, 强), agnes (免费), rsclaw (自家 gen surface).
        // Everything else (kling/minimax/…) is no longer routed from core:
        // those upstreams live behind the rsclaw gen aggregator or a skill.
        fn classify_provider(model: &str) -> &'static str {
            let m = model.to_lowercase();
            if m.contains("agnes") {
                "agnes"
            } else if m.starts_with("rsclaw/") || m.contains("rsclaw-video") || m == "rsclaw" {
                "rsclaw"
            } else if m.starts_with("openai/") || m.contains("sora") || m == "openai" {
                "openai"
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
        //      "AccountOverdueError" on doubao — don't burn another submit attempt
        //      until the operator resets).
        //   2. POST the submit request. On success → record_success + break (provider
        //      has billed; polling stays on this provider to avoid double-billing the
        //      user, even on poll-side hiccups).
        //   3. On submit failure → classify + record_failure in the shared health table
        //      (Balance / Auth / etc. transitions), advance to next model.
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
                        &images,
                    )
                    .await
                    .map(|id| ("seedance", id)),
                    None => Err(anyhow!(
                        "video_gen: no API key for doubao/Seedance. Set `model.models.providers.doubao.apiKey` in rsclaw.json5 or export ARK_API_KEY, then retry — or tell the user the doubao key is missing."
                    )),
                },
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
                "openai" => match resolve_key("openai", "OPENAI_API_KEY") {
                    Some(key) => {
                        // baseUrl passthrough: provider config base_url wins,
                        // else the builtin default (https://api.openai.com/v1).
                        let base = self
                            .config
                            .model
                            .models
                            .as_ref()
                            .and_then(|m| m.providers.get("openai"))
                            .and_then(|p| p.base_url.clone())
                            .unwrap_or_else(|| {
                                rsclaw_provider::defaults::resolve_base_url("openai").0
                            });
                        rsclaw_jobs::submit_openai_video(
                            &client,
                            &base,
                            &key,
                            prompt,
                            duration,
                            aspect_ratio,
                            Some(model_id.as_str()),
                            &images,
                        )
                        .await
                        .map(|id| ("openai", id))
                    }
                    None => Err(anyhow!(
                        "video_gen: no API key for OpenAI. Set `model.models.providers.openai.apiKey` in rsclaw.json5 or export OPENAI_API_KEY, then retry — or tell the user the OpenAI key is missing."
                    )),
                },
                "rsclaw" => match resolve_key("rsclaw", "RSCLAW_API_KEY") {
                    Some(key) => submit_rsclaw_video(
                        &key,
                        prompt,
                        duration,
                        resolution,
                        aspect_ratio,
                        generate_audio,
                        Some(model_id.as_str()),
                        &images,
                        video_ref,
                        &format!("rsclaw-video-{tool_call_id}"),
                    )
                    .await
                    .map(|id| ("rsclaw_native", id)),
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
                    self.model_health
                        .record_failure(model_id, kind.clone(), truncated);
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

        let mut job = rsclaw_types::ExternalJob::new_submitted(
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
                account: ctx.account.clone(),
            },
            rsclaw_types::ExternalJobOrigin::Agent,
            provider_key,
            &task_id,
            rsclaw_types::ExternalJobKind::VideoGen,
            prompt,
        );
        set_video_job_timeout(&mut job);
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

    /// Query an existing video task without submitting or mutating it.
    pub(crate) async fn tool_video_status(&self, args: Value) -> Result<Value> {
        let job_id = args
            .get("job_id")
            .or_else(|| args.get("task_id"))
            .or_else(|| args.get("id"))
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| anyhow!("video_status: `job_id` is required"))?;

        // `video_gen` returns a gateway-internal UUID as `job_id`. This is the
        // preferred provider-independent lookup: it works for Seedance, Agnes,
        // OpenAI, native rsclaw and legacy avatar/MV jobs.
        if let Some(job) = self
            .store
            .db
            .get_external_job(job_id)
            .map_err(|error| anyhow!("video_status: read local job: {error}"))?
        {
            return Ok(local_video_job_status(&job));
        }

        // The tool also returns the provider `task_id`. Native rsclaw IDs are
        // self-describing, so they can be queried directly even when the local
        // queue row has already been cleaned up.
        let api_key = || {
            self.config
                .model
                .models
                .as_ref()
                .and_then(|models| models.providers.get("rsclaw"))
                .and_then(|provider| provider.api_key.as_ref())
                .and_then(|key| key.as_plain().map(str::to_owned))
                .or_else(|| std::env::var("RSCLAW_API_KEY").ok())
                .ok_or_else(|| {
                    anyhow!(
                        "video_status: no rsclaw API key configured; query with the internal `job_id` instead"
                    )
                })
        };
        let outcome = if job_id.starts_with("job_") {
            rsclaw_jobs::poll_rsclaw_native(&api_key()?, job_id).await?
        } else if job_id.starts_with("video_") {
            rsclaw_jobs::poll_rsclaw_legacy(&api_key()?, job_id).await?
        } else {
            return Err(anyhow!(
                "video_status: unknown job ID `{job_id}`; use the internal `job_id`, native `job_...`, or legacy `video_...` returned by video_gen"
            ));
        };
        Ok(provider_video_status(job_id, outcome))
    }

    /// Avatar (数字人) generation — `POST /v1/videos/avatar` (gen-api.md §3).
    /// Character image required; driven EITHER by speech `audio` (lip-sync →
    /// `talk`) OR by a driving `video` (motion/expression transfer, character
    /// swap → `animate`). The server auto-selects the lane from the inputs —
    /// no `model` needed. Body: `input_reference.image_url` + (`audio` and/or
    /// driving video as `input_references[{type:video}]`).
    pub(crate) async fn tool_avatar_gen(
        &self,
        args: Value,
        ctx: &super::runtime::RunContext,
    ) -> Result<Value> {
        let images = normalize_gen_assets(&args["image"]).await;
        let audio = normalize_gen_assets(&args["audio"]).await;
        let Some(image_url) = images.first() else {
            return Ok(
                json!({ "error": "avatar_gen: a character `image` is required (local path, https URL, or data URI)" }),
            );
        };
        // Driving video (animate lane) — local path / data-URI / http URL all
        // accepted (normalised to a data-URI for local files, same as image/
        // audio).
        let drive = normalize_gen_assets(&args["video"]).await;
        let drive_video = drive.first();
        if audio.first().is_none() && drive_video.is_none() {
            return Ok(
                json!({ "error": "avatar_gen: provide a driving signal — either `audio` (speech → lip-sync) or `video` (a driving video → motion transfer)" }),
            );
        }
        let mut body = json!({
            "input_reference": { "image_url": image_url },
        });
        if let Some(audio_url) = audio.first() {
            body["audio"] = json!(audio_url);
        }
        if let Some(v) = drive_video {
            body["input_references"] = json!([{ "type": "video", "video_url": v }]);
        }
        // Optional passthroughs: explicit lane override + animate mode.
        if let Some(m) = args["model"].as_str().filter(|s| !s.is_empty()) {
            body["model"] = json!(m.rsplit('/').next().unwrap_or(m));
        }
        if let Some(mode) = args["mode"].as_str().filter(|s| !s.is_empty()) {
            body["mode"] = json!(mode);
        }
        self.submit_rsclaw_gen_video("avatar", body, "avatar", ctx)
            .await
    }

    /// Music-video (MV) generation — `POST /v1/videos/mv` (gen-api.md §3b).
    /// Character image + lyrics → singing MV (worker chain: lyrics → music →
    /// image + audio-drive → mp4). `image` + `lyrics` REQUIRED; `prompt`
    /// (style/timbre) and `duration` optional. `model` default rsclaw-mv-v1.
    pub(crate) async fn tool_mv_gen(
        &self,
        args: Value,
        ctx: &super::runtime::RunContext,
    ) -> Result<Value> {
        let images = normalize_gen_assets(&args["image"]).await;
        let Some(image_url) = images.first() else {
            return Ok(
                json!({ "error": "mv_gen: a character `image` is required (local path, https URL, or data URI)" }),
            );
        };
        let Some(lyrics) = args["lyrics"].as_str().filter(|s| !s.is_empty()) else {
            return Ok(json!({ "error": "mv_gen: `lyrics` is required (the song words to sing)" }));
        };
        let mut body = json!({
            "input_reference": { "image_url": image_url },
            "lyrics": lyrics,
        });
        if let Some(prompt) = args["prompt"].as_str().filter(|s| !s.is_empty()) {
            body["prompt"] = json!(prompt);
        }
        if let Some(dur) = args["duration"].as_u64() {
            body["duration"] = json!(dur);
        }
        if let Some(m) = args["model"].as_str().filter(|s| !s.is_empty()) {
            body["model"] = json!(m.rsplit('/').next().unwrap_or(m));
        }
        let label = args["prompt"].as_str().unwrap_or("mv");
        self.submit_rsclaw_gen_video("mv", body, label, ctx).await
    }

    /// Shared submit path for the rsclaw-gen video families (avatar / mv).
    /// POSTs the pre-built `body` to `/v1/videos/{endpoint}`,
    /// then enqueues an `ExternalJob{ provider: "rsclaw_legacy", kind: VideoGen
    /// }`. This remains isolated from standard video's native `/v1/jobs`
    /// lifecycle.
    async fn submit_rsclaw_gen_video(
        &self,
        endpoint: &str,
        body: Value,
        job_label: &str,
        ctx: &super::runtime::RunContext,
    ) -> Result<Value> {
        let api_key = self
            .config
            .model
            .models
            .as_ref()
            .and_then(|m| m.providers.get("rsclaw"))
            .and_then(|p| p.api_key.as_ref())
            .and_then(|k| k.as_plain().map(str::to_owned))
            .or_else(|| std::env::var("RSCLAW_API_KEY").ok())
            .ok_or_else(|| {
                anyhow!(
                    "{endpoint}_gen: no API key for rsclaw. Set `model.models.providers.rsclaw.apiKey` in rsclaw.json5 or export RSCLAW_API_KEY, then retry."
                )
            })?;

        let task_id = post_rsclaw_gen(endpoint, &api_key, &body).await?;

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
                account: ctx.account.clone(),
            },
            rsclaw_types::ExternalJobOrigin::Agent,
            "rsclaw_legacy",
            &task_id,
            rsclaw_types::ExternalJobKind::VideoGen,
            job_label,
        );
        let job_id = job.id.clone();
        self.store
            .db
            .enqueue_external_job(&job)
            .map_err(|e| anyhow!("{endpoint}_gen: enqueue external job: {e}"))?;

        Ok(json!({
            "status": "submitted",
            "provider": "rsclaw_legacy",
            "kind": endpoint,
            "task_id": task_id,
            "job_id": job_id,
            "message": "Generation submitted to the rsclaw gen service. The finished video will be delivered automatically when ready. The user has been informed; do NOT poll or wait — your turn is complete."
        }))
    }
}

/// Apply the longer timeout only to standard video submitted through the native
/// rsclaw jobs API. Other providers and rsclaw legacy video keep the default.
fn set_video_job_timeout(job: &mut rsclaw_types::ExternalJob) {
    if job.provider == "rsclaw_native"
        && matches!(job.kind, rsclaw_types::ExternalJobKind::VideoGen)
    {
        job.timeout_at = job.submitted_at + RSCLAW_NATIVE_VIDEO_TIMEOUT_SECS;
    }
}

/// Normalize gen asset input(s) — image OR audio. http(s)/data: pass through;
/// a LOCAL FILE PATH is read and base64-encoded into a `data:<mime>;base64,...`
/// URI with the mime inferred from the extension (image + audio + video).
/// Unreadable paths are dropped. The rsclaw gen service accepts URL / data-URI
/// / multipart for every asset slot, so a data-URI is always safe to send.
pub(crate) async fn normalize_gen_assets(v: &Value) -> Vec<String> {
    let raw: Vec<String> = match v {
        Value::String(s) if !s.is_empty() => vec![s.clone()],
        Value::Array(a) => a
            .iter()
            .filter_map(|x| x.as_str().filter(|s| !s.is_empty()).map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    };
    let mut out = Vec::with_capacity(raw.len());
    for asset in raw {
        if asset.starts_with("http://")
            || asset.starts_with("https://")
            || asset.starts_with("data:")
        {
            out.push(asset);
            continue;
        }
        match tokio::fs::read(&asset).await {
            Ok(bytes) => {
                use base64::Engine;
                let mime = match std::path::Path::new(&asset)
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase())
                    .as_deref()
                {
                    Some("jpg") | Some("jpeg") => "image/jpeg",
                    Some("webp") => "image/webp",
                    Some("gif") => "image/gif",
                    Some("png") => "image/png",
                    Some("wav") => "audio/wav",
                    Some("mp3") => "audio/mpeg",
                    Some("flac") => "audio/flac",
                    Some("opus") => "audio/opus",
                    Some("m4a") | Some("aac") => "audio/mp4",
                    Some("mp4") => "video/mp4",
                    Some("webm") => "video/webm",
                    _ => "application/octet-stream",
                };
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                out.push(format!("data:{mime};base64,{b64}"));
            }
            Err(e) => {
                tracing::warn!(path = %asset, error = %e, "gen: asset not readable, skipping");
            }
        }
    }
    out
}

/// POST a pre-built body to `{gen_host}/v1/videos/{endpoint}` and return the
/// rsclaw `video_<id>`. 307/308 from the LB are followed by
/// `rsclaw_http::post_json` (Bearer re-attached per hop). Polling reuses
/// `rsclaw_jobs::poll_rsclaw_legacy`.
async fn post_rsclaw_gen(endpoint: &str, api_key: &str, body: &Value) -> Result<String> {
    let url = format!(
        "{}/v1/videos/{endpoint}",
        rsclaw_provider::rsclaw_http::gen_host_base(None)
    );
    let client =
        rsclaw_provider::rsclaw_http::build_client(rsclaw_provider::DEFAULT_USER_AGENT, 30)?;
    let resp = rsclaw_provider::rsclaw_http::post_json(&client, &url, api_key, body).await?;
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| anyhow!("{endpoint}_gen: rsclaw read body: {e}"))?;
    if !status.is_success() {
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let raw = String::from_utf8_lossy(&bytes);
        let msg = v
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .or_else(|| v.get("message").and_then(|v| v.as_str()))
            .unwrap_or_else(|| rsclaw_util::truncate_str(&raw, 200));
        return Err(anyhow!("{endpoint}_gen: rsclaw API {status}: {msg}"));
    }
    let v: Value = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow!("{endpoint}_gen: rsclaw parse response: {e}"))?;
    let id = v
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{endpoint}_gen: rsclaw no `id` in response: {v}"))?
        .to_owned();
    Ok(id)
}

fn local_video_job_status(job: &rsclaw_types::ExternalJob) -> Value {
    let status = match job.status {
        rsclaw_types::ExternalJobStatus::Pending if job.progress.is_some() => "in_progress",
        rsclaw_types::ExternalJobStatus::Pending => "pending",
        rsclaw_types::ExternalJobStatus::Polling => "in_progress",
        rsclaw_types::ExternalJobStatus::Done => "completed",
        rsclaw_types::ExternalJobStatus::Failed => "failed",
        rsclaw_types::ExternalJobStatus::TimedOut => "timed_out",
    };
    json!({
        "status": status,
        "job_id": job.id,
        "task_id": job.external_task_id,
        "provider": job.provider,
        "poll_count": job.poll_count,
        "submitted_at": job.submitted_at,
        "result_url": job.result_url,
        "result_path": job.result_path,
        "error": job.error,
        "progress": job.progress,
        "delivery_complete": job.delivered_at.is_some(),
    })
}

fn provider_video_status(task_id: &str, outcome: rsclaw_types::PollOutcome) -> Value {
    match outcome {
        rsclaw_types::PollOutcome::Pending => json!({
            "status": "pending",
            "task_id": task_id,
        }),
        rsclaw_types::PollOutcome::InProgress(progress) => json!({
            "status": "in_progress",
            "task_id": task_id,
            "progress": progress,
        }),
        rsclaw_types::PollOutcome::Done(url) => json!({
            "status": "completed",
            "task_id": task_id,
            "result_url": url,
        }),
        rsclaw_types::PollOutcome::Failed(error) => json!({
            "status": "failed",
            "task_id": task_id,
            "error": error,
        }),
    }
}

/// Validate a configured rsclaw model against the current native jobs contract.
fn validate_native_rsclaw_model(model: Option<&str>) -> Result<()> {
    let Some(model) = model else {
        return Ok(());
    };
    let bare = model.rsplit('/').next().unwrap_or(model);
    if bare == "rsclaw-video-v3" {
        Ok(())
    } else {
        Err(anyhow!(
            "video_gen: rsclaw native jobs only support `rsclaw-video-v3`; unsupported model `{model}`"
        ))
    }
}

/// Resolve and validate the native video resolution. The tool-level default is
/// 480p; external providers continue to use their own quality defaults.
fn video_resolution(value: Option<&Value>) -> Result<&str> {
    match value {
        None | Some(Value::Null) => Ok("480p"),
        Some(Value::String(value))
            if matches!(value.as_str(), "480p" | "720p" | "1080p" | "2k") =>
        {
            Ok(value)
        }
        Some(_) => Err(anyhow!(
            "video_gen: `resolution` must be one of 480p, 720p, 1080p, or 2k"
        )),
    }
}

/// Convert one normalized generation asset into the typed native jobs shape.
fn native_video_asset(value: &str) -> Value {
    if value.starts_with("data:") {
        json!({"type": "data_uri", "data_uri": value})
    } else {
        json!({"type": "url", "url": value})
    }
}

/// Build the typed native `/v1/jobs` request for standard rsclaw video.
fn native_video_body(
    prompt: &str,
    duration: u64,
    resolution: &str,
    aspect_ratio: &str,
    generate_audio: Option<bool>,
    images: &[String],
    video_ref: Option<&str>,
) -> Value {
    let mut frames = json!({});
    let mut references = Vec::new();
    if let Some(video) = video_ref {
        references.push(json!({
            "type": "video",
            "role": "structure",
            "asset": native_video_asset(video),
        }));
        references.extend(images.iter().map(|image| {
            json!({
                "type": "image",
                "role": "subject",
                "asset": native_video_asset(image),
            })
        }));
    } else if images.len() <= 2 {
        if let Some(first) = images.first() {
            frames["start"] = native_video_asset(first);
        }
        if let Some(last) = images.get(1) {
            frames["end"] = native_video_asset(last);
        }
    } else {
        references.extend(images.iter().map(|image| {
            json!({
                "type": "image",
                "role": "subject",
                "asset": native_video_asset(image),
            })
        }));
    }

    let mut kind = json!({
        "kind": "video",
        "model": "rsclaw-video-v3",
        "prompt": prompt,
        "resolution": resolution,
        "aspect_ratio": aspect_ratio,
        "duration_secs": duration,
        "frames": frames,
        "references": references,
    });
    if let Some(generate_audio) = generate_audio {
        kind["generate_audio"] = json!(generate_audio);
    }
    json!({"kind": kind, "metadata": {}})
}

/// Submit a standard rsclaw video task through the typed durable jobs API and
/// return its native `job_<id>`.
///
/// Standard video generation is always `rsclaw-video-v3`; old model ids and the
/// compatibility `/v1/videos` surface are intentionally not used here.
async fn submit_rsclaw_video(
    api_key: &str,
    prompt: &str,
    duration: u64,
    resolution: &str,
    aspect_ratio: &str,
    generate_audio: Option<bool>,
    model_hint: Option<&str>,
    images: &[String],
    video_ref: Option<&str>,
    idempotency_key: &str,
) -> Result<String> {
    validate_native_rsclaw_model(model_hint)?;
    let body = native_video_body(
        prompt,
        duration,
        resolution,
        aspect_ratio,
        generate_audio,
        images,
        video_ref,
    );
    let url = format!(
        "{}/v1/jobs",
        rsclaw_provider::rsclaw_http::gen_host_base(None)
    );
    let redirect_client =
        rsclaw_provider::rsclaw_http::build_client(rsclaw_provider::DEFAULT_USER_AGENT, 30)?;
    let resp = rsclaw_provider::rsclaw_http::post_json_with_idempotency_key(
        &redirect_client,
        &url,
        api_key,
        &body,
        idempotency_key,
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_rsclaw_video_timeout_is_one_hour_without_extending_others() {
        let delivery = rsclaw_types::ExternalJobDelivery {
            channel: "test".to_owned(),
            target_id: "target".to_owned(),
            is_group: false,
            reply_to: None,
            account: None,
        };
        let mut native = rsclaw_types::ExternalJob::new_submitted(
            "session",
            delivery.clone(),
            rsclaw_types::ExternalJobOrigin::Agent,
            "rsclaw_native",
            "job_1",
            rsclaw_types::ExternalJobKind::VideoGen,
            "prompt",
        );
        set_video_job_timeout(&mut native);
        assert_eq!(
            native.timeout_at - native.submitted_at,
            RSCLAW_NATIVE_VIDEO_TIMEOUT_SECS
        );

        let mut other = rsclaw_types::ExternalJob::new_submitted(
            "session",
            delivery.clone(),
            rsclaw_types::ExternalJobOrigin::Agent,
            "rsclaw_legacy",
            "task_1",
            rsclaw_types::ExternalJobKind::VideoGen,
            "prompt",
        );
        set_video_job_timeout(&mut other);
        assert_eq!(
            other.timeout_at - other.submitted_at,
            rsclaw_types::DEFAULT_TIMEOUT_SECS as i64
        );

        let mut non_video = rsclaw_types::ExternalJob::new_submitted(
            "session",
            delivery.clone(),
            rsclaw_types::ExternalJobOrigin::Agent,
            "rsclaw_native",
            "job_2",
            rsclaw_types::ExternalJobKind::ImageGen,
            "prompt",
        );
        set_video_job_timeout(&mut non_video);
        assert_eq!(
            non_video.timeout_at - non_video.submitted_at,
            rsclaw_types::DEFAULT_TIMEOUT_SECS as i64
        );
    }

    #[test]
    fn native_rsclaw_accepts_only_v3() {
        assert!(validate_native_rsclaw_model(Some("rsclaw-video-v3")).is_ok());
        assert!(validate_native_rsclaw_model(Some("rsclaw/rsclaw-video-v3")).is_ok());
        assert!(validate_native_rsclaw_model(Some("rsclaw-video-v1")).is_err());
        assert!(validate_native_rsclaw_model(Some("rsclaw-video-v1-fast")).is_err());
        assert!(validate_native_rsclaw_model(None).is_ok());
    }

    #[test]
    fn video_status_maps_provider_outcomes_without_resubmitting() {
        assert_eq!(
            provider_video_status("job_1", rsclaw_types::PollOutcome::Pending)["status"],
            "pending"
        );
        let running =
            provider_video_status("job_1", rsclaw_types::PollOutcome::InProgress(Some(0.5)));
        assert_eq!(running["status"], "in_progress");
        assert_eq!(running["progress"], 0.5);
        let completed = provider_video_status(
            "job_1",
            rsclaw_types::PollOutcome::Done("https://example.test/video.mp4".to_owned()),
        );
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["result_url"], "https://example.test/video.mp4");
        let failed = provider_video_status(
            "job_1",
            rsclaw_types::PollOutcome::Failed("backend_failure".to_owned()),
        );
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["error"], "backend_failure");
    }

    #[test]
    fn native_video_resolution_defaults_to_480p_and_validates_values() {
        assert_eq!(video_resolution(None).expect("default resolution"), "480p");
        assert_eq!(
            video_resolution(Some(&json!("1080p"))).expect("explicit resolution"),
            "1080p"
        );
        assert!(video_resolution(Some(&json!("4k"))).is_err());
        assert!(video_resolution(Some(&json!(1080))).is_err());
    }

    #[test]
    fn native_video_assets_use_typed_protocol_variants() {
        assert_eq!(
            native_video_asset("https://example.test/a.png"),
            json!({"type": "url", "url": "https://example.test/a.png"})
        );
        assert_eq!(
            native_video_asset("data:image/png;base64,AA=="),
            json!({"type": "data_uri", "data_uri": "data:image/png;base64,AA=="})
        );
    }

    #[test]
    fn native_video_body_matches_jobs_contract_for_frames_and_v2v() {
        let images = vec![
            "https://example.test/start.png".to_owned(),
            "https://example.test/end.png".to_owned(),
        ];
        let body = native_video_body("snow", 5, "480p", "16:9", None, &images, None);
        assert_eq!(body["kind"]["model"], "rsclaw-video-v3");
        assert_eq!(body["kind"]["resolution"], "480p");
        assert_eq!(body["kind"]["duration_secs"], 5);
        assert!(body["kind"].get("fps").is_none());
        assert!(body["kind"].get("steps").is_none());
        assert!(body["kind"].get("generate_audio").is_none());
        assert_eq!(body["kind"]["frames"]["start"]["type"], "url");
        assert_eq!(body["kind"]["frames"]["end"]["url"], images[1]);
        assert!(
            body["kind"]["references"]
                .as_array()
                .expect("references")
                .is_empty()
        );

        let body = native_video_body(
            "transfer",
            6,
            "1080p",
            "9:16",
            Some(true),
            &["https://example.test/subject.png".to_owned()],
            Some("https://example.test/drive.mp4"),
        );
        assert_eq!(body["kind"]["generate_audio"], true);
        assert_eq!(body["kind"]["references"][0]["role"], "structure");
        assert_eq!(body["kind"]["references"][0]["type"], "video");
        assert_eq!(body["kind"]["references"][1]["role"], "subject");

        let body = native_video_body("silent", 5, "2k", "1:1", Some(false), &[], None);
        assert_eq!(body["kind"]["generate_audio"], false);
    }
}
