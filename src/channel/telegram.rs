//! Telegram Bot API channel.
//!
//! Implements long-polling for inbound messages and the sendMessage /
//! editMessageText APIs for outbound replies.
//!
//! Features:
//!   - Long-poll getUpdates loop with configurable timeout.
//!   - Text chunking (4096-char limit).
//!   - Retry with exponential back-off (AGENTS.md §22).
//!   - Preview streaming: partial mode sends an initial message then edits it
//!     with the full reply via editMessageText (agents.md §21).

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use super::{Channel, OutboundMessage};
use crate::channel::chunker::{ChunkConfig, chunk_text, platform_chunk_limit};

// ---------------------------------------------------------------------------
// Telegram API types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TgResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<TgMessage>,
}

#[derive(Debug, Deserialize)]
pub struct TgMessage {
    pub message_id: i64,
    pub from: Option<TgUser>,
    pub chat: TgChat,
    pub text: Option<String>,
    pub message_thread_id: Option<i64>,
    pub voice: Option<TgVoice>,
    pub audio: Option<TgAudio>,
    pub photo: Option<Vec<TgPhotoSize>>,
    pub video: Option<TgVideo>,
    pub document: Option<TgDocument>,
    pub caption: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TgPhotoSize {
    pub file_id: String,
    #[allow(dead_code)]
    pub width: Option<i64>,
    #[allow(dead_code)]
    pub height: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct TgVoice {
    pub file_id: String,
    pub duration: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct TgAudio {
    pub file_id: String,
    pub duration: Option<i64>,
    pub title: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TgVideo {
    pub file_id: String,
    #[allow(dead_code)]
    pub duration: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct TgDocument {
    pub file_id: String,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TgFile {
    pub file_id: String,
    pub file_path: Option<String>,
    pub file_size: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct TgUser {
    pub id: i64,
    pub first_name: String,
    pub username: Option<String>,
}

/// Minimal fields returned by sendMessage (used for preview streaming).
#[derive(Debug, Deserialize)]
struct TgSentMessage {
    pub message_id: i64,
}

#[derive(Debug, Deserialize)]
pub struct TgChat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String, // "private" | "group" | "supergroup" | "channel"
}

// ---------------------------------------------------------------------------
// Retry config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub attempts: u32,
    pub min_delay_ms: u64,
    pub max_delay_ms: u64,
    pub jitter: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            attempts: 3,
            min_delay_ms: 400,
            max_delay_ms: 30_000,
            jitter: 0.1,
        }
    }
}

fn backoff_delay(attempt: u32, config: &RetryConfig) -> Duration {
    let base = config.min_delay_ms as f64 * 2f64.powi(attempt as i32);
    let clamped = base.min(config.max_delay_ms as f64);
    let jitter = clamped * config.jitter * rand::random::<f64>();
    Duration::from_millis((clamped + jitter) as u64)
}

// ---------------------------------------------------------------------------
// TelegramChannel
// ---------------------------------------------------------------------------

pub struct TelegramChannel {
    token: String,
    /// Base URL for all Bot API calls, e.g. "https://api.telegram.org".
    /// Override via `channels.telegram.apiBase` to point at a mock server.
    api_base: String,
    client: Client,
    retry: RetryConfig,
    /// Callback: called with (peer_id, text, chat_id, is_group, thread_id, images, files).
    #[allow(clippy::type_complexity)]
    on_message: Arc<
        dyn Fn(i64, String, i64, bool, Option<i64>, Vec<crate::agent::registry::ImageAttachment>, Vec<crate::agent::registry::FileAttachment>)
            + Send
            + Sync,
    >,
}

impl TelegramChannel {
    #[allow(clippy::type_complexity)]
    pub fn new(
        token: impl Into<String>,
        api_base: Option<String>,
        on_message: Arc<
            dyn Fn(
                    i64,
                    String,
                    i64,
                    bool,
                    Option<i64>,
                    Vec<crate::agent::registry::ImageAttachment>,
                    Vec<crate::agent::registry::FileAttachment>,
                ) + Send
                + Sync,
        >,
    ) -> Self {
        Self {
            token: token.into(),
            api_base: api_base
                .unwrap_or_else(|| "https://api.telegram.org".to_owned()),
            client: crate::config::build_proxy_client()
                .timeout(Duration::from_secs(35))
                .build()
                .expect("reqwest client"),
            retry: RetryConfig::default(),
            on_message,
        }
    }

    fn api_url(&self, method: &str) -> String {
        format!("{}/bot{}/{method}", self.api_base, self.token)
    }

    /// Send a single text chunk to `chat_id`, with optional
    /// `reply_to_message_id`.
    async fn send_message_chunk(
        &self,
        chat_id: i64,
        text: &str,
        reply_to: Option<i64>,
        thread_id: Option<i64>,
    ) -> Result<()> {
        let mut body = json!({
            "chat_id":    chat_id,
            "text":       text,
            "parse_mode": "Markdown",
        });

        if let Some(r) = reply_to {
            body["reply_to_message_id"] = json!(r);
        }
        if let Some(t) = thread_id {
            body["message_thread_id"] = json!(t);
        }

        for attempt in 0..self.retry.attempts {
            let resp = match self
                .client
                .post(self.api_url("sendMessage"))
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    // Retry on transient network/TLS errors.
                    let delay = backoff_delay(attempt, &self.retry);
                    warn!(attempt, ?delay, %e, "Telegram sendMessage network error, retrying");
                    sleep(delay).await;
                    continue;
                }
            };

            let status = resp.status();

            // 429 -- rate limited.
            if status.as_u16() == 429 {
                let delay = backoff_delay(attempt, &self.retry);
                warn!(attempt, ?delay, "Telegram rate limit, backing off");
                sleep(delay).await;
                continue;
            }

            if status.as_u16() == 400 {
                let err = resp.text().await.unwrap_or_default();
                if err.contains("parse entities") || err.contains("can't parse") {
                    // Markdown parse error — retry without parse_mode (plain text).
                    warn!("Telegram: Markdown parse error, retrying as plain text");
                    let mut plain_body = body.clone();
                    plain_body.as_object_mut().map(|o| o.remove("parse_mode"));
                    match self.client.post(self.api_url("sendMessage"))
                        .json(&plain_body).send().await
                    {
                        Ok(r) if r.status().is_success() => return Ok(()),
                        Ok(r) => {
                            let e = r.text().await.unwrap_or_default();
                            return Err(anyhow::anyhow!("sendMessage plain failed: {e}"));
                        }
                        Err(e) => return Err(anyhow::anyhow!("sendMessage plain error: {e}")),
                    }
                }
                return Err(anyhow::anyhow!("sendMessage failed {status}: {err}"));
            }

            if !status.is_success() {
                let err = resp.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!("sendMessage failed {status}: {err}"));
            }

            return Ok(());
        }

        Err(anyhow::anyhow!(
            "sendMessage failed after {} attempts",
            self.retry.attempts
        ))
    }

