//! LINE Messaging API channel.
//!
//! Uses webhook mode: LINE pushes events to /hooks/line.
//! Send replies via the LINE Push/Reply Message API.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, info, warn};

use super::{Channel, OutboundMessage};
use crate::channel::chunker::{BreakPreference, ChunkConfig, chunk_text, platform_chunk_limit};

const LINE_API_BASE: &str = "https://api.line.me/v2/bot";
const LINE_API_DATA_BASE: &str = "https://api-data.line.me/v2/bot";

// ---------------------------------------------------------------------------
// Webhook event types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LineWebhookBody {
    pub events: Vec<LineEvent>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub reply_token: Option<String>,
    pub source: Option<LineSource>,
    pub message: Option<LineMessage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineSource {
    #[serde(rename = "type")]
    pub source_type: String,
    pub user_id: Option<String>,
    pub group_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineMessage {
    #[serde(rename = "type")]
    pub message_type: String,
    pub id: String,
    pub text: Option<String>,
    pub file_name: Option<String>,
}

// ---------------------------------------------------------------------------
// LineChannel
// ---------------------------------------------------------------------------

pub struct LineChannel {
    channel_access_token: String,
    api_base: String,
    api_data_base: String,
    client: Client,
    #[allow(clippy::type_complexity)]
    on_message: Arc<dyn Fn(String, String, bool, Vec<crate::agent::registry::ImageAttachment>) + Send + Sync>,
}

impl LineChannel {
    pub fn new(
        channel_access_token: impl Into<String>,
        on_message: Arc<dyn Fn(String, String, bool, Vec<crate::agent::registry::ImageAttachment>) + Send + Sync>,
    ) -> Self {
        Self::with_api_base(channel_access_token, None, on_message)
    }

    pub fn with_api_base(
        channel_access_token: impl Into<String>,
        api_base: Option<String>,
        on_message: Arc<dyn Fn(String, String, bool, Vec<crate::agent::registry::ImageAttachment>) + Send + Sync>,
    ) -> Self {
        let base = api_base.unwrap_or_else(|| LINE_API_BASE.to_owned());
        // Derive data API base by replacing the messaging host.
        let data_base = base.replace("api.line.me", "api-data.line.me");
        // If the replacement had no effect (custom base), fall back to default data base.
        let data_base = if data_base != base {
            data_base
        } else {
            LINE_API_DATA_BASE.to_owned()
        };
        Self {
            channel_access_token: channel_access_token.into(),
            api_base: base,
            api_data_base: data_base,
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            on_message,
        }
    }

    /// Handle incoming webhook from LINE.
    pub async fn handle_webhook(&self, body: &str) -> Result<()> {
        let webhook: LineWebhookBody =
            serde_json::from_str(body).context("LINE: invalid webhook JSON")?;

        for event in &webhook.events {
            if event.event_type != "message" {
                continue;
            }
            let source = match &event.source {
                Some(s) => s,
                None => continue,
            };
            let msg = match &event.message {
                Some(m) => m,
                None => continue,
            };

            let user_id = source.user_id.as_deref().unwrap_or("").to_owned();
            let is_group = source.source_type == "group";

            let mut text = String::new();
            let mut images: Vec<crate::agent::registry::ImageAttachment> = Vec::new();

            match msg.message_type.as_str() {
                "text" => {
                    text = msg.text.as_deref().unwrap_or("").to_owned();
                    if text.is_empty() {
                        continue;
                    }
                }
                "image" => {
                    match self.download_line_content(&msg.id).await {
                        Ok(bytes) => {
                            use base64::Engine;
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                            images.push(crate::agent::registry::ImageAttachment {
                                data: format!("data:image/jpeg;base64,{b64}"),
                                mime_type: "image/jpeg".to_owned(),
                            });
                            text = crate::i18n::t("describe_image", crate::i18n::default_lang());
                            info!(size = bytes.len(), "LINE: image downloaded");
                        }
                        Err(e) => {
                            warn!("LINE: image download failed: {e:#}");
                            continue;
                        }
                    }
                }
                "audio" => {
                    match self.download_line_content(&msg.id).await {
                        Ok(bytes) => {
                            match crate::channel::transcription::transcribe_audio(
                                &self.client, &bytes, "voice.m4a", "audio/mp4",
                            ).await {
                                Ok(t) if !t.is_empty() => {
                                    info!(chars = t.len(), "LINE: audio transcribed");
                                    text = t;
                                }
                                Ok(_) => { warn!("LINE: audio transcription returned empty"); continue; }
                                Err(e) => { warn!("LINE: audio transcription failed: {e:#}"); continue; }
                            }
                        }
                        Err(e) => {
                            warn!("LINE: audio download failed: {e:#}");
                            continue;
                        }
                    }
                }
                "video" => {
                    match self.download_line_content(&msg.id).await {
                        Ok(bytes) => {
                            match line_extract_audio_and_transcribe(&self.client, &bytes).await {
                                Ok(t) if !t.is_empty() => {
                                    info!(chars = t.len(), "LINE: video audio transcribed");
                                    text = t;
                                }
                                Ok(_) => { warn!("LINE: video transcription returned empty"); continue; }
                                Err(e) => { warn!("LINE: video transcription failed: {e:#}"); continue; }
                            }
                        }
                        Err(e) => {
                            warn!("LINE: video download failed: {e:#}");
                            continue;
                        }
                    }
                }
                "file" => {
                    let filename = msg.file_name.as_deref().unwrap_or("file");
                    match self.download_line_content(&msg.id).await {
                        Ok(bytes) => {
                            if is_text_file(filename) {
                                if let Ok(content) = String::from_utf8(bytes) {
                                    text = format!("[File: {filename}]\n{content}");
                                    info!(name = filename, "LINE: text file received");
                                }
                            } else {
                                debug!("LINE: non-text file ignored: {filename}");
                                continue;
                            }
                        }
                        Err(e) => {
                            warn!("LINE: file download failed: {e:#}");
                            continue;
                        }
                    }
                }
                other => {
                    debug!(msg_type = other, "LINE: skipping unsupported message type");
                    continue;
                }
            }

            if text.is_empty() && images.is_empty() {
                continue;
            }

            info!(from = %user_id, text_len = text.len(), "LINE: message received");
            (self.on_message)(user_id, text, is_group, images);
        }
        Ok(())
    }

    /// Download content from LINE Content API.
    async fn download_line_content(&self, message_id: &str) -> Result<Vec<u8>> {
        let url = format!("{}/message/{message_id}/content", self.api_data_base);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.channel_access_token)
            .send()
            .await
            .context("LINE: content download request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            anyhow::bail!("LINE: content download failed {status}: {err}");
        }

        Ok(resp.bytes().await?.to_vec())
    }

    async fn send_push(&self, to: &str, text: &str) -> Result<()> {
        let url = format!("{}/message/push", self.api_base);
        let body = json!({
            "to": to,
            "messages": [{ "type": "text", "text": text }]
        });
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.channel_access_token)
            .json(&body)
            .send()
            .await
            .context("LINE: push message failed")?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("LINE: push failed: {body}");
        }
        Ok(())
    }

    async fn send_image(&self, to: &str, image_url: &str) -> Result<()> {
        let url = format!("{}/message/push", self.api_base);
        let body = json!({
            "to": to,
            "messages": [{
                "type": "image",
                "originalContentUrl": image_url,
                "previewImageUrl": image_url
            }]
        });
        let _ = self
            .client
            .post(&url)
            .bearer_auth(&self.channel_access_token)
            .json(&body)
            .send()
            .await;
        Ok(())
    }
}

