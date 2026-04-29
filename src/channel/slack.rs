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
use tracing::{info, warn};

use super::{Channel, OutboundMessage};
use crate::channel::{
    attachments::{mime_to_ext, parse_data_url, pick_file_mime},
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
    #[allow(clippy::type_complexity)]
    on_message: Arc<
        dyn Fn(
                String,
                String,
                String,
                bool,
                Vec<crate::agent::registry::ImageAttachment>,
                Vec<crate::agent::registry::FileAttachment>,
            ) + Send
            + Sync,
    >,
    // (user_id, text, channel_id, is_channel, images, files)
}

impl SlackChannel {
    #[allow(clippy::type_complexity)]
    pub fn new(
        bot_token: impl Into<String>,
        app_token: Option<String>,
        api_base: Option<String>,
        on_message: Arc<
            dyn Fn(
                    String,
                    String,
                    String,
                    bool,
                    Vec<crate::agent::registry::ImageAttachment>,
                    Vec<crate::agent::registry::FileAttachment>,
                ) + Send
                + Sync,
        >,
    ) -> Self {
        Self {
            bot_token: bot_token.into(),
            app_token,
            api_base: api_base
                .unwrap_or_else(|| SLACK_API_BASE.to_owned()),
            client: crate::config::build_proxy_client()
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

    /// Upload a file or image via Slack's V2 external upload flow:
    ///   1. POST `files.getUploadURLExternal` (form: filename, length) →
    ///      `{ ok, upload_url, file_id }`
    ///   2. POST raw bytes to `upload_url` (no auth, content-type from mime)
    ///   3. POST `files.completeUploadExternal` (json: files, channel_id) →
    ///      `{ ok, files }`
    /// V1 `files.upload` was disabled in March 2025 with
    /// `{"ok":false,"error":"method_deprecated"}`.
    async fn upload_v2(
        &self,
        channel_id: &str,
        filename: &str,
        mime: &str,
        bytes: &[u8],
        title: &str,
    ) -> Result<()> {
        // Step 1: get upload URL.
        let step1 = self
            .client
            .post(format!("{}/files.getUploadURLExternal", self.api_base))
            .bearer_auth(&self.bot_token)
            .form(&[("filename", filename), ("length", &bytes.len().to_string())])
            .send()
            .await
            .context("getUploadURLExternal request")?;
        let body1: Value = step1
            .json()
            .await
            .context("getUploadURLExternal parse")?;
        if body1.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            bail!(
                "getUploadURLExternal: {}",
                body1.get("error").and_then(|e| e.as_str()).unwrap_or("?")
            );
        }
        let upload_url = body1
            .get("upload_url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("getUploadURLExternal: no upload_url"))?;
        let file_id = body1
            .get("file_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("getUploadURLExternal: no file_id"))?
            .to_owned();

        // Step 2: POST the file as multipart form — Slack's signed
        // upload_url expects a `file=` field. Earlier raw-body POST
        // returned HTTP 200 but produced a 0-byte attachment in the
        // channel ("uploaded but blank" symptom).
        let part = reqwest::multipart::Part::bytes(bytes.to_vec())
            .file_name(filename.to_owned())
            .mime_str(mime)
            .map_err(|e| anyhow::anyhow!("build upload multipart: {e}"))?;
        let form = reqwest::multipart::Form::new().part("file", part);
        let put = self
            .client
            .post(upload_url)
            .multipart(form)
            .send()
            .await
            .context("upload_url POST")?;
        if !put.status().is_success() {
            let s = put.status();
            let b = put.text().await.unwrap_or_default();
            bail!("upload_url returned {s}: {b}");
        }
        let put_body = put.text().await.unwrap_or_default();
        info!(filename, len = bytes.len(), resp = %put_body, "slack: upload step 2 done");

        // Step 3: complete + share to channel.
        let payload = json!({
            "files": [{ "id": file_id, "title": title }],
            "channel_id": channel_id,
        });
        let step3 = self
            .client
            .post(format!("{}/files.completeUploadExternal", self.api_base))
            .bearer_auth(&self.bot_token)
            .header("content-type", "application/json")
            .body(payload.to_string())
            .send()
            .await
            .context("completeUploadExternal request")?;
        let body3: Value = step3
            .json()
            .await
            .context("completeUploadExternal parse")?;
        if body3.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            bail!(
                "completeUploadExternal: {}",
                body3.get("error").and_then(|e| e.as_str()).unwrap_or("?")
            );
        }
        Ok(())
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

        const WS_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
        loop {
            let msg = match tokio::time::timeout(WS_IDLE_TIMEOUT, read.next()).await {
                Ok(Some(msg)) => msg,
                Ok(None) => { info!("Slack: WS stream ended"); break; }
                Err(_) => { warn!("Slack: WS idle timeout ({}s), reconnecting", WS_IDLE_TIMEOUT.as_secs()); bail!("Slack: idle timeout"); }
            };
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
                    info!("Slack: events_api type={etype}");
                    // app_mention fires when the bot is @-mentioned in a
                    // channel; the payload shape is identical to message
                    // for our purposes (user/text/channel/files), so treat
                    // both the same. Without this branch, mentions in
                    // channels were silently dropped because the default
                    // arm only logged at debug level.
                    if etype == "message" || etype == "app_mention" {
                        // Skip messages the bot itself sent — Slack pushes
                        // every message in subscribed channels including
                        // the bot's own replies, which without this guard
                        // produces an infinite self-reply loop:
                        //   bot replies → Slack pushes the reply →
                        //   on_message → agent treats it as a user msg →
                        //   replies again → loop forever.
                        // Slack marks bot-sent messages with either a
                        // `bot_id` field or a `subtype == "bot_message"`.
                        let is_bot_msg = event.get("bot_id").is_some()
                            || event.get("subtype").and_then(|v| v.as_str())
                                == Some("bot_message");
                        if is_bot_msg {
                            info!("Slack: skipping bot's own message");
                            continue;
                        }
                        let user = event["user"].as_str().unwrap_or("").to_owned();
                        let mut text = event["text"].as_str().unwrap_or("").to_owned();
                        // Slack intercepts a leading `/` as a native slash
                        // command UI and never delivers it to the bot, so
                        // rewrite a leading `\xxx` to `/xxx` as a Slack-only
                        // alias. Users discover it from /help (which they
                        // can't actually type, hence \help works as the
                        // discovery escape hatch).
                        if let Some(rest) = text.strip_prefix('\\') {
                            if rest.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) {
                                text = format!("/{rest}");
                            }
                        }
                        let channel = event["channel"].as_str().unwrap_or("").to_owned();
                        let is_channel = event["channel_type"]
                            .as_str()
                            .map(|t| t == "channel" || t == "group")
                            .unwrap_or(false);

                        // Process file attachments. Images go into `images`
                        // so the runtime hands them to the vision model AND
                        // pending_files (analyze vs save) can fire. Other
                        // files go into `file_attachments` for the same
                        // reason. Audio/video still get inline-transcribed.
                        let mut images: Vec<crate::agent::registry::ImageAttachment> = Vec::new();
                        let mut file_attachments: Vec<crate::agent::registry::FileAttachment> = Vec::new();
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
                                        use base64::Engine as _;
                                        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                                        let mime = if mimetype.is_empty() { "image/png" } else { mimetype };
                                        images.push(crate::agent::registry::ImageAttachment {
                                            data: format!("data:{mime};base64,{b64}"),
                                            mime_type: mime.to_owned(),
                                        });
                                        info!(size = bytes.len(), %filename, "Slack: image forwarded for vision");
                                    } else {
                                        let processed = slack_process_file(filename, &bytes);
                                        file_attachments.push(crate::agent::registry::FileAttachment {
                                            filename: filename.to_owned(),
                                            data: bytes.clone(),
                                            mime_type: if mimetype.is_empty() {
                                                "application/octet-stream".to_owned()
                                            } else {
                                                mimetype.to_owned()
                                            },
                                        });
                                        if !text.is_empty() { text.push('\n'); }
                                        text.push_str(&processed);
                                    }
                                } else {
                                    if !text.is_empty() { text.push('\n'); }
                                    text.push_str(&format!("[file download failed: {filename}]"));
                                }
                            }
                        }

