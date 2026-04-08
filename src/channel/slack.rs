//! Slack channel — Socket Mode + Events API.
//!
//! Socket Mode uses a WebSocket connection authenticated with an
//! `xapp-*` app-level token, allowing Slack events without a public
//! HTTP endpoint.
//!
//! Send path: Slack Web API `chat.postMessage` / `chat.update`.
//! Receive path: Socket Mode WebSocket → `message` events.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use futures::{SinkExt as _, StreamExt as _, future::BoxFuture};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tracing::{debug, info, warn};

use super::{Channel, OutboundMessage};
use crate::channel::{
    chunker::{BreakPreference, ChunkConfig, chunk_text, platform_chunk_limit},
    telegram::RetryConfig,
};

const SLACK_API_BASE: &str = "https://slack.com/api";

// ---------------------------------------------------------------------------
// SlackChannel
// ---------------------------------------------------------------------------

pub struct SlackChannel {
    bot_token: String,
    app_token: Option<String>,
    /// API base URL — defaults to SLACK_API_BASE, overridable for testing.
    api_base: String,
    client: Client,
    retry: RetryConfig,
    on_message: Arc<dyn Fn(String, String, String, bool) + Send + Sync>,
    // (peer_id/user_id, text, channel_id, is_channel)
}

impl SlackChannel {
    pub fn new(
        bot_token: impl Into<String>,
        app_token: Option<String>,
        api_base: Option<String>,
        on_message: Arc<dyn Fn(String, String, String, bool) + Send + Sync>,
    ) -> Self {
        Self {
            bot_token: bot_token.into(),
            app_token,
            api_base: api_base
                .unwrap_or_else(|| SLACK_API_BASE.to_owned()),
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            retry: RetryConfig::default(),
            on_message,
        }
    }

