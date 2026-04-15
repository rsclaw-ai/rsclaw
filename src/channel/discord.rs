//! Discord Bot channel.
//!
//! Uses the Discord HTTP API for sending messages and a WebSocket
//! connection to the Discord Gateway for receiving events.
//!
//! Features:
//!   - Gateway identify + heartbeat loop.
//!   - MESSAGE_CREATE event dispatch.
//!   - Text chunking (2000-char Discord limit).
//!   - Exponential back-off on send failures.
//!   - Preview streaming: partial mode sends a placeholder then edits it with
//!     the full reply via PATCH /channels/{id}/messages/{msg_id} (agents.md
//!     §21).

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use futures::{SinkExt as _, StreamExt as _, future::BoxFuture};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tracing::{debug, info, warn};

/// Minimum reply length (chars) that triggers preview streaming.
const DISCORD_PREVIEW_THRESHOLD: usize = 200;
/// Delay between sending the placeholder and editing with the full reply.
const DISCORD_EDIT_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

use super::{Channel, OutboundMessage};
use crate::channel::{
    chunker::{BreakPreference, ChunkConfig, chunk_text, platform_chunk_limit},
    telegram::RetryConfig,
};

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
const DISCORD_GATEWAY: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
const DISCORD_GATEWAY_BOT_PATH: &str = "/gateway/bot";

