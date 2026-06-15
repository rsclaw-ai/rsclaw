//! Audio generation tool — `audio_gen` covering music (`POST /v1/audio/music`)
//! and voice / TTS-clone (`POST /v1/audio/speech`) on the rsclaw gen surface.
//!
//! Both are SYNCHRONOUS per gen-api.md §4/§5: the HTTP response body IS the
//! audio bytes (`Content-Type: audio/*`). So — unlike video / avatar / mv —
//! there's no `ExternalJob` polling. We POST, save the returned bytes, and
//! return an `audio_file` path that the agent reply boundary auto-attaches as
//! an audio file (same mechanism that now also delivers `tool_tts`).
//!
//! Slow music jobs may exceed the synchronous window and return `504` with a
//! `poll GET /v1/jobs/{id}` hint; v1 surfaces that as a retryable error rather
//! than wiring a second async job kind.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

impl super::runtime::AgentRuntime {
    /// Generate audio (music or voice) via the rsclaw gen service.
    pub(crate) async fn tool_audio_gen(&self, args: Value) -> Result<Value> {
        let kind = args["kind"].as_str().unwrap_or("music").to_lowercase();

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
                    "audio_gen: no API key for rsclaw. Set `model.models.providers.rsclaw.apiKey` in rsclaw.json5 or export RSCLAW_API_KEY, then retry."
                )
            })?;

        let host = rsclaw_provider::rsclaw_http::gen_host_base(None);
        let ua = self
            .config
            .gateway
            .user_agent
            .as_deref()
            .unwrap_or(rsclaw_provider::DEFAULT_USER_AGENT);
        // Music synthesis can run well past a typical request; give it room.
        let client = reqwest::Client::builder()
            .user_agent(ua)
            .timeout(std::time::Duration::from_secs(180))
            .build()
            .unwrap_or_default();

        // Output container — default mp3 for IM-platform compatibility
        // (feishu/weixin won't render ogg/opus inline), overridable.
        let fmt = args["response_format"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("mp3")
            .to_owned();

        let (endpoint, body) = match kind.as_str() {
            "voice" | "speech" | "tts" => {
                let input = args["text"]
                    .as_str()
                    .or_else(|| args["input"].as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        anyhow!("audio_gen(voice): `text` (the words to speak) is required")
                    })?;
                let model = args["model"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(|m| m.rsplit('/').next().unwrap_or(m))
                    .unwrap_or("rsclaw-voice-v1");
                let mut b = json!({
                    "model": model,
                    "input": input,
                    "response_format": fmt,
                });
                if let Some(voice) = args["voice"].as_str().filter(|s| !s.is_empty()) {
                    b["voice"] = json!(voice);
                }
                if let Some(instr) = args["instructions"].as_str().filter(|s| !s.is_empty()) {
                    b["instructions"] = json!(instr);
                }
                if let Some(speed) = args["speed"].as_f64() {
                    b["speed"] = json!(speed);
                }
                // One-shot voice clone: reference_audio (URL / data-URI / local
                // path → base64). reference_text optionally improves fidelity.
                let refs = super::tools_video::normalize_gen_assets(&args["reference_audio"]).await;
                if let Some(r) = refs.first() {
                    b["reference_audio"] = json!({ "audio_url": r });
                    if let Some(rt) = args["reference_text"].as_str().filter(|s| !s.is_empty()) {
                        b["reference_text"] = json!(rt);
                    }
                }
                ("/v1/audio/speech", b)
            }
            _ => {
                // music
                let prompt = args["prompt"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        anyhow!("audio_gen(music): `prompt` (style description) is required")
                    })?;
                let model = args["model"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(|m| m.rsplit('/').next().unwrap_or(m))
                    .unwrap_or("rsclaw-music-v1");
                let mut b = json!({
                    "model": model,
                    "prompt": prompt,
                    "response_format": fmt,
                });
                if let Some(lyrics) = args["lyrics"].as_str().filter(|s| !s.is_empty()) {
                    b["lyrics"] = json!(lyrics);
                }
                if let Some(dur) = args["duration"].as_u64() {
                    b["duration"] = json!(dur);
                }
                ("/v1/audio/music", b)
            }
        };

        let url = format!("{}{endpoint}", host.trim_end_matches('/'));
        let resp = client
            .post(&url)
            .bearer_auth(&api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("audio_gen: request failed: {e}"))?;
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();

        if status.as_u16() == 504 {
            return Ok(json!({
                "error": "audio_gen: the synchronous window timed out (504). The job is still running server-side; retry shortly."
            }));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| anyhow!("audio_gen: read body: {e}"))?;
        if !status.is_success() {
            let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let raw = String::from_utf8_lossy(&bytes);
            let msg = v
                .pointer("/error/message")
                .and_then(|x| x.as_str())
                .or_else(|| v.get("message").and_then(|x| x.as_str()))
                .unwrap_or_else(|| rsclaw_util::truncate_str(&raw, 200));
            return Err(anyhow!("audio_gen: rsclaw API {status}: {msg}"));
        }

        // Success. The body is normally raw audio bytes, but tolerate a JSON
        // envelope `{ "url" | "audio_url" | data[0].url }` from a gateway by
        // downloading the referenced asset.
        let audio_bytes: Vec<u8> = if content_type.contains("json")
            || (!content_type.starts_with("audio/")
                && serde_json::from_slice::<Value>(&bytes).is_ok())
        {
            let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let asset_url = v["url"]
                .as_str()
                .or_else(|| v["audio_url"].as_str())
                .or_else(|| v.pointer("/data/0/url").and_then(|x| x.as_str()))
                .filter(|s| s.starts_with("http"))
                .ok_or_else(|| {
                    anyhow!(
                        "audio_gen: JSON response without a usable audio url: {}",
                        rsclaw_util::truncate_str(&v.to_string(), 200)
                    )
                })?;
            client
                .get(asset_url)
                .bearer_auth(&api_key)
                .send()
                .await
                .map_err(|e| anyhow!("audio_gen: asset download failed: {e}"))?
                .bytes()
                .await
                .map_err(|e| anyhow!("audio_gen: asset read failed: {e}"))?
                .to_vec()
        } else {
            bytes.to_vec()
        };

        let path = save_generated_audio(&audio_bytes, &fmt).await?;
        Ok(json!({
            "audio_file": path,
            "kind": kind,
            "format": fmt,
            "message": "Audio generated and sent to the user as an attachment."
        }))
    }
}

/// Persist generated audio bytes to `~/Downloads/rsclaw/audios/` with the
/// canonical `dl_a_<YYYYMMDDHHmm><abc>.<ext>` filename and return the absolute
/// path. Mirrors the image/video download naming.
async fn save_generated_audio(bytes: &[u8], fmt: &str) -> Result<String> {
    let ext = match fmt {
        "wav" => "wav",
        "flac" => "flac",
        "opus" => "opus",
        "ogg" => "ogg",
        "m4a" | "aac" => "m4a",
        _ => "mp3",
    };
    let kind = rsclaw_channel::kind_from_extension(ext);
    let category = rsclaw_channel::category_for_kind(kind);
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
        .map_err(|e| anyhow!("audio_gen: create_dir: {e}"))?;
    let ts = chrono::Local::now().format("%Y%m%d%H%M").to_string();
    let abc: String = (0..3)
        .map(|_| (rand::random::<u8>() % 26 + b'a') as char)
        .collect();
    let path = dir.join(format!("dl_{kind}_{ts}{abc}.{ext}"));
    tokio::fs::write(&path, bytes)
        .await
        .map_err(|e| anyhow!("audio_gen: write: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}
