//! WhatsApp channel skeleton.
//!
//! Supports the WhatsApp Business Cloud API (Meta/Facebook).
//! Full webhook + send implementation; polling is not available
//! (WhatsApp uses webhooks only).
//!
//! Outbound: `POST /messages` with type=text.
//! Inbound:  webhook POST to `/hooks/whatsapp` → parse → dispatch.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use futures::future::BoxFuture;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, info, warn};

use super::{Channel, OutboundMessage};
use crate::channel::chunker::{BreakPreference, ChunkConfig, chunk_text, platform_chunk_limit};

const WHATSAPP_API_BASE: &str = "https://graph.facebook.com/v19.0";

// ---------------------------------------------------------------------------
// WhatsApp Cloud API types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct WebhookPayload {
    pub entry: Vec<WhatsAppEntry>,
}

#[derive(Debug, Deserialize)]
pub struct WhatsAppEntry {
    pub changes: Vec<WhatsAppChange>,
}

#[derive(Debug, Deserialize)]
pub struct WhatsAppChange {
    pub value: WhatsAppValue,
}

#[derive(Debug, Deserialize)]
pub struct WhatsAppValue {
    pub messages: Option<Vec<WhatsAppMessage>>,
}

#[derive(Debug, Deserialize)]
pub struct WhatsAppMessage {
    pub from: String,
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub text: Option<WhatsAppText>,
    pub image: Option<WhatsAppMediaRef>,
    pub audio: Option<WhatsAppMediaRef>,
    pub video: Option<WhatsAppMediaRef>,
    pub document: Option<WhatsAppMediaRef>,
}

#[derive(Debug, Deserialize)]
pub struct WhatsAppText {
    pub body: String,
}

#[derive(Debug, Deserialize)]
pub struct WhatsAppMediaRef {
    pub id: String,
    pub mime_type: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
}

// ---------------------------------------------------------------------------
// WhatsAppChannel
// ---------------------------------------------------------------------------

pub struct WhatsAppChannel {
    phone_number_id: String,
    access_token: String,
    api_base: String,
    client: Client,
    #[allow(clippy::type_complexity)]
    on_message: Arc<dyn Fn(String, String, Vec<crate::agent::registry::ImageAttachment>) + Send + Sync>,
    // (from_number, text, images)
}

impl WhatsAppChannel {
    pub fn new(
        phone_number_id: impl Into<String>,
        access_token: impl Into<String>,
        on_message: Arc<dyn Fn(String, String, Vec<crate::agent::registry::ImageAttachment>) + Send + Sync>,
    ) -> Self {
        Self::with_api_base(phone_number_id, access_token, None, on_message)
    }

    pub fn with_api_base(
        phone_number_id: impl Into<String>,
        access_token: impl Into<String>,
        api_base: Option<String>,
        on_message: Arc<dyn Fn(String, String, Vec<crate::agent::registry::ImageAttachment>) + Send + Sync>,
    ) -> Self {
        Self {
            phone_number_id: phone_number_id.into(),
            access_token: access_token.into(),
            api_base: api_base.unwrap_or_else(|| WHATSAPP_API_BASE.to_owned()),
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            on_message,
        }
    }

    pub async fn send_text(&self, to: &str, text: &str) -> Result<()> {
        let body = json!({
            "messaging_product": "whatsapp",
            "to":   to,
            "type": "text",
            "text": { "body": text },
        });

        let resp = self
            .client
            .post(format!(
                "{}/{}/messages",
                self.api_base,
                self.phone_number_id
            ))
            .bearer_auth(&self.access_token)
            .json(&body)
            .send()
            .await
            .context("WhatsApp send")?;

        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            bail!("WhatsApp send failed: {err}");
        }