// ---------------------------------------------------------------------------
// Discord API types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct GatewayPayload {
    pub op: u8,
    pub d: Option<Value>,
    pub s: Option<u64>,
    pub t: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MessageCreate {
    pub id: String,
    pub content: String,
    pub channel_id: String,
    pub author: DiscordUser,
    pub guild_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DiscordUser {
    pub id: String,
    pub bot: Option<bool>,
}

// ---------------------------------------------------------------------------
// DiscordChannel
// ---------------------------------------------------------------------------

pub struct DiscordChannel {
    token: String,
    client: Client,
    retry: RetryConfig,
    allow_bots: bool,
    on_message: Arc<dyn Fn(String, String, String, bool) + Send + Sync>,
    // (peer_id, text, channel_id, is_guild)
    /// HTTP API base URL (overridable for testing).
    api_base: String,
    /// Gateway WebSocket URL override. When set, skip the /gateway/bot fetch.
    gateway_url: Option<String>,
}

impl DiscordChannel {
    pub fn new(
        token: impl Into<String>,
        allow_bots: bool,
        on_message: Arc<dyn Fn(String, String, String, bool) + Send + Sync>,
        api_base: Option<String>,
        gateway_url: Option<String>,
    ) -> Self {
        Self {
            token: token.into(),
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            retry: RetryConfig::default(),
            allow_bots,
            on_message,
            api_base: api_base.unwrap_or_else(|| DISCORD_API_BASE.to_owned()),
            gateway_url,
        }
    }

    fn auth_header(&self) -> String {
        format!("Bot {}", self.token)
    }

    async fn send_chunk(&self, channel_id: &str, text: &str) -> Result<()> {
        let body = json!({ "content": text });

        for attempt in 0..self.retry.attempts {
            let resp = self
                .client
                .post(format!("{}/channels/{channel_id}/messages", self.api_base))
                .header("authorization", self.auth_header())
                .json(&body)
                .send()
                .await
                .context("Discord send message")?;

            let status = resp.status();

            if status.as_u16() == 429 {
                // Respect Retry-After header if present.
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|s| Duration::from_millis((s * 1000.0) as u64))
                    .unwrap_or(Duration::from_millis(500));
                warn!(attempt, ?retry_after, "Discord rate limit");
                sleep(retry_after).await;
                continue;
            }

            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                bail!("Discord send failed {status}: {body}");
            }

            return Ok(());
        }

        bail!("Discord send failed after {} attempts", self.retry.attempts)
    }

    /// POST a message to `channel_id` and return the Discord message ID.
    /// Used by preview streaming to get the ID of the placeholder message.
    async fn send_message_returning_id(&self, channel_id: &str, text: &str) -> Result<String> {
        let body = json!({ "content": text });

        for attempt in 0..self.retry.attempts {
            let resp = self
                .client
                .post(format!("{}/channels/{channel_id}/messages", self.api_base))
                .header("authorization", self.auth_header())
                .json(&body)
                .send()
                .await
                .context("Discord send message (preview)")?;

            let status = resp.status();
            if status.as_u16() == 429 {
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|s| std::time::Duration::from_millis((s * 1000.0) as u64))
                    .unwrap_or(std::time::Duration::from_millis(500));
                warn!(attempt, ?retry_after, "Discord rate limit (preview send)");
                sleep(retry_after).await;
                continue;
            }
            if !status.is_success() {
                let err = resp.text().await.unwrap_or_default();
                bail!("Discord send (preview) failed {status}: {err}");
            }

            let v: Value = resp.json().await.context("parse Discord message result")?;
            let id = v["id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Discord: missing message id in response"))?
                .to_owned();
            return Ok(id);
        }

        bail!(
            "Discord send (preview) failed after {} attempts",
            self.retry.attempts
        )
    }

    /// Edit an existing Discord message via PATCH
    /// /channels/{id}/messages/{msg_id}.
    ///
    /// Used by preview streaming (agents.md §21) to replace the placeholder
    /// message with the final reply text.
    async fn edit_message(&self, channel_id: &str, message_id: &str, new_text: &str) -> Result<()> {
        let body = json!({ "content": new_text });

        for attempt in 0..self.retry.attempts {
            let resp = self
                .client
                .patch(format!(
                    "{}/channels/{channel_id}/messages/{message_id}",
                    self.api_base
                ))
                .header("authorization", self.auth_header())
                .json(&body)
                .send()
                .await
                .context("Discord edit message")?;

            let status = resp.status();
            if status.as_u16() == 429 {
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|s| std::time::Duration::from_millis((s * 1000.0) as u64))
                    .unwrap_or(std::time::Duration::from_millis(500));
                warn!(attempt, ?retry_after, "Discord rate limit (edit)");
                sleep(retry_after).await;
                continue;
            }
            if !status.is_success() {
                let err = resp.text().await.unwrap_or_default();
                bail!("Discord edit message failed {status}: {err}");
            }
            return Ok(());
        }

        bail!(
            "Discord edit message failed after {} attempts",
            self.retry.attempts
        )
    }

    /// Preview streaming send (agents.md §21, "partial" mode).
    ///
    /// For replies longer than `DISCORD_PREVIEW_THRESHOLD` characters:
    ///   1. POST a placeholder message ("…") to show the user an immediate
    ///      response.
    ///   2. PATCH the same message with the full reply text after a short
    ///      delay.
    ///
    /// Shorter replies fall back to the standard `send_chunk` path.
    pub async fn send_with_preview(&self, channel_id: &str, text: &str) -> Result<()> {
        if text.len() <= DISCORD_PREVIEW_THRESHOLD {
            return self.send_chunk(channel_id, text).await;
        }

        // Send placeholder so the user sees an immediate response.
        let msg_id = self.send_message_returning_id(channel_id, "…").await?;
        debug!(channel_id, msg_id, "Discord preview: placeholder sent");

        // Simulate streaming delay then edit with the full reply.
        sleep(DISCORD_EDIT_DELAY).await;
        self.edit_message(channel_id, &msg_id, text).await?;
        debug!(
            channel_id,
            msg_id, "Discord preview: message updated with full reply"
        );

        Ok(())
    }

    /// Get the WebSocket gateway URL.
    async fn get_gateway_url(&self) -> Result<String> {
        let resp = self
            .client
            .get(format!("{}{DISCORD_GATEWAY_BOT_PATH}", self.api_base))
            .header("authorization", self.auth_header())
            .send()
            .await
            .context("GET /gateway/bot")?;

        let v: Value = resp.json().await.context("parse gateway")?;
        let url = v["url"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing gateway url"))?;
        Ok(format!("{url}/?v=10&encoding=json"))
    }

    /// Full Discord Gateway WebSocket loop.
    ///
    /// Protocol:
    ///   OP 10 HELLO  — receive heartbeat_interval
    ///   OP 2  IDENTIFY — send bot token + intents (MESSAGE: 1<<9 = 512)
    ///   OP 1  HEARTBEAT — send every heartbeat_interval ms
    ///   OP 11 HEARTBEAT_ACK — receive from server
    ///   OP 0  DISPATCH — receive events (READY, MESSAGE_CREATE)
    async fn gateway_loop(&self) -> Result<()> {
        let url = if let Some(ref override_url) = self.gateway_url {
            override_url.clone()
        } else {
            self.get_gateway_url()
                .await
                .unwrap_or_else(|_| DISCORD_GATEWAY.to_owned())
        };
        info!("Discord: connecting to gateway {url}");

        let (ws_stream, _) = connect_async(&url).await.context("Discord WS connect")?;
        let (mut write, mut read) = ws_stream.split();

        #[allow(unused_assignments)]
        let mut heartbeat_interval = Duration::from_millis(41_250); // default; overwritten by OP10
        let mut last_sequence: Option<u64> = None;
        let mut heartbeat_ticker: Option<tokio::time::Interval> = None;
        let mut identified = false;

        loop {
            // Drive heartbeat + read concurrently.
            let ws_msg = if let Some(ref mut ticker) = heartbeat_ticker {
                tokio::select! {
                    _ = ticker.tick() => {
                        let hb = json!({"op": 1, "d": last_sequence});
                        debug!("Discord: sending heartbeat (seq={last_sequence:?})");
                        if write.send(WsMessage::Text(hb.to_string().into())).await.is_err() {
                            bail!("Discord: WS write error on heartbeat");
                        }
                        continue;
                    }
                    msg = read.next() => msg,
                }
            } else {
                read.next().await
            };

            let raw = match ws_msg {
                Some(Ok(WsMessage::Text(s))) => s.to_string(),
                Some(Ok(WsMessage::Close(frame))) => {
                    let code = frame.as_ref().map(|f| f.code.into()).unwrap_or(0u16);
                    info!("Discord: gateway closed (code {code})");
                    bail!("Discord: gateway closed (code {code})");
                }
                Some(Ok(_)) => continue, // binary/ping/pong
                Some(Err(e)) => bail!("Discord: WS error: {e}"),
                None => bail!("Discord: WS stream ended"),
            };

            let payload: Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(e) => {
                    warn!("Discord: parse error: {e}");
                    continue;
                }
            };

            if let Some(s) = payload["s"].as_u64() {
                last_sequence = Some(s);
            }

            let op = payload["op"].as_u64().unwrap_or(255);
            match op {
                // OP 10 HELLO
                10 => {
                    let ms = payload["d"]["heartbeat_interval"]
                        .as_u64()
                        .unwrap_or(41_250);
                    heartbeat_interval = Duration::from_millis(ms);
                    heartbeat_ticker = Some(tokio::time::interval(heartbeat_interval));
                    info!("Discord: HELLO — heartbeat every {ms}ms");

                    if !identified {
                        // Intents: GUILD_MESSAGES (1<<9) + DIRECT_MESSAGES (1<<12)
                        let identify = json!({
                            "op": 2,
                            "d": {
                                "token": self.token,
                                "intents": (1u32 << 9) | (1u32 << 12),
                                "properties": {
                                    "os": "linux",
                                    "browser": "rsclaw",
                                    "device": "rsclaw",
                                }
                            }
                        });
                        write
                            .send(WsMessage::Text(identify.to_string().into()))
                            .await
                            .context("Discord: send IDENTIFY")?;
                        identified = true;
                        debug!("Discord: sent IDENTIFY");
                    }
                }
                // OP 11 HEARTBEAT_ACK
                11 => {
                    debug!("Discord: heartbeat ACK");
                }
                // OP 1 HEARTBEAT request from server
                1 => {
                    let hb = json!({"op": 1, "d": last_sequence});
                    let _ = write.send(WsMessage::Text(hb.to_string().into())).await;
                }
                // OP 0 DISPATCH
                0 => {
                    let event_type = payload["t"].as_str().unwrap_or("");
                    match event_type {
                        "READY" => {
                            let user = &payload["d"]["user"]["username"];
                            info!("Discord: READY as {user}");
                        }
                        "MESSAGE_CREATE" => {
                            let d = &payload["d"];
                            let is_bot = d["author"]["bot"].as_bool().unwrap_or(false);
                            if is_bot && !self.allow_bots {
                                continue;
                            }
                            let mut content = d["content"].as_str().unwrap_or("").to_owned();
                            let channel_id = d["channel_id"].as_str().unwrap_or("").to_owned();
                            let peer_id = d["author"]["id"].as_str().unwrap_or("").to_owned();
                            let is_guild = d["guild_id"].is_string();

                            // Process attachments (images, audio, video, files)
                            if let Some(attachments) = d["attachments"].as_array() {
                                for att in attachments {
                                    let url = att["url"].as_str().unwrap_or("");
                                    let filename = att["filename"].as_str().unwrap_or("file");
                                    let content_type = att["content_type"].as_str().unwrap_or("");
                                    if url.is_empty() { continue; }

                                    let download = self.client.get(url).send().await;
                                    let bytes = match download {
                                        Ok(resp) if resp.status().is_success() => {
                                            resp.bytes().await.ok().map(|b| b.to_vec())
                                        }
                                        _ => None,
                                    };

                                    if let Some(bytes) = bytes {
                                        if content_type.starts_with("audio/") || content_type.starts_with("video/") {
                                            match crate::channel::transcription::transcribe_audio(
                                                &self.client, &bytes, filename, content_type,
                                            ).await {
                                                Ok(text) => {
                                                    info!("Discord: attachment transcribed ({} chars)", text.len());
                                                    if !content.is_empty() { content.push('\n'); }
                                                    content.push_str(&text);
                                                }
                                                Err(_) => {
                                                    if !content.is_empty() { content.push('\n'); }
                                                    content.push_str(&format!("[{content_type} attachment: {filename}]"));
                                                }
                                            }
                                        } else if content_type.starts_with("image/") {
                                            // Image: note it for now (no vision vec in Discord callback)
                                            if !content.is_empty() { content.push('\n'); }
                                            content.push_str(&crate::i18n::t("image_attachment_received", crate::i18n::default_lang()));
                                        } else {
                                            let processed = discord_process_file(filename, &bytes);
                                            if !content.is_empty() { content.push('\n'); }
                                            content.push_str(&processed);
                                        }
                                    } else {
                                        if !content.is_empty() { content.push('\n'); }
                                        content.push_str(&format!("[attachment download failed: {filename}]"));
                                    }
                                }
                            }

                            if content.is_empty() { continue; }
                            debug!(peer = %peer_id, channel = %channel_id, "Discord: MESSAGE_CREATE");
                            (self.on_message)(peer_id, content, channel_id, is_guild);
                        }
                        _ => {
                            debug!("Discord: event {event_type}");
                        }
                    }
                }
                _ => {
                    debug!("Discord: unknown op {op}");
                }
            }
        }
    }
}

