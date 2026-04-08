//! Zalo Official Account API channel.
//!
//! Uses webhook mode: Zalo pushes events to /hooks/zalo.
//! Send replies via the Zalo OA Send Message API.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, info, warn};

use super::{Channel, OutboundMessage};
use crate::channel::chunker::{BreakPreference, ChunkConfig, chunk_text, platform_chunk_limit};

const ZALO_API_BASE: &str = "https://openapi.zalo.me/v3.0/oa";

// ---------------------------------------------------------------------------
// Webhook types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ZaloWebhookBody {
    pub event_name: Option<String>,
    pub sender: Option<ZaloSender>,
    pub message: Option<ZaloMessage>,
}

#[derive(Debug, Deserialize)]
pub struct ZaloSender {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct ZaloMessage {
    pub text: Option<String>,
    pub msg_id: Option<String>,
    pub url: Option<String>,
    #[serde(rename = "attachments")]
    pub attachments: Option<Vec<ZaloAttachment>>,
}

#[derive(Debug, Deserialize)]
pub struct ZaloAttachment {
    pub payload: Option<ZaloAttachmentPayload>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ZaloAttachmentPayload {
    pub url: Option<String>,
    pub name: Option<String>,
}

// ---------------------------------------------------------------------------
// ZaloChannel
// ---------------------------------------------------------------------------

pub struct ZaloChannel {
    access_token: String,
    api_base: String,
    client: Client,
    #[allow(clippy::type_complexity)]
    on_message: Arc<dyn Fn(String, String, Vec<crate::agent::registry::ImageAttachment>) + Send + Sync>,
}

impl ZaloChannel {
    pub fn new(
        access_token: impl Into<String>,
        on_message: Arc<dyn Fn(String, String, Vec<crate::agent::registry::ImageAttachment>) + Send + Sync>,
    ) -> Self {
        Self::with_api_base(access_token, None, on_message)
    }

    pub fn with_api_base(
        access_token: impl Into<String>,
        api_base: Option<String>,
        on_message: Arc<dyn Fn(String, String, Vec<crate::agent::registry::ImageAttachment>) + Send + Sync>,
    ) -> Self {
        Self {
            access_token: access_token.into(),
            api_base: api_base.unwrap_or_else(|| ZALO_API_BASE.to_owned()),
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            on_message,
        }
    }