impl Channel for LineChannel {
    fn name(&self) -> &str {
        "line"
    }

    fn send(&self, msg: OutboundMessage) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let cfg = ChunkConfig {
                max_chars: platform_chunk_limit("line"),
                min_chars: 1,
                break_preference: BreakPreference::Paragraph,
            };
            for chunk in &chunk_text(&msg.text, &cfg) {
                self.send_push(&msg.target_id, chunk).await?;
            }

            for image_data in &msg.images {
                use base64::Engine;
                let (mime, b64) = if let Some(rest) =
                    image_data.strip_prefix("data:image/png;base64,")
                {
                    ("image/png", rest)
                } else if let Some(rest) =
                    image_data.strip_prefix("data:image/jpeg;base64,")
                {
                    ("image/jpeg", rest)
                } else if let Some(rest) =
                    image_data.strip_prefix("data:image/webp;base64,")
                {
                    ("image/webp", rest)
                } else {
                    warn!("LINE: unrecognised image data URI prefix, skipping");
                    continue;
                };

                let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
                    Ok(b) if !b.is_empty() => b,
                    _ => {
                        warn!("LINE: failed to decode base64 image, skipping");
                        continue;
                    }
                };

                let filename = if mime == "image/jpeg" { "image.jpg" } else { "image.png" };