impl Channel for DiscordChannel {
    fn name(&self) -> &str {
        "discord"
    }

    fn send(&self, msg: OutboundMessage) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let cfg = ChunkConfig {
                max_chars: platform_chunk_limit("discord"),
                min_chars: 1,
                break_preference: BreakPreference::Newline,
            };
            let chunks = chunk_text(&msg.text, &cfg);
            for chunk in &chunks {
                self.send_chunk(&msg.target_id, chunk).await?;
            }

            // Send image attachments via multipart file upload
            if !msg.images.is_empty() {
                info!(count = msg.images.len(), "discord: sending images");
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
                        warn!(idx, "discord: base64 decode failed: {e}");
                        continue;
                    }
                };
                let part = match reqwest::multipart::Part::bytes(bytes)
                    .file_name("image.png")
                    .mime_str("image/png")
                {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(idx, "discord: build multipart failed: {e}");
                        continue;
                    }
                };
                let form = reqwest::multipart::Form::new()
                    .part("files[0]", part)
                    .text(
                        "payload_json",
                        serde_json::json!({"content": ""}).to_string(),
                    );
                let url = format!(
                    "{}/channels/{}/messages",
                    self.api_base, msg.target_id
                );
                match self
                    .client
                    .post(&url)
                    .header("authorization", self.auth_header())
                    .multipart(form)
                    .send()
                    .await
                {
                    Ok(resp) if !resp.status().is_success() => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        warn!(idx, %status, "discord: image upload failed: {body}");
                    }
                    Err(e) => warn!(idx, "discord: image upload request failed: {e}"),
                    Ok(_) => {}
                }
            }

            Ok(())
        })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                match self.gateway_loop().await {
                    Ok(()) => {
                        info!("Discord gateway loop exited cleanly, reconnecting");
                        backoff = Duration::from_secs(1);
                    }
                    Err(e) => {
                        warn!(
                            "Discord gateway error: {e:#}, reconnecting in {}s",
                            backoff.as_secs()
                        );
                        sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(120));
                    }
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// File processing helpers
// ---------------------------------------------------------------------------