    /// Handle incoming webhook from Zalo.
    pub async fn handle_webhook(&self, body: &str) -> Result<()> {
        let webhook: ZaloWebhookBody =
            serde_json::from_str(body).context("Zalo: invalid webhook JSON")?;

        let event = webhook.event_name.as_deref().unwrap_or("");
        let sender_id = webhook
            .sender
            .as_ref()
            .map(|s| s.id.clone())
            .unwrap_or_default();

        if sender_id.is_empty() {
            return Ok(());
        }

        let mut text = String::new();
        let mut images: Vec<crate::agent::registry::ImageAttachment> = Vec::new();

        match event {
            "user_send_text" => {
                text = webhook
                    .message
                    .as_ref()
                    .and_then(|m| m.text.clone())
                    .unwrap_or_default();
            }
            "user_send_image" => {
                // Image URL may be in message.url or message.attachments[].payload.url
                let url = webhook
                    .message
                    .as_ref()
                    .and_then(|m| {
                        m.url.as_deref().or_else(|| {
                            m.attachments.as_ref().and_then(|atts| {
                                atts.first()
                                    .and_then(|a| a.payload.as_ref())
                                    .and_then(|p| p.url.as_deref())
                            })
                        })
                    });

                if let Some(url) = url {
                    match crate::channel::transcription::download_file(&self.client, url).await {
                        Ok(bytes) => {
                            use base64::Engine;
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                            images.push(crate::agent::registry::ImageAttachment {
                                data: format!("data:image/jpeg;base64,{b64}"),
                                mime_type: "image/jpeg".to_owned(),
                            });
                            text = crate::i18n::t("describe_image", crate::i18n::default_lang());
                            info!(size = bytes.len(), "Zalo: image downloaded");
                        }
                        Err(e) => {
                            warn!("Zalo: image download failed: {e:#}");
                            return Ok(());
                        }
                    }
                }
            }
            "user_send_audio" => {
                let url = webhook
                    .message
                    .as_ref()
                    .and_then(|m| {
                        m.url.as_deref().or_else(|| {
                            m.attachments.as_ref().and_then(|atts| {
                                atts.first()
                                    .and_then(|a| a.payload.as_ref())
                                    .and_then(|p| p.url.as_deref())
                            })
                        })
                    });

                if let Some(url) = url {
                    match crate::channel::transcription::download_file(&self.client, url).await {
                        Ok(bytes) => {
                            match crate::channel::transcription::transcribe_audio(
                                &self.client, &bytes, "voice.mp3", "audio/mpeg",
                            ).await {
                                Ok(t) if !t.is_empty() => {
                                    info!(chars = t.len(), "Zalo: audio transcribed");
                                    text = t;
                                }
                                Ok(_) => { warn!("Zalo: audio transcription returned empty"); return Ok(()); }
                                Err(e) => { warn!("Zalo: audio transcription failed: {e:#}"); return Ok(()); }
                            }
                        }
                        Err(e) => {
                            warn!("Zalo: audio download failed: {e:#}");
                            return Ok(());
                        }
                    }
                }
            }
            "user_send_video" => {
                let url = webhook
                    .message
                    .as_ref()
                    .and_then(|m| {
                        m.url.as_deref().or_else(|| {
                            m.attachments.as_ref().and_then(|atts| {
                                atts.first()
                                    .and_then(|a| a.payload.as_ref())
                                    .and_then(|p| p.url.as_deref())
                            })
                        })
                    });

                if let Some(url) = url {
                    match crate::channel::transcription::download_file(&self.client, url).await {
                        Ok(bytes) => {
                            match zalo_extract_audio_and_transcribe(&self.client, &bytes).await {
                                Ok(t) if !t.is_empty() => {
                                    info!(chars = t.len(), "Zalo: video audio transcribed");
                                    text = t;
                                }
                                Ok(_) => { warn!("Zalo: video transcription returned empty"); return Ok(()); }
                                Err(e) => { warn!("Zalo: video transcription failed: {e:#}"); return Ok(()); }
                            }
                        }
                        Err(e) => {
                            warn!("Zalo: video download failed: {e:#}");
                            return Ok(());
                        }
                    }
                }
            }
            "user_send_file" => {
                let (url, filename) = webhook
                    .message
                    .as_ref()
                    .map(|m| {
                        let u = m.url.as_deref().or_else(|| {
                            m.attachments.as_ref().and_then(|atts| {
                                atts.first()
                                    .and_then(|a| a.payload.as_ref())
                                    .and_then(|p| p.url.as_deref())
                            })
                        });
                        let name = m.attachments.as_ref().and_then(|atts| {
                            atts.first()
                                .and_then(|a| a.payload.as_ref())
                                .and_then(|p| p.name.as_deref())
                        }).unwrap_or("file");
                        (u, name)
                    })
                    .unwrap_or((None, "file"));

                if let Some(url) = url {
                    match crate::channel::transcription::download_file(&self.client, url).await {
                        Ok(bytes) => {
                            if is_text_file(filename) {
                                if let Ok(content) = String::from_utf8(bytes) {
                                    text = format!("[File: {filename}]\n{content}");
                                    info!(name = filename, "Zalo: text file received");
                                }
                            } else {
                                debug!("Zalo: non-text file ignored: {filename}");
                                return Ok(());
                            }
                        }
                        Err(e) => {
                            warn!("Zalo: file download failed: {e:#}");
                            return Ok(());
                        }
                    }
                }
            }
            _ => {
                debug!(event, "Zalo: skipping unsupported event");
                return Ok(());
            }
        }

        if text.is_empty() && images.is_empty() {
            return Ok(());
        }

        info!(from = %sender_id, text_len = text.len(), "Zalo: message received");
        (self.on_message)(sender_id, text, images);
        Ok(())
    }

    async fn send_text(&self, user_id: &str, text: &str) -> Result<()> {
        let url = format!("{}/message/cs", self.api_base);
        let body = json!({
            "recipient": { "user_id": user_id },
            "message": { "text": text }
        });
        let resp = self
            .client
            .post(&url)
            .header("access_token", &self.access_token)
            .json(&body)
            .send()
            .await
            .context("Zalo: send message failed")?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Zalo: send failed: {body}");
        }
        Ok(())
    }
}

impl Channel for ZaloChannel {
    fn name(&self) -> &str {
        "zalo"
    }

