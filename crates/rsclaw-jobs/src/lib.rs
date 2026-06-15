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
    images: &[String],
) -> Result<String> {
    // Chain entries / `model` args arrive provider-prefixed
    // (`doubao/doubao-seedance-2-0-fast-…`); strip it so the bare ARK model
    // id goes on the wire. This is what lets the caller pick any seedance
    // variant (pro / fast / lite) via the model field.
    let model = model_override
        .map(|m| m.rsplit('/').next().unwrap_or(m))
        .filter(|m| !m.is_empty() && *m != "doubao" && *m != "bytedance")
        .unwrap_or(SEEDANCE_DEFAULT_MODEL);
    // Build the `content` array: a text item plus, for image-to-video, one
    // `image_url` item per reference frame with a `role`. Per the Ark spec
    // the three image scenes are mutually exclusive, so map by count:
    //   1 image  → first_frame  (图生视频-首帧)
    //   2 images → first_frame + last_frame  (图生视频-首尾帧)
    //   3+ images→ reference_image each  (多模态参考生视频, Seedance 2.0, 1~9)
    // `url` accepts a public URL or a `data:image/<fmt>;base64,...` Data URI.
    let mut content = vec![json!({"type": "text", "text": prompt})];
    let roles: &[&str] = match images.len() {
        0 | 1 => &["first_frame"],
        2 => &["first_frame", "last_frame"],
        _ => &[],
    };
    for (i, img) in images.iter().enumerate() {
        let role = if images.len() >= 3 {
            "reference_image"
        } else {
            roles[i]
        };
        content.push(json!({
            "type": "image_url",
            "image_url": {"url": img},
            "role": role,
        }));
    }
    let body = json!({
        "model": model,
        "content": content,
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
    images: &[String],
) -> Result<String> {
    // Strip provider prefix so a `minimax/<id>` chain entry sends the bare
    // model id (lets the caller pick Hailuo-2.3 / 2.3-Fast etc).
    let model = model_override
        .map(|m| m.rsplit('/').next().unwrap_or(m))
        .filter(|m| !m.is_empty() && *m != "minimax")
        .unwrap_or(MINIMAX_DEFAULT_MODEL);
    let mut body = json!({
        "prompt": prompt,
        "model": model,
        "duration": duration,
        "resolution": minimax_resolution(aspect_ratio),
    });
    // Image-to-video: MiniMax (Hailuo) takes a single `first_frame_image`,
    // a public URL or a `data:image/...;base64,...` Data URI.
    if let Some(first) = images.first() {
        body["first_frame_image"] = json!(first);
    }
    let resp: serde_json::Value = client
        .post(format!("{MINIMAX_BASE}/video_generation"))
        .bearer_auth(api_key)
        .json(&body)
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
    images: &[String],
) -> Result<String> {
    // Strip provider prefix so a `kling/<id>` chain entry sends the bare
    // model_name (lets the caller pick kling-v1 / v2-master etc).
    let model = model_override
        .map(|m| m.rsplit('/').next().unwrap_or(m))
        .filter(|m| !m.is_empty() && *m != "kling")
        .unwrap_or(KLING_DEFAULT_MODEL);
    let jwt = kling_jwt(access_key, secret_key)?;
    // Kling wants RAW base64 for image inputs — strip the `data:<mime>;base64,`
    // prefix that the tool layer adds. Public URLs pass through untouched.
    let kling_img = |s: &String| -> String {
        match s.split_once(";base64,") {
            Some((_, b64)) => b64.to_owned(),
            None => s.clone(),
        }
    };
    // Image-to-video uses a different endpoint + body: `image` (start frame)
    // and optional `image_tail` (end frame). aspect_ratio is derived from the
    // image, so it's omitted there.
    let (url, body) = if images.is_empty() {
        (
            format!("{KLING_BASE}/v1/videos/text2video"),
            json!({
                "model_name": model,
                "prompt": prompt,
                "duration": duration.to_string(),
                "aspect_ratio": aspect_ratio,
            }),
        )
    } else {
        let mut b = json!({
            "model_name": model,
            "prompt": prompt,
            "duration": duration.to_string(),
            "image": kling_img(&images[0]),
        });
        if let Some(tail) = images.get(1) {
            b["image_tail"] = json!(kling_img(tail));
        }
        (format!("{KLING_BASE}/v1/videos/image2video"), b)
    };
    let resp: serde_json::Value = client
        .post(url)
        .bearer_auth(&jwt)
        .json(&body)
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

/// Submit an Agnes text→video OR image→video task and return the provider's
/// `video_id`. When `images` is non-empty the request becomes image-to-video
/// (`image` array + `mode: ti2vid`); each entry is a public URL or a
/// `data:image/...;base64,...` Data URI.
pub async fn submit_agnes(
    client: &reqwest::Client,
    api_key: &str,
    prompt: &str,
    duration: u64,
    aspect_ratio: &str,
    model_override: Option<&str>,
    images: &[String],
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
    let mut body = json!({
        "model": model,
        "prompt": prompt,
        "width": width,
        "height": height,
        "frame_rate": frame_rate,
        "num_frames": num_frames,
    });
    // Image-to-video. Per the Agnes Video V2.0 spec the shapes differ by
    // count: a single reference frame goes in the TOP-LEVEL `image` as a
    // STRING (`mode: ti2vid`) — passing an array there 400s
    // ("cannot unmarshal array ... type string"). Multiple frames
    // (first+last / keyframes) go in `extra_body.image` as an array with
    // `mode: keyframes`. Each entry is a public URL or
    // `data:image/...;base64,...` Data URI. Text-to-video sends neither.
    match images.len() {
        0 => {}
        1 => {
            body["image"] = json!(images[0]);
            body["mode"] = json!("ti2vid");
        }
        _ => {
            body["mode"] = json!("keyframes");
            body["extra_body"] = json!({ "image": images, "mode": "keyframes" });
        }
    }
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
// OpenAI-compatible video (Sora-2 shape) — async submit + poll, configurable
// base_url. Defaults to https://api.openai.com/v1 but any OAI-compatible
// video gateway works by setting `models.providers.openai.base_url`. Mirrors
// the image side's `custom_oai` baseUrl passthrough.
// ---------------------------------------------------------------------------

const OPENAI_DEFAULT_VIDEO_MODEL: &str = "sora-2";

fn openai_video_size(aspect_ratio: &str) -> &'static str {
    match aspect_ratio {
        "9:16" => "720x1280",
        "1:1" => "1024x1024",
        _ => "1280x720",
    }
}

/// Submit an OpenAI-compatible (Sora-2 shape) text→video / image→video task
/// and return the provider's `id`. `base_url` is the provider's configured
/// endpoint (already including the `/v1` suffix for stock OpenAI); we post to
/// `{base}/videos`. Each `images` entry is a public URL or a
/// `data:image/...;base64,...` Data URI; the first frame is forwarded as
/// `input_reference` (OAI) — gateways that don't recognise it ignore it.
pub async fn submit_openai_video(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    prompt: &str,
    duration: u64,
    aspect_ratio: &str,
    model_override: Option<&str>,
    images: &[String],
) -> Result<String> {
    let model = model_override
        .map(|m| m.rsplit('/').next().unwrap_or(m))
        .filter(|m| !m.is_empty() && *m != "openai")
        .unwrap_or(OPENAI_DEFAULT_VIDEO_MODEL);
    let mut body = json!({
        "model": model,
        "prompt": prompt,
        "seconds": duration,
        "size": openai_video_size(aspect_ratio),
    });
    if let Some(first) = images.first() {
        body["input_reference"] = json!(first);
    }
    let url = format!("{}/videos", base_url.trim_end_matches('/'));
    let resp: serde_json::Value = client
        .post(url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("openai-video: submit failed: {e}"))?
        .json()
        .await
        .map_err(|e| anyhow!("openai-video: submit parse failed: {e}"))?;
    let id = resp["id"]
        .as_str()
        .ok_or_else(|| anyhow!("openai-video: no id in response: {resp}"))?
        .to_owned();
    Ok(id)
}

pub async fn poll_openai_video(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    id: &str,
) -> Result<PollOutcome> {
    let base = base_url.trim_end_matches('/');
    let resp: serde_json::Value = client
        .get(format!("{base}/videos/{id}"))
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| anyhow!("openai-video: poll failed: {e}"))?
        .json()
        .await
        .map_err(|e| anyhow!("openai-video: poll parse failed: {e}"))?;
    let status = resp["status"].as_str().unwrap_or("unknown");
    match status {
        "completed" | "succeeded" => {
            // Prefer an explicit URL in the status body (what OAI-compatible
            // gateways return). Stock OpenAI serves bytes at `/videos/{id}/content`
            // behind auth instead — fall back to that URL last so at least the
            // gateway case delivers.
            let url = resp["video_url"]
                .as_str()
                .or_else(|| resp.pointer("/data/0/url").and_then(|v| v.as_str()))
                .or_else(|| resp["url"].as_str())
                .or_else(|| resp.pointer("/output/url").and_then(|v| v.as_str()))
                .filter(|s| s.starts_with("http"))
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{base}/videos/{id}/content"));
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
