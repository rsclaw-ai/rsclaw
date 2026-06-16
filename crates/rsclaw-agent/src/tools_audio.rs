//! Shared audio-generation base for `music_gen` (`tools_music.rs`) and
//! `voice_gen` (`tools_voice.rs`) on the rsclaw gen surface (gen-api.md §4/§5).
//!
//! Both endpoints are SYNCHRONOUS: the HTTP response body IS the audio bytes
//! (`Content-Type: audio/*`). So — unlike video / avatar / mv — there's no
//! `ExternalJob` polling. We POST, save the returned bytes, and return an
//! `audio_file` path that the agent reply boundary auto-attaches.
//!
//! This file holds the pieces music + voice share: the `audio_submit` POST/
//! save path, the output-format helper, and the canonical on-disk naming.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

impl super::runtime::AgentRuntime {
    /// Shared sync submit for the audio endpoints: POST, handle the bytes /
    /// JSON-envelope / 504 cases, save to disk, return an `audio_file` path.
    pub(crate) async fn audio_submit(
        &self,
        endpoint: &str,
        body: &Value,
        fmt: &str,
        kind: &str,
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
                    "{kind}_gen: no API key for rsclaw. Set `model.models.providers.rsclaw.apiKey` in rsclaw.json5 or export RSCLAW_API_KEY, then retry."
                )
            })?;
        let host = rsclaw_provider::rsclaw_http::gen_host_base(None);
        let ua = self
            .config
            .gateway
            .user_agent
            .as_deref()
            .unwrap_or(rsclaw_provider::DEFAULT_USER_AGENT);

        let url = format!("{}{endpoint}", host.trim_end_matches('/'));
        // MUST go through the rsclaw_http redirect helper: the gen LB answers
        // with a 308 to a different origin, and reqwest's default redirect
        // policy STRIPS the Authorization header across origins → the upstream
        // sees "missing Bearer". post_json re-attaches the bearer on each hop.
        // Music synthesis can run well past a typical request; 180s timeout.
        let client = rsclaw_provider::rsclaw_http::build_client(ua, 180)
            .map_err(|e| anyhow!("{kind}_gen: client: {e}"))?;
        let resp = rsclaw_provider::rsclaw_http::post_json(&client, &url, &api_key, body)
            .await
            .map_err(|e| anyhow!("{kind}_gen: request failed: {e}"))?;
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();

        if status.as_u16() == 504 {
            return Ok(json!({
                "error": format!("{kind}_gen: the synchronous window timed out (504). The job is still running server-side; retry shortly.")
            }));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| anyhow!("{kind}_gen: read body: {e}"))?;
        if !status.is_success() {
            let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let raw = String::from_utf8_lossy(&bytes);
            let msg = v
                .pointer("/error/message")
                .and_then(|x| x.as_str())
                .or_else(|| v.get("message").and_then(|x| x.as_str()))
                .unwrap_or_else(|| rsclaw_util::truncate_str(&raw, 200));
            return Err(anyhow!("{kind}_gen: rsclaw API {status}: {msg}"));
        }

        // Success. The body is normally raw audio bytes, but tolerate a JSON
        // envelope `{ "url" | "audio_url" | data[0].url }` by downloading the
        // referenced asset.
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
                        "{kind}_gen: JSON response without a usable audio url: {}",
                        rsclaw_util::truncate_str(&v.to_string(), 200)
                    )
                })?;
            reqwest::Client::new()
                .get(asset_url)
                .send()
                .await
                .map_err(|e| anyhow!("{kind}_gen: asset download failed: {e}"))?
                .bytes()
                .await
                .map_err(|e| anyhow!("{kind}_gen: asset read failed: {e}"))?
                .to_vec()
        } else {
            bytes.to_vec()
        };

        // The backend may ignore `response_format` (e.g. music returns WAV even
        // when mp3 was asked). Name the file by the ACTUAL content-type so the
        // extension matches the bytes; fall back to the requested fmt.
        let actual_fmt = match content_type.split(';').next().unwrap_or("").trim() {
            "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
            "audio/mpeg" | "audio/mp3" => "mp3",
            "audio/flac" | "audio/x-flac" => "flac",
            "audio/mp4" | "audio/aac" => "m4a",
            "audio/ogg" | "audio/opus" => "ogg",
            _ => fmt,
        };
        let path = save_generated_audio(&audio_bytes, actual_fmt).await?;
        Ok(json!({
            "audio_file": path,
            "kind": kind,
            "format": actual_fmt,
            "message": "Audio generated and sent to the user as an attachment."
        }))
    }
}

/// Resolve the output container — default mp3 for IM-platform compatibility
/// (feishu/weixin won't render ogg/opus inline), overridable.
pub(crate) fn audio_format(args: &Value) -> String {
    args["response_format"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("mp3")
        .to_owned()
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