    /// Send a single text chunk and return the resulting `message_id`.
    /// Used by preview streaming to obtain the ID of the placeholder message.
    async fn send_message_returning_id(
        &self,
        chat_id: i64,
        text: &str,
        reply_to: Option<i64>,
        thread_id: Option<i64>,
    ) -> Result<i64> {
        let mut body = json!({
            "chat_id":    chat_id,
            "text":       text,
            "parse_mode": "Markdown",
        });
        if let Some(r) = reply_to {
            body["reply_to_message_id"] = json!(r);
        }
        if let Some(t) = thread_id {
            body["message_thread_id"] = json!(t);
        }

        for attempt in 0..self.retry.attempts {
            let resp = self
                .client
                .post(self.api_url("sendMessage"))
                .json(&body)
                .send()
                .await
                .context("sendMessage (preview)")?;

            let status = resp.status();
            if status.as_u16() == 429 {
                let delay = backoff_delay(attempt, &self.retry);
                warn!(
                    attempt,
                    ?delay,
                    "Telegram rate limit (preview send), backing off"
                );
                sleep(delay).await;
                continue;
            }
            if !status.is_success() {
                let err = resp.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!("sendMessage failed {status}: {err}"));
            }

            let tg: TgResponse<TgSentMessage> =
                resp.json().await.context("parse sendMessage result")?;
            if !tg.ok {
                return Err(anyhow::anyhow!(
                    "sendMessage not ok: {}",
                    tg.description.unwrap_or_default()
                ));
            }
            let msg_id = tg
                .result
                .ok_or_else(|| anyhow::anyhow!("sendMessage: missing result"))?
                .message_id;
            return Ok(msg_id);
        }