    async fn post_message(&self, channel_id: &str, text: &str) -> Result<()> {
        let body = json!({
            "channel": channel_id,
            "text":    text,
        });

        for attempt in 0..self.retry.attempts {
            let resp = self
                .client
                .post(format!("{}/chat.postMessage", self.api_base))
                .bearer_auth(&self.bot_token)
                .json(&body)
                .send()
                .await
                .context("Slack postMessage")?;

            let status = resp.status();

            if status.as_u16() == 429 {
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(1);
                warn!(attempt, retry_after, "Slack rate limit");
                sleep(Duration::from_secs(retry_after)).await;
                continue;
            }

            let v: Value = resp.json().await.context("parse Slack response")?;
            if v["ok"].as_bool() != Some(true) {
                let err = v["error"].as_str().unwrap_or("unknown");
                // "ratelimited" may also come in the response body.
                if err == "ratelimited" {
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
                bail!("Slack postMessage error: {err}");
            }

            return Ok(());
        }

        bail!(
            "Slack postMessage failed after {} attempts",
            self.retry.attempts
        )
    }

    /// Open a Socket Mode WebSocket connection URL.
    async fn open_socket_url(&self) -> Result<String> {
        let app_token = self
            .app_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Slack app_token required for Socket Mode"))?;

        let resp = self
            .client
            .post(format!("{}/apps.connections.open", self.api_base))
            .bearer_auth(app_token)
            .send()
            .await
            .context("apps.connections.open")?;

        let v: Value = resp.json().await.context("parse socket url")?;
        if v["ok"].as_bool() != Some(true) {
            bail!(
                "apps.connections.open failed: {}",
                v["error"].as_str().unwrap_or("unknown")
            );
        }

        v["url"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("missing socket url"))
    }

    /// Socket Mode WebSocket loop.
    ///
    /// Slack Socket Mode protocol:
    ///   - Connect to WSS URL from apps.connections.open
    ///   - Receive `hello` event
    ///   - Receive `events_api` / `slash_commands` / `interactive` envelopes
    ///   - Each envelope must be ACKed with `{"envelope_id": "<id>"}`
    async fn socket_loop(&self) -> Result<()> {
        let url = self.open_socket_url().await?;
        info!("Slack: connecting to Socket Mode {url}");

        let (ws_stream, _) = connect_async(&url).await.context("Slack WS connect")?;
        let (mut write, mut read) = ws_stream.split();

        while let Some(msg) = read.next().await {
            let raw = match msg {
                Ok(WsMessage::Text(s)) => s.to_string(),
                Ok(WsMessage::Close(frame)) => {
                    let code = frame.as_ref().map(|f| f.code.into()).unwrap_or(0u16);
                    info!("Slack: socket closed (code {code})");
                    bail!("Slack: socket closed (code {code})");
                }
                Ok(_) => continue,
                Err(e) => bail!("Slack: WS error: {e}"),
            };

            let payload: Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(e) => {
                    warn!("Slack: parse error: {e}");
                    continue;
                }
            };

            let msg_type = payload["type"].as_str().unwrap_or("");
            match msg_type {
                "hello" => {
                    info!("Slack: Socket Mode hello — connected");
                }
                "events_api" => {
                    let envelope_id = payload["envelope_id"].as_str().unwrap_or("").to_owned();
                    // ACK immediately to avoid replay.
                    if !envelope_id.is_empty() {
                        let ack = json!({"envelope_id": envelope_id});
                        let _ = write.send(WsMessage::Text(ack.to_string().into())).await;
                    }
                    let event = &payload["payload"]["event"];
                    let etype = event["type"].as_str().unwrap_or("");
                    if etype == "message" {
                        let user = event["user"].as_str().unwrap_or("").to_owned();
                        let mut text = event["text"].as_str().unwrap_or("").to_owned();
                        let channel = event["channel"].as_str().unwrap_or("").to_owned();
                        let is_channel = event["channel_type"]
                            .as_str()
                            .map(|t| t == "channel" || t == "group")
                            .unwrap_or(false);

                        // Process file attachments
                        if let Some(files) = event["files"].as_array() {
                            for file in files {
                                let url = file["url_private_download"].as_str().unwrap_or("");
                                let filename = file["name"].as_str().unwrap_or("file");
                                let mimetype = file["mimetype"].as_str().unwrap_or("");
                                if url.is_empty() { continue; }

                                let download = self.client.get(url)
                                    .bearer_auth(&self.bot_token)
                                    .send().await;
                                let bytes = match download {
                                    Ok(resp) if resp.status().is_success() => {
                                        resp.bytes().await.ok().map(|b| b.to_vec())
                                    }
                                    _ => None,
                                };

                                if let Some(bytes) = bytes {
                                    if mimetype.starts_with("audio/") || mimetype.starts_with("video/") {
                                        match crate::channel::transcription::transcribe_audio(
                                            &self.client, &bytes, filename, mimetype,
                                        ).await {
                                            Ok(t) => {
                                                info!("Slack: file transcribed ({} chars)", t.len());
                                                if !text.is_empty() { text.push('\n'); }
                                                text.push_str(&t);
                                            }
                                            Err(_) => {
                                                if !text.is_empty() { text.push('\n'); }
                                                text.push_str(&format!("[{mimetype} file: {filename}]"));
                                            }
                                        }
                                    } else if mimetype.starts_with("image/") {
                                        if !text.is_empty() { text.push('\n'); }
                                        text.push_str(&crate::i18n::t("image_file_received", crate::i18n::default_lang()));
                                    } else {
                                        let processed = slack_process_file(filename, &bytes);
                                        if !text.is_empty() { text.push('\n'); }
                                        text.push_str(&processed);
                                    }
                                } else {
                                    if !text.is_empty() { text.push('\n'); }
                                    text.push_str(&format!("[file download failed: {filename}]"));
                                }
                            }
                        }

                        if !user.is_empty() && !text.is_empty() {
                            debug!(user = %user, channel = %channel, "Slack: message event");
                            (self.on_message)(user, text, channel, is_channel);
                        }
                    }
                }
                "disconnect" => {
                    info!("Slack: server requested disconnect");
                    bail!("Slack: server disconnect");
                }
                _ => {
                    debug!("Slack: event type {msg_type}");
                }
            }
        }
        bail!("Slack: socket stream ended")
    }
}

