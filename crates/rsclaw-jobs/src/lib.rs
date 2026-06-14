//! `rsclaw-jobs` — pure provider-API job layer for async video / image
//! generation (crate-split, step-12 P2).
//!
//! Lifted out of `gateway/external_jobs_worker.rs`: the per-provider HTTP
//! adapters (`submit_*` for the tool side, `poll_*` + `download_artifact`
//! for the worker side). This crate has NO gateway dependency — it knows
//! nothing about the task queue, `ExternalJob` persistence, channels, or
//! shutdown. The gateway-coupled `ExternalJobsWorker` tick/delivery loop
//! stays in root and calls back into these functions.
//!
//! This breaks the old agent->gateway edge: `agent::tools_video` now calls
//! `rsclaw_jobs::submit_*` instead of reaching into the gateway worker.
//!
//! Adding a new async provider means: add the corresponding `submit_*` for
//! the tool side and `poll_*` for the worker side, then extend the
//! `dispatch_poll` match in the gateway worker.

use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::json;

use rsclaw_types::{ExternalJobKind, PollOutcome};

// ---------------------------------------------------------------------------
// Seedance (ByteDance ARK) — async submit + poll
// ---------------------------------------------------------------------------

const SEEDANCE_BASE: &str = "https://ark.cn-beijing.volces.com/api/v3";
const SEEDANCE_DEFAULT_MODEL: &str = "doubao-seedance-2-0-260128";

/// Submit a Seedance video generation task and return the provider's
/// `task_id`. The caller is responsible for persisting an `ExternalJob`
/// referencing this id so the worker can pick up polling.
pub async fn submit_seedance(
    client: &reqwest::Client,
    api_key: &str,
    prompt: &str,
    duration: u64,
    aspect_ratio: &str,
    model_override: Option<&str>,
) -> Result<String> {
    let model = model_override.unwrap_or(SEEDANCE_DEFAULT_MODEL);
    let body = json!({
        "model": model,
        "content": [{"type": "text", "text": prompt}],
        "ratio": aspect_ratio,
        "duration": duration,
        "watermark": false,
    });
    let resp: serde_json::Value = client
        .post(format!("{SEEDANCE_BASE}/contents/generations/tasks"))
        .bearer_auth(api_key)
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
    Ok(task_id)
}

pub async fn poll_seedance(
    client: &reqwest::Client,
    api_key: &str,
    task_id: &str,
) -> Result<PollOutcome> {
    let resp: serde_json::Value = client
        .get(format!(
            "{SEEDANCE_BASE}/contents/generations/tasks/{task_id}"
        ))
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| anyhow!("seedance: poll failed: {e}"))?
        .json()
        .await
        .map_err(|e| anyhow!("seedance: poll parse failed: {e}"))?;
    let status = resp["status"].as_str().unwrap_or("unknown");
    match status {
        "succeeded" => {
            let url = resp
                .pointer("/content/video_url")
                .or_else(|| resp.pointer("/content/0/video_url/url"))
                .or_else(|| resp.pointer("/content/0/url"))
                .or_else(|| resp.pointer("/result/video_url/url"))
                .or_else(|| resp.pointer("/output/url"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("seedance: no video URL in result: {resp}"))?
                .to_owned();
            Ok(PollOutcome::Done(url))
        }
        "failed" | "cancelled" => {
            let msg = resp["error"]["message"]
                .as_str()
                .or_else(|| resp["message"].as_str())
                .unwrap_or("task failed");
            Ok(PollOutcome::Failed(format!("{status}: {msg}")))
        }
        _ => Ok(PollOutcome::Pending),
    }
}

// ---------------------------------------------------------------------------
// MiniMax (Hailuo) — async submit + poll
// ---------------------------------------------------------------------------

const MINIMAX_BASE: &str = "https://api.minimaxi.com/v1";
const MINIMAX_DEFAULT_MODEL: &str = "video-01-director";

fn minimax_resolution(aspect_ratio: &str) -> &'static str {
    match aspect_ratio {
        "9:16" => "720x1280",
        "1:1" => "720x720",
        _ => "1280x720",
    }
}