                        if !user.is_empty() && (!text.is_empty() || !images.is_empty() || !file_attachments.is_empty()) {
                            info!(
                                user = %user,
                                channel = %channel,
                                etype = %etype,
                                len = text.len(),
                                imgs = images.len(),
                                files = file_attachments.len(),
                                "Slack: dispatching message"
                            );
                            (self.on_message)(user, text, channel, is_channel, images, file_attachments);
                        } else {
                            warn!(
                                user_empty = user.is_empty(),
                                text_empty = text.is_empty(),
                                etype = %etype,
                                "Slack: event ignored — empty user or text"
                            );
                        }
                    }
                }
                "disconnect" => {
                    info!("Slack: server requested disconnect");
                    bail!("Slack: server disconnect");
                }
                _ => {
                    // Other Socket Mode envelopes: slash_commands, interactive,
                    // etc. We don't act on them but log so the user can see
                    // *something* arrived if the message handler stays silent.
                    info!("Slack: ignored envelope type={msg_type}");
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
            // Only post text when there's actual text. Slack's
            // chat.postMessage rejects empty bodies with `no_text`, and
            // the `?` here used to short-circuit the entire send — so an
            // image-only reply (e.g. `/ss` returns OutboundMessage with
            // text="" + 1 image) silently dropped the image because we
            // never reached the upload loop below.
            if !msg.text.trim().is_empty() {
                for chunk in &chunk_text(&msg.text, &cfg) {
                    self.post_message(&msg.target_id, chunk).await?;
                }
            }
            if !msg.images.is_empty() {
                info!(count = msg.images.len(), "slack: sending images");
            }
            for (idx, image_data) in msg.images.iter().enumerate() {
                // http(s):// — let Slack unfurl via an image block instead
                // of mis-decoding the URL string as base64.
                if image_data.starts_with("http://") || image_data.starts_with("https://") {
                    let payload = json!({
                        "channel": msg.target_id,
                        "blocks": [{
                            "type": "image",
                            "image_url": image_data,
                            "alt_text": "image",
                        }],
                    });
                    match self
                        .client
                        .post(format!("{}/chat.postMessage", self.api_base))
                        .bearer_auth(&self.bot_token)
                        .header("content-type", "application/json")
                        .body(payload.to_string())
                        .send()
                        .await
                    {
                        Ok(resp) if !resp.status().is_success() => {
                            let status = resp.status();
                            let body = resp.text().await.unwrap_or_default();
                            warn!(idx, %status, "slack: image block post failed: {body}");
                        }
                        Err(e) => warn!(idx, "slack: image block request failed: {e}"),
                        Ok(_) => {}
                    }
                    continue;
                }

                let (mime, b64) = parse_data_url(image_data)
                    .unwrap_or(("image/png", image_data.as_str()));
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
                let ext = mime_to_ext(mime);
                let filename = format!("image.{ext}");
                if let Err(e) = self
                    .upload_v2(&msg.target_id, &filename, mime, &bytes, "Image")
                    .await
                {
                    warn!(idx, "slack: image upload (v2) failed: {e:#}");
                } else {
                    info!(idx, "slack: image uploaded (v2)");
                }
            }

            // Send file attachments via the v2 flow:
            //   1) files.getUploadURLExternal -> upload_url + file_id
            //   2) POST raw bytes to upload_url
            //   3) files.completeUploadExternal -> link to channel
            // The legacy files.upload returns
            // {"ok":false,"error":"method_deprecated"} as of 2025-03 for new
            // apps and is being phased out for existing apps.
            if !msg.files.is_empty() {
                info!(count = msg.files.len(), "slack: sending files");
            }
            for (idx, (filename, mime, path)) in msg.files.iter().enumerate() {
                let bytes = match std::fs::read(path) {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(idx, %filename, "slack: file read failed: {e}");
                        continue;
                    }
                };
                let mime_str = pick_file_mime(mime, filename);
                if let Err(e) = self
                    .upload_v2(&msg.target_id, filename, mime_str, &bytes, filename)
                    .await
                {
                    warn!(idx, %filename, "slack: file upload (v2) failed: {e:#}");
                } else {
                    info!(idx, %filename, "slack: file sent (v2)");
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
        if let Ok(text) = crate::agent::doc::safe_extract_pdf_from_mem(bytes) {
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
        let ch = SlackChannel::new("xoxb-token", None, None, Arc::new(|_, _, _, _, _, _| {}));
        assert_eq!(ch.name(), "slack");
    }
}