        Ok(())
    }

    /// Parse and dispatch an inbound webhook payload.
    pub async fn handle_webhook(&self, payload: &WebhookPayload) {
        for entry in &payload.entry {
            for change in &entry.changes {
                if let Some(messages) = &change.value.messages {
                    for msg in messages {
                        let mut text = String::new();
                        let mut images: Vec<crate::agent::registry::ImageAttachment> = Vec::new();

                        match msg.kind.as_str() {
                            "text" => {
                                if let Some(t) = &msg.text {
                                    text = t.body.clone();
                                }
                            }
                            "image" => {
                                if let Some(ref media) = msg.image {
                                    match self.download_whatsapp_media(&media.id).await {
                                        Ok(bytes) => {
                                            let mime = media.mime_type.as_deref().unwrap_or("image/jpeg");
                                            use base64::Engine;
                                            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                                            images.push(crate::agent::registry::ImageAttachment {
                                                data: format!("data:{mime};base64,{b64}"),
                                                mime_type: mime.to_owned(),
                                            });
                                            text = crate::i18n::t("describe_image", crate::i18n::default_lang());
                                            info!(size = bytes.len(), "WhatsApp image downloaded");
                                        }
                                        Err(e) => {
                                            warn!("WhatsApp image download failed: {e:#}");
                                            continue;
                                        }
                                    }
                                }
                            }
                            "audio" => {
                                if let Some(ref media) = msg.audio {
                                    match self.download_whatsapp_media(&media.id).await {
                                        Ok(bytes) => {
                                            let mime = media.mime_type.as_deref().unwrap_or("audio/ogg");
                                            match crate::channel::transcription::transcribe_audio(
                                                &self.client, &bytes, "voice.ogg", mime,
                                            ).await {
                                                Ok(t) if !t.is_empty() => {
                                                    info!(chars = t.len(), "WhatsApp voice transcribed");
                                                    text = t;
                                                }
                                                Ok(_) => { warn!("WhatsApp voice transcription returned empty"); continue; }
                                                Err(e) => { warn!("WhatsApp voice transcription failed: {e:#}"); continue; }
                                            }
                                        }
                                        Err(e) => {
                                            warn!("WhatsApp audio download failed: {e:#}");
                                            continue;
                                        }
                                    }
                                }
                            }
                            "video" => {
                                if let Some(ref media) = msg.video {
                                    match self.download_whatsapp_media(&media.id).await {
                                        Ok(bytes) => {
                                            match whatsapp_extract_audio_and_transcribe(&self.client, &bytes).await {
                                                Ok(t) if !t.is_empty() => {
                                                    info!(chars = t.len(), "WhatsApp video audio transcribed");
                                                    text = t;
                                                }
                                                Ok(_) => { warn!("WhatsApp video transcription returned empty"); continue; }
                                                Err(e) => { warn!("WhatsApp video transcription failed: {e:#}"); continue; }
                                            }
                                        }
                                        Err(e) => {
                                            warn!("WhatsApp video download failed: {e:#}");
                                            continue;
                                        }
                                    }
                                }
                            }
                            "document" => {
                                if let Some(ref media) = msg.document {
                                    let filename = media.filename.as_deref().unwrap_or("file");
                                    match self.download_whatsapp_media(&media.id).await {
                                        Ok(bytes) => {
                                            if is_text_file(filename) {
                                                if let Ok(content) = String::from_utf8(bytes) {
                                                    text = format!("[File: {filename}]\n{content}");
                                                    info!(name = filename, "WhatsApp text file received");
                                                }
                                            } else {
                                                debug!("WhatsApp: non-text document ignored: {filename}");
                                                continue;
                                            }
                                        }
                                        Err(e) => {
                                            warn!("WhatsApp document download failed: {e:#}");
                                            continue;
                                        }
                                    }
                                }
                            }
                            _ => {
                                debug!(kind = %msg.kind, "WhatsApp: skipping unsupported message type");
                                continue;
                            }
                        }

                        if !text.is_empty() || !images.is_empty() {
                            (self.on_message)(msg.from.clone(), text, images);
                        }
                    }
                }
            }
        }
    }

    /// Download media from WhatsApp Cloud API.
    /// First GET the media URL, then download the actual file.
    async fn download_whatsapp_media(&self, media_id: &str) -> Result<Vec<u8>> {
        // Step 1: Get the media URL
        let meta_url = format!("{}/{media_id}", self.api_base);
        let resp = self
            .client
            .get(&meta_url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .context("WhatsApp media metadata request")?;

        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            bail!("WhatsApp media metadata failed: {err}");
        }

        let meta: serde_json::Value = resp.json().await.context("WhatsApp media metadata parse")?;
        let download_url = meta
            .get("url")
            .and_then(|v| v.as_str())
            .context("WhatsApp media: no url in metadata")?;

        // Step 2: Download the actual file
        let resp = self
            .client
            .get(download_url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .context("WhatsApp media download")?;

        if !resp.status().is_success() {
            bail!("WhatsApp media download failed: {}", resp.status());
        }

        Ok(resp.bytes().await?.to_vec())
    }
}

impl Channel for WhatsAppChannel {
    fn name(&self) -> &str {
        "whatsapp"
    }