impl Channel for SlackChannel {
    fn name(&self) -> &str {
        "slack"
    }

    fn send(&self, msg: OutboundMessage) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let cfg = ChunkConfig {
                max_chars: platform_chunk_limit("slack"),
                min_chars: 1,
                break_preference: BreakPreference::Paragraph,
            };
            for chunk in &chunk_text(&msg.text, &cfg) {
                self.post_message(&msg.target_id, chunk).await?;
            }
            if !msg.images.is_empty() {
                info!(count = msg.images.len(), "slack: sending images");
            }
            for (idx, image_data) in msg.images.iter().enumerate() {
                let b64 = image_data
                    .strip_prefix("data:image/png;base64,")
                    .or_else(|| image_data.strip_prefix("data:image/jpeg;base64,"))
                    .unwrap_or(image_data);
                use base64::Engine;
                let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
                    Ok(b) if !b.is_empty() => b,
                    Ok(_) => {
                        warn!(idx, "slack: image decode produced empty bytes");
                        continue;
                    }
                    Err(e) => {
                        warn!(idx, "slack: base64 decode failed: {e}");
                        continue;
                    }
                };
                let part = match reqwest::multipart::Part::bytes(bytes)
                    .file_name("image.png")
                    .mime_str("image/png")
                {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(idx, "slack: build multipart failed: {e}");
                        continue;
                    }
                };
                let form = reqwest::multipart::Form::new()
                    .text("channels", msg.target_id.clone())
                    .text("filename", "image.png")
                    .text("title", "Image")
                    .part("file", part);
                match self.client.post(format!("{}/files.upload", self.api_base))
                    .header("Authorization", format!("Bearer {}", self.bot_token))
                    .multipart(form)
                    .send().await
                {
                    Ok(resp) if !resp.status().is_success() => {
                        let status = resp.status();
                        warn!(idx, %status, "slack: files.upload failed");
                    }
                    Err(e) => warn!(idx, "slack: files.upload request failed: {e}"),
                    Ok(_) => {}
                }
            }
            Ok(())
        })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            if self.app_token.is_none() {
                info!("Slack: no app_token — Socket Mode disabled, using send-only mode");
                std::future::pending::<()>().await;
                return Ok(());
            }
            let mut backoff = Duration::from_secs(1);
            loop {
                match self.socket_loop().await {
                    Ok(()) => {
                        info!("Slack: socket loop exited, reconnecting");
                        backoff = Duration::from_secs(1);
                    }
                    Err(e) => {
                        warn!(
                            "Slack: socket error: {e:#}, reconnecting in {}s",
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

fn slack_is_text_file(name: &str) -> bool {
    let exts = [
        ".txt", ".md", ".csv", ".json", ".toml", ".yaml", ".yml", ".xml", ".html",
        ".rs", ".py", ".js", ".ts", ".go", ".sh", ".log", ".conf", ".cfg", ".c", ".h", ".java",
    ];
    exts.iter().any(|e| name.ends_with(e))
}

fn slack_process_file(filename: &str, bytes: &[u8]) -> String {
    let lower = filename.to_lowercase();
    if lower.ends_with(".pdf") {
        if let Ok(text) = pdf_extract::extract_text_from_mem(bytes) {
            return format!("[PDF: {filename}]\n{}", &text[..text.len().min(20000)]);
        }
        // Fallback to pdftotext CLI
        let tmp = std::env::temp_dir().join(format!("rsclaw_slack_{filename}"));
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
    } else if slack_is_text_file(&lower) {
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
        let ch = SlackChannel::new("xoxb-token", None, None, Arc::new(|_, _, _, _| {}));
        assert_eq!(ch.name(), "slack");
    }
}