/// Submit a MiniMax video generation task and return the provider's
/// `task_id`. Status polling resolves to a `file_id`, which a follow-up
/// `/files/retrieve` call inside `poll_minimax` turns into a download URL.
pub async fn submit_minimax(
    client: &reqwest::Client,
    api_key: &str,
    prompt: &str,
    duration: u64,
    aspect_ratio: &str,
    model_override: Option<&str>,
) -> Result<String> {
    let model = model_override.unwrap_or(MINIMAX_DEFAULT_MODEL);
    let resp: serde_json::Value = client
        .post(format!("{MINIMAX_BASE}/video_generation"))
        .bearer_auth(api_key)
        .json(&json!({
            "prompt": prompt,
            "model": model,
            "duration": duration,
            "resolution": minimax_resolution(aspect_ratio),
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
    Ok(task_id)
}

pub async fn poll_minimax(
    client: &reqwest::Client,
    api_key: &str,
    task_id: &str,
) -> Result<PollOutcome> {
    let poll: serde_json::Value = client
        .get(format!("{MINIMAX_BASE}/query/video_generation"))
        .bearer_auth(api_key)
        .query(&[("task_id", task_id)])
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
    match status {
        "Success" => {
            // MiniMax: status=Success → /task/file_id → /files/retrieve → download_url
            let file_id = poll
                .pointer("/task/file_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("minimax: no file_id in result: {poll}"))?
                .to_owned();
            let file_resp: serde_json::Value = client
                .get(format!("{MINIMAX_BASE}/files/retrieve"))
                .bearer_auth(api_key)
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
            Ok(PollOutcome::Done(url))
        }
        "Fail" => Ok(PollOutcome::Failed(format!(
            "minimax task {task_id} failed"
        ))),
        _ => Ok(PollOutcome::Pending),
    }
}

// ---------------------------------------------------------------------------
// Kling (Kuaishou) — async submit + poll, JWT auth
// ---------------------------------------------------------------------------

const KLING_BASE: &str = "https://api.klingai.com";
const KLING_DEFAULT_MODEL: &str = "kling-v2-master";

/// Submit a Kling text→video task and return the provider's `task_id`.
/// JWT is built fresh for each call (30 min expiry; cheap to regenerate).
pub async fn submit_kling(
    client: &reqwest::Client,
    access_key: &str,
    secret_key: &str,
    prompt: &str,
    duration: u64,
    aspect_ratio: &str,
    model_override: Option<&str>,
) -> Result<String> {
    let model = model_override.unwrap_or(KLING_DEFAULT_MODEL);
    let jwt = kling_jwt(access_key, secret_key)?;
    let resp: serde_json::Value = client
        .post(format!("{KLING_BASE}/v1/videos/text2video"))
        .bearer_auth(&jwt)
        .json(&json!({
            "model_name": model,
            "prompt": prompt,
            "duration": duration.to_string(),
            "aspect_ratio": aspect_ratio,
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
    Ok(task_id)
}

pub async fn poll_kling(
    client: &reqwest::Client,
    access_key: &str,
    secret_key: &str,
    task_id: &str,
) -> Result<PollOutcome> {
    let jwt = kling_jwt(access_key, secret_key)?;
    let poll: serde_json::Value = client
        .get(format!("{KLING_BASE}/v1/videos/text2video/{task_id}"))
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
    match status {
        "succeed" => {
            let url = poll
                .pointer("/data/task_result/videos/0/url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("kling: no video URL in result: {poll}"))?
                .to_owned();
            Ok(PollOutcome::Done(url))
        }
        "failed" => {
            let msg = poll
                .pointer("/data/task_status_msg")
                .and_then(|v| v.as_str())
                .unwrap_or("task failed");
            Ok(PollOutcome::Failed(format!("kling: {msg}")))
        }
        _ => Ok(PollOutcome::Pending),
    }
}

/// Build a short-lived JWT for Kling API authentication (HS256).
fn kling_jwt(access_key: &str, secret_key: &str) -> Result<String> {
    use base64::Engine;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let now = chrono::Utc::now().timestamp();
    let header =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#);
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

// ---------------------------------------------------------------------------
// Artifact download
// ---------------------------------------------------------------------------

/// Download the provider URL into
/// `~/Downloads/rsclaw/<category>/dl_<X>_<ts><abc>.<ext>` using the same
/// canonical naming as the synchronous tool path. Returns the absolute local
/// path.
pub async fn download_artifact(
    client: &reqwest::Client,
    url: &str,
    kind: ExternalJobKind,
) -> Result<String> {
    let bytes = client
        .get(url)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| anyhow!("download: {e}"))?
        .bytes()
        .await
        .map_err(|e| anyhow!("download read: {e}"))?;
    let ext = match kind {
        ExternalJobKind::VideoGen => "mp4",
        ExternalJobKind::ImageGen => "png",
    };
    let kind_letter = rsclaw_channel::kind_from_extension(ext);
    let category = rsclaw_channel::category_for_kind(kind_letter);
    let dir = dirs_next::download_dir()
        .unwrap_or_else(|| {
            dirs_next::home_dir()
                .unwrap_or_else(rsclaw_config::loader::base_dir)
                .join("Downloads")
        })
        .join("rsclaw")
        .join(category);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| anyhow!("download: create_dir: {e}"))?;
    let ts = chrono::Local::now().format("%Y%m%d%H%M").to_string();
    let abc: String = (0..3)
        .map(|_| (rand::random::<u8>() % 26 + b'a') as char)
        .collect();
    let path = dir.join(format!("dl_{kind_letter}_{ts}{abc}.{ext}"));
    tokio::fs::write(&path, &bytes)
        .await
        .map_err(|e| anyhow!("download: write: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}

// ---------------------------------------------------------------------------
// Agnes Video V2.0 (Sapiens AI) — async submit + poll
// ---------------------------------------------------------------------------
//
// Submit: POST https://apihub.agnes-ai.com/v1/videos → {video_id, task_id,
//   status:"queued", ...}. We key off `video_id` (the doc's recommended
//   retrieval id).
// Poll:   GET https://apihub.agnes-ai.com/agnesapi?video_id=<VIDEO_ID>.
//   status ∈ {queued, in_progress, completed, failed}; the finished URL is
//   surfaced under one of a few field names depending on upstream shape.

const AGNES_BASE: &str = "https://apihub.agnes-ai.com";
const AGNES_DEFAULT_VIDEO_MODEL: &str = "agnes-video-v2.0";

/// Map an `aspect_ratio` string to a 720p-tier `(width, height)`. Agnes
/// standardizes off-spec sizes to the nearest tier anyway, so this only
/// needs to be close.
fn agnes_video_dims(aspect_ratio: &str) -> (u32, u32) {
    match aspect_ratio {
        "9:16" => (720, 1280),
        "1:1" => (960, 960),
        "4:3" => (1024, 768),
        "3:4" => (768, 1024),
        _ => (1280, 720), // 16:9 default
    }
}

/// Submit an Agnes text→video task and return the provider's `video_id`.
pub async fn submit_agnes(
    client: &reqwest::Client,
    api_key: &str,
    prompt: &str,
    duration: u64,
    aspect_ratio: &str,
    model_override: Option<&str>,
) -> Result<String> {
    // Chain entries arrive as `agnes/agnes-video-v2.0`; strip the
    // `provider/` prefix so the upstream `model` field is the bare id.
    let model = model_override
        .map(|m| m.rsplit('/').next().unwrap_or(m))
        .filter(|m| !m.is_empty() && *m != "agnes")
        .unwrap_or(AGNES_DEFAULT_VIDEO_MODEL);
    let (width, height) = agnes_video_dims(aspect_ratio);
    let frame_rate = 24u64;
    // num_frames must satisfy 8n+1 and be ≤ 441.
    let raw_frames = duration.saturating_mul(frame_rate).max(8);
    let num_frames = (((raw_frames - 1) / 8) * 8 + 1).min(441);
    let body = json!({
        "model": model,
        "prompt": prompt,
        "width": width,
        "height": height,
        "frame_rate": frame_rate,
        "num_frames": num_frames,
    });
    let resp: serde_json::Value = client
        .post(format!("{AGNES_BASE}/v1/videos"))
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("agnes: submit failed: {e}"))?
        .json()
        .await
        .map_err(|e| anyhow!("agnes: submit parse failed: {e}"))?;
    // Prefer video_id (recommended retrieval id); fall back to id/task_id.
    let id = resp["video_id"]
        .as_str()
        .or_else(|| resp["id"].as_str())
        .or_else(|| resp["task_id"].as_str())
        .ok_or_else(|| anyhow!("agnes: no video_id/task_id in response: {resp}"))?
        .to_owned();
    Ok(id)
}

pub async fn poll_agnes(
    client: &reqwest::Client,
    api_key: &str,
    video_id: &str,
) -> Result<PollOutcome> {
    let resp: serde_json::Value = client
        .get(format!("{AGNES_BASE}/agnesapi"))
        .query(&[("video_id", video_id)])
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| anyhow!("agnes: poll failed: {e}"))?
        .json()
        .await
        .map_err(|e| anyhow!("agnes: poll parse failed: {e}"))?;
    let status = resp["status"].as_str().unwrap_or("unknown");
    match status {
        "completed" | "succeeded" => {
            let url = resp["video_url"]
                .as_str()
                .or_else(|| resp["url"].as_str())
                .or_else(|| resp.pointer("/data/0/url").and_then(|v| v.as_str()))
                .or_else(|| resp["remixed_from_video_id"].as_str())
                .filter(|s| s.starts_with("http"))
                .ok_or_else(|| anyhow!("agnes: no video URL in completed result: {resp}"))?
                .to_owned();
            Ok(PollOutcome::Done(url))
        }
        "failed" | "cancelled" | "error" => {
            let msg = resp
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .or_else(|| resp["error"].as_str())
                .or_else(|| resp["message"].as_str())
                .unwrap_or("task failed");
            Ok(PollOutcome::Failed(format!("{status}: {msg}")))
        }
        _ => Ok(PollOutcome::Pending),
    }
}

// ---------------------------------------------------------------------------
// rsclaw (in-fleet `RsclawGenBackend`) — async submit (via
// `agent::tools_video::submit_rsclaw_video`) + poll here.
// ---------------------------------------------------------------------------

/// Poll a rsclaw `video_<id>` job and resolve to an authless download
/// URL on completion.
///
/// Flow:
/// 1. `GET https://api.rsclaw.ai/v1/videos/{id}` (with Bearer; 307/308
///    re-attached by `rsclaw_http::get`) → JSON `{id, status, …}`.
/// 2. `status == "completed"` →
///    `GET https://api.rsclaw.ai/v1/videos/{id}/content` (with Bearer)
///    returns 307 to a Cloudflare R2 presigned URL — we DON'T follow
///    that hop here so Authorization never crosses to R2. The presigned
///    URL is handed back as `PollOutcome::Done(url)`; the caller's
///    `download_artifact` GETs it without auth.
/// 3. `status` in {"failed","cancelled"} → `PollOutcome::Failed(reason)`.
/// 4. Else (`queued` / `in_progress`) → `PollOutcome::Pending`.
pub async fn poll_rsclaw(api_key: &str, video_id: &str) -> Result<PollOutcome> {
    let host = rsclaw_provider::rsclaw_http::gen_host_base(None);
    let client = rsclaw_provider::rsclaw_http::build_client(
        rsclaw_provider::DEFAULT_USER_AGENT,
        30,
    )?;

    // 1. Status probe.
    let status_url = format!("{host}/v1/videos/{video_id}");
    let resp = rsclaw_provider::rsclaw_http::get(&client, &status_url, api_key).await?;
    let st = resp.status();
    if !st.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "rsclaw: status {st}: {}",
            body.chars().take(200).collect::<String>()
        ));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| anyhow!("rsclaw: status parse: {e}"))?;
    let status = v["status"].as_str().unwrap_or("unknown");

    match status {
        "completed" => {
            // 2. Resolve `/content` → R2 presigned URL via the 307. The
            //    helper returns the Location target without following
            //    so Bearer never reaches Cloudflare.
            let content_url = format!("{host}/v1/videos/{video_id}/content");
            match rsclaw_provider::rsclaw_http::get_content_url(&client, &content_url, api_key)
                .await?
            {
                Some(presigned) => Ok(PollOutcome::Done(presigned)),
                None => {
                    // In-memory BlobStore (dev) — content endpoint
                    // serves bytes inline. Surface the API URL itself;
                    // `download_artifact` will hit it without auth and
                    // get 401 — flag as failed so ops sees the
                    // misconfiguration rather than a silent hang.
                    Err(anyhow!(
                        "rsclaw: /content returned bytes inline (in-memory BlobStore); \
                         configure S3-shaped blob store to enable artifact delivery"
                    ))
                }
            }
        }
        "failed" | "cancelled" => {
            let msg = v
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .or_else(|| v["error"].as_str())
                .or_else(|| v["message"].as_str())
                .unwrap_or("task failed");
            Ok(PollOutcome::Failed(format!("{status}: {msg}")))
        }
        // queued / in_progress / anything else the server may add later
        _ => Ok(PollOutcome::Pending),
    }
}