        Err(anyhow::anyhow!(
            "sendMessage (preview) failed after {} attempts",
            self.retry.attempts
        ))
    }

    /// Edit an already-sent message via editMessageText.
    ///
    /// Used by preview streaming (agents.md §21) to update the placeholder
    /// message with the final (or growing) reply text.
    async fn edit_message(&self, chat_id: i64, message_id: i64, new_text: &str) -> Result<()> {
        let body = json!({
            "chat_id":    chat_id,
            "message_id": message_id,
            "text":       new_text,
            "parse_mode": "Markdown",
        });

        for attempt in 0..self.retry.attempts {
            let resp = self
                .client
                .post(self.api_url("editMessageText"))
                .json(&body)
                .send()
                .await
                .context("editMessageText")?;

            let status = resp.status();
            if status.as_u16() == 429 {
                let delay = backoff_delay(attempt, &self.retry);
                warn!(attempt, ?delay, "Telegram rate limit (edit), backing off");
                sleep(delay).await;
                continue;
            }
            if !status.is_success() {
                let err = resp.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!("editMessageText failed {status}: {err}"));
            }
            return Ok(());
        }

        Err(anyhow::anyhow!(
            "editMessageText failed after {} attempts",
            self.retry.attempts
        ))
    }

    /// Preview streaming send (agents.md §21, "partial" mode).
    ///
    /// For replies longer than `PREVIEW_THRESHOLD` characters:
    ///   1. Send an initial placeholder ("…").
    ///   2. Edit the message with the full reply text after a 500 ms pause. (In
    ///      a real streaming scenario the edit would be called repeatedly as
    ///      token deltas arrive; here we have the full text already, so we
    ///      simulate the two-step flow.)
    ///
    /// Shorter replies fall back to the standard single-shot send path.
    pub async fn send_with_preview(
        &self,
        chat_id: i64,
        text: &str,
        reply_to: Option<i64>,
        thread_id: Option<i64>,
    ) -> Result<()> {
        const PREVIEW_THRESHOLD: usize = 200;
        const EDIT_DELAY: Duration = Duration::from_millis(500);

        if text.len() <= PREVIEW_THRESHOLD {
            // Short reply — no need for preview streaming.
            return self
                .send_message_chunk(chat_id, text, reply_to, thread_id)
                .await;
        }

        // Send the placeholder first so the user sees a response immediately.
        let placeholder = "…";
        let msg_id = self
            .send_message_returning_id(chat_id, placeholder, reply_to, thread_id)
            .await?;
        debug!(chat_id, msg_id, "preview: placeholder sent");

        // Simulate streaming delay then edit with the full text.
        sleep(EDIT_DELAY).await;
        self.edit_message(chat_id, msg_id, text).await?;
        debug!(chat_id, msg_id, "preview: message updated with full reply");

        Ok(())
    }

    /// Download a file from Telegram by file_id. Returns the raw bytes.
    async fn download_file(&self, file_id: &str) -> Result<Vec<u8>> {
        // 1. Get file path from Telegram.
        let url = self.api_url("getFile");
        let raw_resp = self
            .client
            .post(&url)
            .json(&json!({ "file_id": file_id }))
            .send()
            .await?;
        let raw_text = raw_resp.text().await?;
        let resp: TgResponse<TgFile> = match serde_json::from_str(&raw_text) {
            Ok(r) => r,
            Err(e) => {
                warn!(file_id = file_id, response = &raw_text[..raw_text.len().min(300)], "Telegram getFile parse error: {e}");
                anyhow::bail!("Telegram getFile parse error");
            }
        };
        if !resp.ok {
            warn!(file_id = file_id, response = &raw_text[..raw_text.len().min(300)], "Telegram getFile failed");
        }

        let file_path = match resp.result {
            Some(f) if f.file_path.is_some() => {
                // SAFETY of expect: guarded by is_some() above
                f.file_path.expect("guarded by is_some")
            }
            Some(_) => {
                warn!(file_id = file_id, "Telegram getFile: no file_path (file may exceed 20MB bot limit)");
                anyhow::bail!("Telegram getFile returned no file_path");
            }
            None => {
                warn!(file_id = file_id, ok = resp.ok, "Telegram getFile: no result");
                anyhow::bail!("Telegram getFile returned no file_path");
            }
        };

        // 2. Download the file.
        let download_url = format!(
            "{}/file/bot{}/{}",
            self.api_base, self.token, file_path
        );
        let bytes = self.client.get(&download_url).send().await?.bytes().await?;

        debug!(size = bytes.len(), path = %file_path, "downloaded file from Telegram");
        Ok(bytes.to_vec())
    }

    /// Download a voice/audio file from Telegram and transcribe it.
    ///
    /// Uses the shared multi-provider transcription module (OpenAI Whisper,
    /// local whisper.cpp, Tencent ASR, or Aliyun ASR — auto-detected).
    async fn transcribe_voice(&self, file_id: &str) -> Result<String> {
        let audio_bytes = self.download_file(file_id).await?;

        // 3. Transcribe via shared multi-provider module.
        crate::channel::transcription::transcribe_audio(
            &self.client,
            &audio_bytes,
            "voice.ogg",
            "audio/ogg",
        )
        .await
    }

    /// Register bot commands with Telegram so they appear in the command menu.
    async fn register_commands(&self) {
        let commands = serde_json::json!({
            "commands": [
                {"command": "help", "description": "Show available commands"},
                {"command": "run", "description": "Execute a shell command"},
                {"command": "search", "description": "Search the web"},
                {"command": "fetch", "description": "Fetch a web page"},
                {"command": "find", "description": "Find files"},
                {"command": "grep", "description": "Search file contents"},
                {"command": "read", "description": "Read a file"},
                {"command": "status", "description": "Gateway status"},
                {"command": "version", "description": "Show version"},
                {"command": "models", "description": "List models"},
                {"command": "clear", "description": "Clear session"},
                {"command": "remember", "description": "Save to memory"},
                {"command": "recall", "description": "Search memory"},
            ]
        });

        let url = self.api_url("setMyCommands");

        match self.client.post(&url).json(&commands).send().await {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!("telegram: bot commands registered");
            }
            Ok(resp) => {
                tracing::warn!("telegram: setMyCommands failed: {}", resp.status());
            }
            Err(e) => {
                tracing::warn!("telegram: setMyCommands error: {e}");
            }
        }
    }

    /// Get updates via long-polling.
    async fn get_updates(&self, offset: i64) -> Result<Vec<Update>> {
        let body = json!({
            "offset":  offset,
            "timeout": 25,
            "allowed_updates": ["message"],
        });

        let resp = self
            .client
            .post(self.api_url("getUpdates"))
            .json(&body)
            .send()
            .await
            .context("getUpdates")?;

        let tg: TgResponse<Vec<Update>> = resp.json().await.context("parse getUpdates")?;

        if !tg.ok {
            return Err(anyhow::anyhow!(
                "getUpdates error: {}",
                tg.description.unwrap_or_default()
            ));
        }

        Ok(tg.result.unwrap_or_default())
    }
}