                // Upload image via LINE's blob upload API.
                // Returns a blob ID usable as originalContentUrl.
                let upload_url = format!("{}/audienceGroup/upload", self.api_data_base);
                let part = match reqwest::multipart::Part::bytes(bytes)
                    .file_name(filename)
                    .mime_str(mime)
                {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("LINE: failed to build multipart part: {e}");
                        continue;
                    }
                };
                let form = reqwest::multipart::Form::new().part("file", part);
                let upload_resp = self
                    .client
                    .post(upload_url)
                    .bearer_auth(&self.channel_access_token)
                    .multipart(form)
                    .send()
                    .await;

                match upload_resp {
                    Ok(r) if r.status().is_success() => {
                        // LINE's audience group upload returns a blob resource URL.
                        if let Ok(body) = r.json::<serde_json::Value>().await {
                            if let Some(blob_url) = body
                                .get("url")
                                .and_then(|v| v.as_str())
                            {
                                let blob_owned = blob_url.to_owned();
                                let _ = self.send_image(&msg.target_id, &blob_owned).await;
                                debug!("LINE: image sent via blob upload");
                                continue;
                            }
                        }
                        warn!("LINE: blob upload response missing url field");
                    }
                    Ok(r) => {
                        let status = r.status();
                        let err = r.text().await.unwrap_or_default();
                        warn!("LINE: image blob upload failed {status}: {err}");
                    }
                    Err(e) => {
                        warn!("LINE: image blob upload request failed: {e}");
                    }
                }
            }

            Ok(())
        })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            info!("LINE channel running (webhook mode -- no polling loop)");
            std::future::pending::<()>().await;
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_text_file(name: &str) -> bool {
    let exts = [
        ".txt", ".md", ".csv", ".json", ".toml", ".yaml", ".yml", ".xml", ".html", ".rs", ".py",
        ".js", ".ts", ".go", ".sh", ".log", ".conf", ".cfg",
    ];
    exts.iter().any(|e| name.ends_with(e))
}

/// Extract audio track from video bytes via ffmpeg, then transcribe.
async fn line_extract_audio_and_transcribe(
    client: &Client,
    video_bytes: &[u8],
) -> anyhow::Result<String> {
    let tmp_dir = std::env::temp_dir();
    let video_path = tmp_dir.join(format!("rsclaw_line_video_{}.mp4", uuid::Uuid::new_v4()));
    let audio_path = tmp_dir.join(format!("rsclaw_line_video_{}.ogg", uuid::Uuid::new_v4()));

    std::fs::write(&video_path, video_bytes)?;

    let status = tokio::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-i",
            video_path.to_str().unwrap_or(""),
            "-vn",
            "-acodec",
            "libopus",
            audio_path.to_str().unwrap_or(""),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;

    let _ = std::fs::remove_file(&video_path);

    if !status.map(|s| s.success()).unwrap_or(false) {
        let _ = std::fs::remove_file(&audio_path);
        anyhow::bail!("ffmpeg failed to extract audio from video");
    }

    let audio_bytes = std::fs::read(&audio_path)?;
    let _ = std::fs::remove_file(&audio_path);

    crate::channel::transcription::transcribe_audio(client, &audio_bytes, "video_audio.ogg", "audio/ogg").await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn init_crypto() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }

    #[test]
    fn channel_name() {
        init_crypto();
        let ch = LineChannel::new("token", Arc::new(|_, _, _, _| {}));
        assert_eq!(ch.name(), "line");
    }

    #[test]
    fn handle_webhook_dispatches_text() {
        init_crypto();
        use std::sync::Mutex;
        let received: Arc<Mutex<Vec<(String, String, bool)>>> = Arc::new(Mutex::new(vec![]));
        let rx = Arc::clone(&received);

        let ch = LineChannel::new(
            "token",
            Arc::new(move |from, text, is_group, _images| {
                rx.lock().expect("lock").push((from, text, is_group));
            }),
        );

        let body = r#"{
            "events": [{
                "type": "message",
                "replyToken": "abc",
                "source": { "type": "user", "userId": "U12345" },
                "message": { "type": "text", "id": "msg1", "text": "hello" }
            }]
        }"#;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(ch.handle_webhook(body)).unwrap();

        let msgs = received.lock().expect("lock");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].0, "U12345");
        assert_eq!(msgs[0].1, "hello");
        assert!(!msgs[0].2);
    }
}
