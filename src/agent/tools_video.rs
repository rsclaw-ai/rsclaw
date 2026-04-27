//! Video generation tool — `tool_video` dispatcher for Seedance / MiniMax /
//! Kling.
//!
//! All providers are async HTTP APIs: this file only resolves config and
//! credentials, calls the matching `submit_*` in
//! `gateway::external_jobs_worker`, and persists an `ExternalJob`. The
//! worker handles polling, download, and channel delivery.
//!
//! Split from `tools_misc.rs` for maintainability. Methods live in
//! `impl AgentRuntime` via the split-impl pattern.

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

impl super::runtime::AgentRuntime {
    /// Generate a video from a text prompt.
    ///
    /// Supports Seedance (ByteDance ARK), MiniMax (Hailuo), and Kling
    /// (Kuaishou). Returns immediately after submit; the artifact is
    /// pushed back through the original channel when polling finishes.
    pub(crate) async fn tool_video(&self, args: Value, ctx: &super::runtime::RunContext) -> Result<Value> {
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
            if hint.contains("kling") || hint.contains("kuaishou") {
                "kling"
            } else if hint.contains("minimax") || hint.contains("hailuo") {
                "minimax"
            } else {
                "doubao"
            }
        } else if let Some(ref vm) = user_video_model {
            let vm = vm.to_lowercase();
            if vm.contains("kling") {
                "kling"
            } else if vm.contains("minimax") || vm.contains("hailuo") {
                "minimax"
            } else {
                "doubao"
            }
        } else {
            // Auto-detect: pick the first configured provider.
            let has_ark = resolve_key("doubao", "ARK_API_KEY").is_some();
            let has_minimax = resolve_key("minimax", "MINIMAX_API_KEY").is_some();
            let has_kling = resolve_key("kling", "KLING_ACCESS_KEY").is_some()
                || std::env::var("KLING_ACCESS_KEY").is_ok();
            if has_ark {
                "doubao"
            } else if has_minimax {
                "minimax"
            } else if has_kling {
                "kling"
            } else {
                return Ok(json!({
                    "error": "No video provider configured. Configure a provider with API key in rsclaw.json5, or set env vars: ARK_API_KEY, MINIMAX_API_KEY, KLING_ACCESS_KEY+KLING_SECRET_KEY."
                }));
            }
        };

        // Resolve API key for the selected provider from config -> env var.
        let api_key = match provider {
            "doubao" => resolve_key("doubao", "ARK_API_KEY"),
            "minimax" => resolve_key("minimax", "MINIMAX_API_KEY"),
            "kling" => None, // Kling uses access_key + secret_key pair, resolved below
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

        // All supported providers are async HTTP APIs: submit, persist an
        // ExternalJob with delivery context, return immediately. The
        // ExternalJobsWorker polls + delivers the artifact when ready and
        // the row in redb keeps the work alive across gateway restarts.
        let (provider_key, task_id) = match provider {
            "doubao" => {
                let key = api_key.ok_or_else(|| anyhow!("video_gen: no API key for doubao/Seedance"))?;
                let id = crate::gateway::external_jobs_worker::submit_seedance(
                    &client, &key, prompt, duration, aspect_ratio, user_video_model.as_deref(),
                ).await?;
                ("seedance", id)
            }
            "minimax" => {
                let key = api_key.ok_or_else(|| anyhow!("video_gen: no API key for MiniMax"))?;
                let id = crate::gateway::external_jobs_worker::submit_minimax(
                    &client, &key, prompt, duration, aspect_ratio, user_video_model.as_deref(),
                ).await?;
                ("minimax", id)
            }
            "kling" => {
                let (ak, sk) = kling_keys.unwrap_or((None, None));
                let access = ak.ok_or_else(|| anyhow!("video_gen: KLING_ACCESS_KEY not configured"))?;
                let secret = sk.ok_or_else(|| anyhow!("video_gen: KLING_SECRET_KEY not configured"))?;
                let id = crate::gateway::external_jobs_worker::submit_kling(
                    &client, &access, &secret, prompt, duration, aspect_ratio, user_video_model.as_deref(),
                ).await?;
                ("kling", id)
            }
            other => bail!("video_gen: unsupported provider {other}"),
        };
        tracing::info!(provider = provider_key, task_id, "tool_video: task submitted (async)");

        let job = crate::gateway::external_jobs::ExternalJob::new_submitted(
            ctx.session_key.clone(),
            crate::gateway::external_jobs::ExternalJobDelivery {
                channel: ctx.channel.clone(),
                target_id: if ctx.chat_id.is_empty() {
                    ctx.peer_id.clone()
                } else {
                    ctx.chat_id.clone()
                },
                is_group: !ctx.chat_id.is_empty() && ctx.chat_id != ctx.peer_id,
                reply_to: None,
            },
            crate::gateway::external_jobs::ExternalJobOrigin::Agent,
            provider_key,
            &task_id,
            crate::gateway::external_jobs::ExternalJobKind::VideoGen,
            prompt,
        );
        let job_id = job.id.clone();
        self.store.db.enqueue_external_job(&job)
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