impl Channel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    fn send(&self, msg: OutboundMessage) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let chat_id: i64 = msg.target_id.parse().context("parse chat_id")?;
            let chunk_cfg = ChunkConfig {
                max_chars: platform_chunk_limit("telegram"),
                min_chars: 1,
                break_preference: super::chunker::BreakPreference::Paragraph,
            };
            if !msg.text.is_empty() {
                let chunks = chunk_text(&msg.text, &chunk_cfg);
                for (i, chunk) in chunks.iter().enumerate() {
                    let reply_to = if i == 0 {
                        msg.reply_to.as_ref().and_then(|r| r.parse::<i64>().ok())
                    } else {
                        None
                    };
                    self.send_message_chunk(chat_id, chunk, reply_to, None)
                        .await?;
                }
            }

            // Send image attachments via sendPhoto
            if !msg.images.is_empty() {
                info!(count = msg.images.len(), "telegram: sending images");
            }
            for (idx, image_data) in msg.images.iter().enumerate() {
                use base64::Engine;
                let b64 = image_data
                    .strip_prefix("data:image/png;base64,")
                    .or_else(|| image_data.strip_prefix("data:image/jpeg;base64,"))
                    .unwrap_or(image_data);
                let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(idx, "telegram: base64 decode failed: {e}");
                        continue;
                    }
                };

                let part = match reqwest::multipart::Part::bytes(bytes)
                    .file_name("image.png")
                    .mime_str("image/png")
                {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(idx, "telegram: build multipart failed: {e}");
                        continue;
                    }
                };
                let form = reqwest::multipart::Form::new()
                    .text("chat_id", msg.target_id.clone())
                    .part("photo", part);

                let url = self.api_url("sendPhoto");
                match self.client.post(&url).multipart(form).send().await {
                    Ok(resp) if !resp.status().is_success() => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        warn!(idx, %status, "telegram: sendPhoto failed: {body}");
                    }
                    Err(e) => warn!(idx, "telegram: sendPhoto request failed: {e}"),
                    Ok(_) => {}
                }
            }

            // Send file attachments via sendDocument
            for (idx, (filename, mime, path_or_url)) in msg.files.iter().enumerate() {
                let bytes = if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
                    match self.client.get(path_or_url.as_str()).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            match resp.bytes().await {
                                Ok(b) if !b.is_empty() => b.to_vec(),
                                _ => { warn!(idx, "telegram: empty file download"); continue; }
                            }
                        }
                        _ => { warn!(idx, "telegram: file download failed: {path_or_url}"); continue; }
                    }
                } else {
                    match std::fs::read(path_or_url) {
                        Ok(b) => b,
                        Err(e) => { warn!(idx, "telegram: failed to read file {path_or_url}: {e}"); continue; }
                    }
                };

                // Audio files: convert to ogg/opus and send as voice message (pure Rust).
                let is_audio = mime.starts_with("audio/");
                let (send_bytes, send_filename, send_mime) = if is_audio && !filename.ends_with(".ogg") && !filename.ends_with(".opus") {
                    let ext = filename.rsplit('.').next().unwrap_or("mp3");
                    match crate::channel::transcription::encode_audio_to_ogg_opus(&bytes, Some(ext)) {
                        Ok(opus_bytes) => {
                            let ogg_name = filename.rsplit_once('.').map(|(n, _)| format!("{n}.ogg")).unwrap_or_else(|| format!("{filename}.ogg"));
                            info!(idx, src_len = bytes.len(), opus_len = opus_bytes.len(), "telegram: converted audio to ogg-opus");
                            (opus_bytes, ogg_name, "audio/ogg".to_owned())
                        }
                        Err(e) => {
                            warn!(idx, "telegram: ogg-opus conversion failed, sending as-is: {e:#}");
                            (bytes, filename.clone(), mime.clone())
                        }
                    }
                } else {
                    (bytes, filename.clone(), mime.clone())
                };

                let part = match reqwest::multipart::Part::bytes(send_bytes)
                    .file_name(send_filename.clone())
                    .mime_str(&send_mime)
                {
                    Ok(p) => p,
                    Err(e) => { warn!(idx, "telegram: build multipart failed: {e}"); continue; }
                };

                // Audio: sendVoice (voice bubble), others: sendDocument (file).
                let (api_method, field_name) = if is_audio {
                    ("sendVoice", "voice")
                } else {
                    ("sendDocument", "document")
                };
                let form = reqwest::multipart::Form::new()
                    .text("chat_id", msg.target_id.clone())
                    .part(field_name, part);

                let url = self.api_url(api_method);
                match self.client.post(&url).multipart(form).send().await {
                    Ok(resp) if !resp.status().is_success() => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        warn!(idx, %status, "telegram: {api_method} failed: {body}");
                    }
                    Err(e) => warn!(idx, "telegram: {api_method} request failed: {e}"),
                    Ok(_) => debug!(idx, "telegram: file sent via {api_method}: {send_filename}"),
                }
            }

            Ok(())
        })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            self.register_commands().await;
            info!("Telegram long-poll loop started");
            let mut offset: i64 = 0;

            loop {
                match self.get_updates(offset).await {
                    Ok(updates) => {
                        for update in &updates {
                            offset = update.update_id + 1;

                            if let Some(msg) = &update.message {
                                let mut tg_file_attachments: Vec<crate::agent::registry::FileAttachment> = Vec::new();
                                // Try text first, then voice/audio transcription, then caption.
                                let text =
                                    if let Some(t) = msg.text.clone().filter(|s| !s.is_empty()) {
                                        t
                                    } else if let Some(ref voice) = msg.voice {
                                        match self.transcribe_voice(&voice.file_id).await {
                                            Ok(t) => {
                                                info!("voice transcribed ({} chars)", t.len());
                                                t
                                            }
                                            Err(e) => {
                                                warn!("voice transcription failed: {e:#}");
                                                continue;
                                            }
                                        }
                                    } else if let Some(ref audio) = msg.audio {
                                        match self.transcribe_voice(&audio.file_id).await {
                                            Ok(t) => {
                                                info!("audio transcribed ({} chars)", t.len());
                                                t
                                            }
                                            Err(e) => {
                                                warn!("audio transcription failed: {e:#}");
                                                continue;
                                            }
                                        }
                                    } else if let Some(ref video) = msg.video {
                                        // Send video as FileAttachment — runtime decides
                                        // whether to use vision (doubao) or transcription.
                                        match self.download_file(&video.file_id).await {
                                            Ok(bytes) => {
                                                info!(size = bytes.len(), "telegram: video downloaded");
                                                tg_file_attachments.push(crate::agent::registry::FileAttachment {
                                                    filename: "video.mp4".to_owned(),
                                                    data: bytes,
                                                    mime_type: "video/mp4".to_owned(),
                                                });
                                                String::new()
                                            }
                                            Err(e) => {
                                                let err_msg = format!("{e:#}");
                                                let reply = if err_msg.contains("too big") {
                                                    warn!("video too large for Telegram Bot API (>20MB)");
                                                    "Video exceeds Telegram 20MB bot limit. Send a smaller file or share via link."
                                                } else {
                                                    warn!("video download failed: {err_msg}");
                                                    "Video download failed."
                                                };
                                                let _ = self.client
                                                    .post(self.api_url("sendMessage"))
                                                    .json(&json!({
                                                        "chat_id": msg.chat.id,
                                                        "text": reply,
                                                    }))
                                                    .send()
                                                    .await;
                                                continue;
                                            }
                                        }
                                    } else if let Some(ref doc) = msg.document {
                                        let filename = doc.file_name.as_deref().unwrap_or("file");
                                        match self.download_file(&doc.file_id).await {
                                            Ok(bytes) => {
                                                tg_file_attachments.push(crate::agent::registry::FileAttachment {
                                                    filename: filename.to_owned(),
                                                    data: bytes,
                                                    mime_type: "application/octet-stream".to_owned(),
                                                });
                                                String::new()
                                            }
                                            Err(e) => {
                                                format!("[file download failed: {e}]")
                                            }
                                        }
                                    } else if let Some(t) =
                                        msg.caption.clone().filter(|s| !s.is_empty())
                                    {
                                        t
                                    } else if msg.photo.is_some() {
                                        // Photo with no caption — use placeholder text.
                                        String::new()
                                    } else {
                                        continue;
                                    };

                                // Download photo attachments for vision support.
                                let mut images = Vec::new();
                                if let Some(ref photos) = msg.photo {
                                    // Telegram sends multiple sizes; last is largest.
                                    if let Some(largest) = photos.last() {
                                        match self.download_file(&largest.file_id).await {
                                            Ok(bytes) => {
                                                use base64::Engine;
                                                let b64 = base64::engine::general_purpose::STANDARD
                                                    .encode(&bytes);
                                                let data_url =
                                                    format!("data:image/jpeg;base64,{b64}");
                                                images.push(
                                                    crate::agent::registry::ImageAttachment {
                                                        data: data_url,
                                                        mime_type: "image/jpeg".to_string(),
                                                    },
                                                );
                                                info!(
                                                    size = bytes.len(),
                                                    "Telegram photo downloaded for vision"
                                                );
                                            }
                                            Err(e) => {
                                                warn!("Telegram photo download failed: {e:#}");
                                            }
                                        }
                                    }
                                }

                                let peer_id = msg.from.as_ref().map(|u| u.id).unwrap_or(0);
                                let chat_id = msg.chat.id;
                                let is_group = msg.chat.kind != "private";
                                let thread = msg.message_thread_id;

                                debug!(peer_id, chat_id, is_group, "Telegram message received");

                                (self.on_message)(
                                    peer_id, text, chat_id, is_group, thread, images, tg_file_attachments,
                                );
                            }
                        }
                    }
                    Err(e) => {
                        error!("getUpdates error: {e:#}");
                        sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_increases() {
        let cfg = RetryConfig::default();
        let d0 = backoff_delay(0, &cfg).as_millis();
        let d1 = backoff_delay(1, &cfg).as_millis();
        let d2 = backoff_delay(2, &cfg).as_millis();
        assert!(d1 >= d0, "backoff should increase");
        assert!(d2 >= d1, "backoff should increase");
    }

    #[test]
    fn backoff_capped() {
        let cfg = RetryConfig {
            max_delay_ms: 1000,
            ..Default::default()
        };
        let d = backoff_delay(20, &cfg).as_millis();
        assert!(d <= 1100, "backoff should be capped + jitter: got {d}");
    }
}