fn discord_is_text_file(name: &str) -> bool {
    let exts = [
        ".txt", ".md", ".csv", ".json", ".toml", ".yaml", ".yml", ".xml", ".html",
        ".rs", ".py", ".js", ".ts", ".go", ".sh", ".log", ".conf", ".cfg", ".c", ".h", ".java",
    ];
    exts.iter().any(|e| name.ends_with(e))
}

fn discord_process_file(filename: &str, bytes: &[u8]) -> String {
    let lower = filename.to_lowercase();
    if lower.ends_with(".pdf") {
        if let Ok(text) = crate::agent::doc::safe_extract_pdf_from_mem(bytes) {
            return format!("[PDF: {filename}]\n{}", &text[..text.len().min(20000)]);
        }
        // Fallback to pdftotext CLI
        let tmp = std::env::temp_dir().join(format!("rsclaw_discord_{filename}"));
        if std::fs::write(&tmp, bytes).is_ok() {
            let output = std::process::Command::new("pdftotext")
                .args([tmp.to_str().unwrap_or(""), "-"])
                .output();
            let _ = std::fs::remove_file(&tmp);
            if let Ok(o) = output {
                if o.status.success() {
                    let text = String::from_utf8_lossy(&o.stdout);
                    return format!("[PDF: {filename}]\n{}", &text[..text.len().min(20000)]);
                }
            }
            format!("[PDF: {filename} ({} bytes)]", bytes.len())
        } else {
            format!("[file: {filename}]")
        }
    } else if lower.ends_with(".docx") || lower.ends_with(".xlsx") || lower.ends_with(".pptx") {
        if let Some(text) = crate::channel::extract_office_text(filename, bytes) {
            let label = if lower.ends_with(".docx") { "Word" }
                else if lower.ends_with(".xlsx") { "Excel" }
                else { "PowerPoint" };
            format!("[{label}: {filename}]\n{}", &text[..text.len().min(20000)])
        } else {
            let label = if lower.ends_with(".docx") { "Word" }
                else if lower.ends_with(".xlsx") { "Excel" }
                else { "PowerPoint" };
            format!("[{label} file: {filename} ({} bytes)]", bytes.len())
        }
    } else if discord_is_text_file(&lower) {
        let text = String::from_utf8_lossy(bytes);
        format!("[File: {filename}]\n```\n{}\n```", &text[..text.len().min(20000)])
    } else {
        let ws = crate::config::loader::base_dir().join("workspace/uploads");
        let _ = std::fs::create_dir_all(&ws);
        let dest = ws.join(filename);
        let _ = std::fs::write(&dest, bytes);
        format!("[File saved: {filename} ({} bytes) at {}]", bytes.len(), dest.display())
    }
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
        let ch = DiscordChannel::new("token", false, Arc::new(|_, _, _, _| {}), None, None);
        assert_eq!(ch.name(), "discord");
    }

    #[test]
    fn auth_header_format() {
        init_crypto();
        let ch = DiscordChannel::new("my-token", false, Arc::new(|_, _, _, _| {}), None, None);
        assert_eq!(ch.auth_header(), "Bot my-token");
    }
}