    fn send(&self, msg: OutboundMessage) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let cfg = ChunkConfig {
                max_chars: platform_chunk_limit("zalo"),
                min_chars: 1,
                break_preference: BreakPreference::Paragraph,
            };
            for chunk in &chunk_text(&msg.text, &cfg) {
                self.send_text(&msg.target_id, chunk).await?;
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
                    warn!("Zalo: unrecognised image data URI prefix, skipping");
                    continue;
                };

                let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
                    Ok(b) if !b.is_empty() => b,
                    _ => {
                        warn!("Zalo: failed to decode base64 image, skipping");
                        continue;
                    }
                };

                let filename = if mime == "image/jpeg" { "image.jpg" } else { "image.png" };

                // Upload image to Zalo OA.
                let upload_url = format!("{}/upload/image", self.api_base);
                let part = match reqwest::multipart::Part::bytes(bytes)
                    .file_name(filename)
                    .mime_str(mime)
                {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("Zalo: failed to build multipart part: {e}");
                        continue;
                    }
                };
                let form = reqwest::multipart::Form::new().part("file", part);
                let upload_resp = self
                    .client
                    .post(&upload_url)
                    .header("access_token", &self.access_token)
                    .multipart(form)
                    .send()
                    .await;

                let attachment_id = match upload_resp {
                    Ok(r) => match r.json::<serde_json::Value>().await {
                        Ok(body) => {
                            if let Some(id) = body
                                .get("data")
                                .and_then(|d| d.get("attachment_id"))
                                .and_then(|v| v.as_str())
                            {
                                id.to_owned()
                            } else {
                                warn!("Zalo: upload response missing attachment_id: {body}");
                                continue;
                            }
                        }
                        Err(e) => {
                            warn!("Zalo: failed to parse upload response: {e}");
                            continue;
                        }
                    },
                    Err(e) => {
                        warn!("Zalo: image upload request failed: {e}");
                        continue;
                    }
                };

                // Send image message using attachment_id.
                let send_url = format!("{}/message/cs", self.api_base);
                let body = json!({
                    "recipient": { "user_id": msg.target_id },
                    "message": {
                        "attachment": {
                            "type": "template",
                            "payload": {
                                "template_type": "media",
                                "elements": [{
                                    "media_type": "image",
                                    "attachment_id": attachment_id,
                                }]
                            }
                        }
                    }
                });
                match self
                    .client
                    .post(&send_url)
                    .header("access_token", &self.access_token)
                    .json(&body)
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => {
                        debug!("Zalo: image sent successfully");
                    }
                    Ok(r) => {
                        let status = r.status();
                        let err = r.text().await.unwrap_or_default();
                        warn!("Zalo: image send failed {status}: {err}");
                    }
                    Err(e) => {
                        warn!("Zalo: image send request failed: {e}");
                    }
                }
            }

            Ok(())
        })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            info!("Zalo channel running (webhook mode -- no polling loop)");
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
async fn zalo_extract_audio_and_transcribe(
    client: &Client,
    video_bytes: &[u8],
) -> anyhow::Result<String> {
    let tmp_dir = std::env::temp_dir();
    let video_path = tmp_dir.join(format!("rsclaw_zalo_video_{}.mp4", uuid::Uuid::new_v4()));
    let audio_path = tmp_dir.join(format!("rsclaw_zalo_video_{}.ogg", uuid::Uuid::new_v4()));

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
        let ch = ZaloChannel::new("token", Arc::new(|_, _, _| {}));
        assert_eq!(ch.name(), "zalo");
    }

    #[test]
    fn handle_webhook_dispatches_text() {
        init_crypto();
        use std::sync::Mutex;
        let received: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(vec![]));
        let rx = Arc::clone(&received);

        let ch = ZaloChannel::new(
            "token",
            Arc::new(move |from, text, _images| {
                rx.lock().expect("lock").push((from, text));
            }),
        );

        let body = r#"{
            "event_name": "user_send_text",
            "sender": { "id": "Z12345" },
            "message": { "text": "xin chao", "msg_id": "m1" }
        }"#;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(ch.handle_webhook(body)).unwrap();

        let msgs = received.lock().expect("lock");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].0, "Z12345");
        assert_eq!(msgs[0].1, "xin chao");
    }
}