    fn send(&self, msg: OutboundMessage) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let cfg = ChunkConfig {
                max_chars: platform_chunk_limit("whatsapp"),
                min_chars: 1,
                break_preference: BreakPreference::Paragraph,
            };
            for chunk in &chunk_text(&msg.text, &cfg) {
                self.send_text(&msg.target_id, chunk).await?;
            }
            for image_data in &msg.images {
                // WhatsApp Cloud API: first upload media, then send.
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
                    warn!("whatsapp: unrecognised image data URI prefix, skipping");
                    continue;
                };

                let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
                    Ok(b) if !b.is_empty() => b,
                    _ => {
                        warn!("whatsapp: failed to decode base64 image, skipping");
                        continue;
                    }
                };

                let filename = if mime == "image/jpeg" { "image.jpg" } else { "image.png" };

                // Upload media.
                let part = match reqwest::multipart::Part::bytes(bytes)
                    .file_name(filename)
                    .mime_str(mime)
                {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("whatsapp: failed to build multipart part: {e}");
                        continue;
                    }
                };
                let form = reqwest::multipart::Form::new()
                    .text("messaging_product", "whatsapp")
                    .text("type", mime)
                    .part("file", part);
                let upload_url = format!(
                    "{}/{}/media",
                    self.api_base,
                    self.phone_number_id
                );
                let upload_resp = self
                    .client
                    .post(&upload_url)
                    .bearer_auth(&self.access_token)
                    .multipart(form)
                    .send()
                    .await;

                let media_id = match upload_resp {
                    Ok(r) => match r.json::<serde_json::Value>().await {
                        Ok(body) => {
                            if let Some(id) = body.get("id").and_then(|v| v.as_str()) {
                                id.to_owned()
                            } else {
                                warn!("whatsapp: media upload response missing id: {body}");
                                continue;
                            }
                        }
                        Err(e) => {
                            warn!("whatsapp: failed to parse media upload response: {e}");
                            continue;
                        }
                    },
                    Err(e) => {
                        warn!("whatsapp: media upload request failed: {e}");
                        continue;
                    }
                };

                // Send image message.
                let send_url = format!(
                    "{}/{}/messages",
                    self.api_base,
                    self.phone_number_id
                );
                match self
                    .client
                    .post(&send_url)
                    .bearer_auth(&self.access_token)
                    .json(&json!({
                        "messaging_product": "whatsapp",
                        "to": msg.target_id,
                        "type": "image",
                        "image": { "id": media_id }
                    }))
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => {
                        debug!("whatsapp: image message sent");
                    }
                    Ok(r) => {
                        let status = r.status();
                        let err = r.text().await.unwrap_or_default();
                        warn!("whatsapp: image send failed {status}: {err}");
                    }
                    Err(e) => {
                        warn!("whatsapp: image send request failed: {e}");
                    }
                }
            }
            Ok(())
        })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            info!("WhatsApp channel running (webhook mode — no polling loop)");
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
async fn whatsapp_extract_audio_and_transcribe(
    client: &Client,
    video_bytes: &[u8],
) -> Result<String> {
    let tmp_dir = std::env::temp_dir();
    let video_path = tmp_dir.join(format!("rsclaw_wa_video_{}.mp4", uuid::Uuid::new_v4()));
    let audio_path = tmp_dir.join(format!("rsclaw_wa_video_{}.ogg", uuid::Uuid::new_v4()));

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

    #[test]
    fn channel_name() {
        let ch = WhatsAppChannel::new("123", "token", Arc::new(|_, _, _| {}));
        assert_eq!(ch.name(), "whatsapp");
    }

    #[test]
    fn handle_webhook_dispatches_text() {
        use std::sync::Mutex;
        let received: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(vec![]));
        let rx = Arc::clone(&received);

        let ch = WhatsAppChannel::new(
            "123",
            "token",
            Arc::new(move |from, text, _images| {
                rx.lock().expect("lock").push((from, text));
            }),
        );

        let payload = WebhookPayload {
            entry: vec![WhatsAppEntry {
                changes: vec![WhatsAppChange {
                    value: WhatsAppValue {
                        messages: Some(vec![WhatsAppMessage {
                            from: "447911123456".to_owned(),
                            id: "wamid.xxx".to_owned(),
                            kind: "text".to_owned(),
                            text: Some(WhatsAppText {
                                body: "hello".to_owned(),
                            }),
                            image: None,
                            audio: None,
                            video: None,
                            document: None,
                        }]),
                    },
                }],
            }],
        };

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(ch.handle_webhook(&payload));

        let msgs = received.lock().expect("lock");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].0, "447911123456");
        assert_eq!(msgs[0].1, "hello");
    }
}
