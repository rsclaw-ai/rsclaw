//! DingTalk (钉钉) bot channel driver.
//!
//! Implements the DingTalk Robot API for receiving and sending messages.
//!
//! Features:
//!   - Access token management with auto-refresh (2-hour TTL).
//!   - Stream Mode WebSocket for receiving inbound messages (no public URL
//!     needed).
//!   - Send replies via robot batch-send API.
//!   - Voice message download and transcription via shared Whisper module.
//!   - Text chunking (20000-char limit).

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use futures::{SinkExt as _, StreamExt as _, future::BoxFuture};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{sync::RwLock, time::sleep};
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tracing::{debug, error, info, warn};

use super::{Channel, OutboundMessage};
use crate::channel::{
    chunker::{BreakPreference, ChunkConfig, chunk_text},
    telegram::RetryConfig,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DINGTALK_OAPI_BASE: &str = "https://oapi.dingtalk.com";
const DINGTALK_API_BASE: &str = "https://api.dingtalk.com";

/// DingTalk single-message text limit.
const DINGTALK_CHUNK_LIMIT: usize = 20_000;

/// Access token refresh margin -- refresh 5 minutes before actual expiry.
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// API response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    expires_in: u64,
    #[serde(default)]
    errcode: i64,
    #[serde(default)]
    errmsg: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamConnectResponse {
    endpoint: Option<String>,
    ticket: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FileDownloadResponse {
    #[serde(rename = "downloadUrl")]
    download_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Cached access token
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct CachedToken {
    token: String,
    obtained_at: Instant,
    expires_in: Duration,
}

impl CachedToken {
    fn is_expired(&self) -> bool {
        self.obtained_at.elapsed() + TOKEN_REFRESH_MARGIN >= self.expires_in
    }
}

// ---------------------------------------------------------------------------
// DingTalkChannel
// ---------------------------------------------------------------------------

pub struct DingTalkChannel {
    app_key: String,
    app_secret: String,
    robot_code: String,
    api_base: String,
    oapi_base: String,
    client: Client,
    retry: RetryConfig,
    token_cache: RwLock<Option<CachedToken>>,
    /// Callback: (sender_id, text, conversation_id, is_group, images).
    #[allow(clippy::type_complexity)]
    on_message: Arc<
        dyn Fn(String, String, String, bool, Vec<crate::agent::registry::ImageAttachment>)
            + Send
            + Sync,
    >,
}

impl DingTalkChannel {
    pub fn new(
        app_key: impl Into<String>,
        app_secret: impl Into<String>,
        robot_code: impl Into<String>,
        api_base: Option<String>,
        oapi_base: Option<String>,
        on_message: Arc<
            dyn Fn(String, String, String, bool, Vec<crate::agent::registry::ImageAttachment>)
                + Send
                + Sync,
        >,
    ) -> Self {
        Self {
            app_key: app_key.into(),
            app_secret: app_secret.into(),
            robot_code: robot_code.into(),
            api_base: api_base.unwrap_or_else(|| DINGTALK_API_BASE.to_owned()),
            oapi_base: oapi_base.unwrap_or_else(|| DINGTALK_OAPI_BASE.to_owned()),
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            retry: RetryConfig::default(),
            token_cache: RwLock::new(None),
            on_message,
        }
    }

    // -----------------------------------------------------------------------
    // Access token management
    // -----------------------------------------------------------------------

    /// Get a valid access token, refreshing if expired.
    async fn get_access_token(&self) -> Result<String> {
        // Fast path: read lock.
        {
            let cache = self.token_cache.read().await;
            if let Some(ref cached) = *cache
                && !cached.is_expired()
            {
                return Ok(cached.token.clone());
            }
        }

        // Slow path: write lock + refresh.
        let mut cache = self.token_cache.write().await;

        // Double-check after acquiring write lock.
        if let Some(ref cached) = *cache
            && !cached.is_expired()
        {
            return Ok(cached.token.clone());
        }

        let url = format!(
            "{}/gettoken?appkey={}&appsecret={}",
            self.oapi_base, self.app_key, self.app_secret
        );

        let resp: TokenResponse = self
            .client
            .get(&url)
            .send()
            .await
            .context("DingTalk gettoken request")?
            .json()
            .await
            .context("DingTalk gettoken parse")?;

        if resp.errcode != 0 {
            bail!(
                "DingTalk gettoken error {}: {}",
                resp.errcode,
                resp.errmsg.unwrap_or_default()
            );
        }

        let token = resp.access_token.clone();
        *cache = Some(CachedToken {
            token: resp.access_token,
            obtained_at: Instant::now(),
            expires_in: Duration::from_secs(resp.expires_in),
        });

        info!(
            "DingTalk access token refreshed (expires in {}s)",
            resp.expires_in
        );
        Ok(token)
    }

    // -----------------------------------------------------------------------
    // Send message
    // -----------------------------------------------------------------------

    /// Send a text message to a user (1:1) via robot batch-send API.
    async fn send_text_to_user(&self, user_id: &str, text: &str) -> Result<()> {
        let token = self.get_access_token().await?;
        let url = format!("{}/v1.0/robot/oToMessages/batchSend", self.api_base);

        let body = json!({
            "robotCode": self.robot_code,
            "userIds": [user_id],
            "msgKey": "sampleText",
            "msgParam": serde_json::to_string(&json!({ "content": text }))?,
        });

        for attempt in 0..self.retry.attempts {
            let resp = self
                .client
                .post(&url)
                .header("x-acs-dingtalk-access-token", &token)
                .json(&body)
                .send()
                .await
                .context("DingTalk batchSend request")?;

            let status = resp.status();

            if status.as_u16() == 429 {
                let delay = backoff_delay(attempt, &self.retry);
                warn!(attempt, ?delay, "DingTalk rate limit, backing off");
                sleep(delay).await;
                continue;
            }

            if !status.is_success() {
                let err = resp.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!("DingTalk batchSend failed {status}: {err}"));
            }

            return Ok(());
        }

        Err(anyhow::anyhow!(
            "DingTalk batchSend failed after {} attempts",
            self.retry.attempts
        ))
    }

    /// Send a text message to a group conversation.
    async fn send_text_to_group(&self, open_conversation_id: &str, text: &str) -> Result<()> {
        let token = self.get_access_token().await?;
        let url = format!("{}/v1.0/robot/groupMessages/send", self.api_base);

        let body = json!({
            "robotCode": self.robot_code,
            "openConversationId": open_conversation_id,
            "msgKey": "sampleText",
            "msgParam": serde_json::to_string(&json!({ "content": text }))?,
        });

        for attempt in 0..self.retry.attempts {
            let resp = self
                .client
                .post(&url)
                .header("x-acs-dingtalk-access-token", &token)
                .json(&body)
                .send()
                .await
                .context("DingTalk groupMessages/send request")?;

            let status = resp.status();

            if status.as_u16() == 429 {
                let delay = backoff_delay(attempt, &self.retry);
                warn!(
                    attempt,
                    ?delay,
                    "DingTalk group send rate limit, backing off"
                );
                sleep(delay).await;
                continue;
            }

            if !status.is_success() {
                let err = resp.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!(
                    "DingTalk groupMessages/send failed {status}: {err}"
                ));
            }

            return Ok(());
        }

        Err(anyhow::anyhow!(
            "DingTalk groupMessages/send failed after {} attempts",
            self.retry.attempts
        ))
    }

    // -----------------------------------------------------------------------
    // Voice / file download
    // -----------------------------------------------------------------------

    /// Download a voice file from DingTalk via the robot messageFiles API.
    async fn download_voice(&self, download_code: &str) -> Result<Vec<u8>> {
        let token = self.get_access_token().await?;
        let url = format!("{}/v1.0/robot/messageFiles/download", self.api_base);

        let body = json!({
            "downloadCode": download_code,
            "robotCode": self.robot_code,
        });

        let resp = self
            .client
            .post(&url)
            .header("x-acs-dingtalk-access-token", &token)
            .json(&body)
            .send()
            .await
            .context("DingTalk messageFiles/download request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            bail!("DingTalk voice download failed {status}: {err}");
        }

        let download_info: FileDownloadResponse =
            resp.json().await.context("DingTalk voice download parse")?;

        let download_url = download_info
            .download_url
            .context("DingTalk voice download: no downloadUrl in response")?;

        let audio_bytes = self.client.get(&download_url).send().await?.bytes().await?;

        debug!(size = audio_bytes.len(), "DingTalk voice file downloaded");
        Ok(audio_bytes.to_vec())
    }

    /// Download and transcribe a voice message.
    async fn transcribe_voice(&self, download_code: &str) -> Result<String> {
        let audio_bytes = self.download_voice(download_code).await?;

        crate::channel::transcription::transcribe_audio(
            &self.client,
            &audio_bytes,
            "voice.amr",
            "audio/amr",
        )
        .await
    }

    /// Download a media file (picture/video/file) via DingTalk robot
    /// messageFiles API.
    async fn download_media_file(&self, download_code: &str) -> Result<Vec<u8>> {
        let token = self.get_access_token().await?;
        let url = format!("{}/v1.0/robot/messageFiles/download", self.api_base);

        let body = json!({
            "downloadCode": download_code,
            "robotCode": self.robot_code,
        });

        let resp = self
            .client
            .post(&url)
            .header("x-acs-dingtalk-access-token", &token)
            .json(&body)
            .send()
            .await
            .context("DingTalk media download request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            bail!("DingTalk media download failed {status}: {err}");
        }

        let download_info: FileDownloadResponse =
            resp.json().await.context("DingTalk media download parse")?;

        let download_url = download_info
            .download_url
            .context("DingTalk media download: no downloadUrl in response")?;

        let media_bytes = self.client.get(&download_url).send().await?.bytes().await?;
        debug!(size = media_bytes.len(), "DingTalk media file downloaded");
        Ok(media_bytes.to_vec())
    }

    // -----------------------------------------------------------------------
    // Stream Mode — WebSocket connection
    // -----------------------------------------------------------------------

    /// Open a Stream Mode connection and return the WebSocket endpoint +
    /// ticket.
    async fn open_stream_connection(&self) -> Result<(String, String)> {
        let token = self.get_access_token().await?;
        let url = format!("{}/v1.0/gateway/connections/open", self.api_base);

        let body = json!({
            "clientId": self.app_key,
            "clientSecret": self.app_secret,
            "subscriptions": [
                {
                    "type": "CALLBACK",
                    "topic": "/v1.0/im/bot/messages/get"
                },
                {
                    "type": "EVENT",
                    "topic": "*"
                }
            ],
        });

        let resp = self
            .client
            .post(&url)
            .header("x-acs-dingtalk-access-token", &token)
            .json(&body)
            .send()
            .await
            .context("DingTalk stream connection open")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            bail!("DingTalk stream open failed {status}: {err}");
        }

        let info: StreamConnectResponse = resp
            .json()
            .await
            .context("DingTalk stream connection parse")?;

        let endpoint = info
            .endpoint
            .context("DingTalk stream: no endpoint in response")?;
        let ticket = info
            .ticket
            .context("DingTalk stream: no ticket in response")?;

        Ok((endpoint, ticket))
    }

    /// Process a single inbound event from the Stream Mode WebSocket.
    async fn handle_stream_event(&self, data: &Value) {
        // DingTalk stream events have a "headers" + "data" structure.
        // The actual message payload is in "data".
        let payload = if let Some(d) = data.get("data") {
            // data field may be a JSON string that needs parsing.
            if let Some(s) = d.as_str() {
                match serde_json::from_str::<Value>(s) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("DingTalk: failed to parse event data string: {e}");
                        return;
                    }
                }
            } else {
                d.clone()
            }
        } else {
            data.clone()
        };

        // Extract message fields.
        let sender_id = payload
            .get("senderStaffId")
            .or_else(|| payload.get("senderId"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        let conversation_id = payload
            .get("conversationId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        let is_group = payload
            .get("conversationType")
            .and_then(|v| v.as_str())
            .map(|t| t == "2")
            .unwrap_or(false);

        let msg_type = payload
            .get("msgtype")
            .or_else(|| payload.get("msgType"))
            .and_then(|v| v.as_str())
            .unwrap_or("text");

        let mut images: Vec<crate::agent::registry::ImageAttachment> = Vec::new();

        let text = match msg_type {
            "text" => {
                // Text content can be in text.content or msgContent.
                let content = payload
                    .get("text")
                    .and_then(|t| t.get("content"))
                    .and_then(|v| v.as_str())
                    .or_else(|| payload.get("msgContent").and_then(|v| v.as_str()));

                match content {
                    Some(t) if !t.trim().is_empty() => t.trim().to_owned(),
                    _ => return,
                }
            }
            "audio" | "voice" => {
                let download_code = payload
                    .get("content")
                    .and_then(|c| c.get("downloadCode"))
                    .and_then(|v| v.as_str());

                match download_code {
                    Some(code) => match self.transcribe_voice(code).await {
                        Ok(t) => {
                            info!("DingTalk voice transcribed ({} chars)", t.len());
                            t
                        }
                        Err(e) => {
                            warn!("DingTalk voice transcription failed: {e:#}");
                            return;
                        }
                    },
                    None => {
                        warn!("DingTalk audio message missing downloadCode");
                        return;
                    }
                }
            }
            "picture" | "richText" => {
                let download_code = payload
                    .get("content")
                    .and_then(|c| c.get("downloadCode"))
                    .and_then(|v| v.as_str());

                match download_code {
                    Some(code) => match self.download_media_file(code).await {
                        Ok(bytes) => {
                            use base64::Engine;
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                            images.push(crate::agent::registry::ImageAttachment {
                                data: format!("data:image/png;base64,{b64}"),
                                mime_type: "image/png".to_owned(),
                            });
                            info!(size = bytes.len(), "DingTalk image downloaded");
                            crate::i18n::t("describe_image", crate::i18n::default_lang())
                        }
                        Err(e) => {
                            warn!("DingTalk image download failed: {e:#}");
                            return;
                        }
                    },
                    None => {
                        // Try direct picture URL from content
                        let pic_url = payload
                            .get("content")
                            .and_then(|c| {
                                c.get("pictureDownloadUrl").or_else(|| c.get("downloadUrl"))
                            })
                            .and_then(|v| v.as_str());
                        match pic_url {
                            Some(url) => match crate::channel::transcription::download_file(
                                &self.client,
                                url,
                            )
                            .await
                            {
                                Ok(bytes) => {
                                    use base64::Engine;
                                    let b64 =
                                        base64::engine::general_purpose::STANDARD.encode(&bytes);
                                    images.push(crate::agent::registry::ImageAttachment {
                                        data: format!("data:image/png;base64,{b64}"),
                                        mime_type: "image/png".to_owned(),
                                    });
                                    crate::i18n::t("describe_image", crate::i18n::default_lang())
                                }
                                Err(e) => {
                                    warn!("DingTalk image URL download failed: {e:#}");
                                    return;
                                }
                            },
                            None => {
                                warn!("DingTalk picture message missing downloadCode and URL");
                                return;
                            }
                        }
                    }
                }
            }
            "video" => {
                let download_code = payload
                    .get("content")
                    .and_then(|c| c.get("downloadCode"))
                    .and_then(|v| v.as_str());

                match download_code {
                    Some(code) => match self.download_media_file(code).await {
                        Ok(bytes) => {
                            match dingtalk_extract_audio_and_transcribe(&self.client, &bytes).await
                            {
                                Ok(t) if !t.is_empty() => {
                                    info!(chars = t.len(), "DingTalk video audio transcribed");
                                    t
                                }
                                Ok(_) => {
                                    warn!("DingTalk video transcription returned empty");
                                    return;
                                }
                                Err(e) => {
                                    warn!("DingTalk video transcription failed: {e:#}");
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            warn!("DingTalk video download failed: {e:#}");
                            return;
                        }
                    },
                    None => {
                        warn!("DingTalk video message missing downloadCode");
                        return;
                    }
                }
            }
            "file" => {
                let download_code = payload
                    .get("content")
                    .and_then(|c| c.get("downloadCode"))
                    .and_then(|v| v.as_str());
                let filename = payload
                    .get("content")
                    .and_then(|c| c.get("fileName"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("file");

                match download_code {
                    Some(code) => match self.download_media_file(code).await {
                        Ok(bytes) => {
                            if is_text_file(filename) {
                                match String::from_utf8(bytes) {
                                    Ok(content) => {
                                        info!(name = filename, "DingTalk text file received");
                                        format!("[File: {filename}]\n{content}")
                                    }
                                    Err(_) => {
                                        debug!("DingTalk file is not valid UTF-8: {filename}");
                                        return;
                                    }
                                }
                            } else {
                                debug!("DingTalk: non-text file ignored: {filename}");
                                return;
                            }
                        }
                        Err(e) => {
                            warn!("DingTalk file download failed: {e:#}");
                            return;
                        }
                    },
                    None => {
                        warn!("DingTalk file message missing downloadCode");
                        return;
                    }
                }
            }
            other => {
                debug!("DingTalk: ignoring message type '{other}'");
                return;
            }
        };

        if sender_id.is_empty() || (text.is_empty() && images.is_empty()) {
            return;
        }

        debug!(
            sender = %sender_id,
            conversation = %conversation_id,
            is_group,
            "DingTalk message received"
        );

        (self.on_message)(sender_id, text, conversation_id, is_group, images);
    }

    /// Run the Stream Mode WebSocket loop (reconnects on failure).
    async fn stream_loop(self: &Arc<Self>) -> Result<()> {
        loop {
            match self.run_single_stream().await {
                Ok(()) => {
                    info!("DingTalk stream connection closed, reconnecting...");
                }
                Err(e) => {
                    error!("DingTalk stream error: {e:#}");
                }
            }
            sleep(Duration::from_secs(5)).await;
        }
    }

    /// Run a single Stream Mode WebSocket connection until it closes or errors.
    async fn run_single_stream(self: &Arc<Self>) -> Result<()> {
        let (endpoint, ticket) = self.open_stream_connection().await?;

        // Append ticket as query parameter.
        let ws_url = if endpoint.contains('?') {
            format!("{}&ticket={}", endpoint, ticket)
        } else {
            format!("{}?ticket={}", endpoint, ticket)
        };

        info!(endpoint = %ws_url, "DingTalk Stream Mode connecting...");

        let (ws_stream, _) = connect_async(&ws_url)
            .await
            .context("DingTalk WebSocket connect")?;

        info!("DingTalk Stream Mode connected");

        let (mut write, mut read) = ws_stream.split();

        // Keep-alive ping interval.
        let ping_interval = Duration::from_secs(30);
        let mut ping_timer = tokio::time::interval(ping_interval);
        ping_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Skip the first immediate tick.
        ping_timer.tick().await;

        loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(WsMessage::Text(text))) => {
                            debug!(text_len = text.len(), preview = &text[..text.len().min(200)], "DingTalk: WS text frame received");
                            match serde_json::from_str::<Value>(&text) {
                                Ok(event) => {
                                    // Check if this is a system ping/pong.
                                    let event_type = event
                                        .get("headers")
                                        .and_then(|h| h.get("topic"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");

                                    if event_type == "ping" {
                                        // Respond with a pong ack.
                                        let msg_id = event
                                            .get("headers")
                                            .and_then(|h| h.get("messageId"))
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");
                                        let ack = json!({
                                            "code": 200,
                                            "headers": {
                                                "contentType": "application/json",
                                                "messageId": msg_id,
                                            },
                                            "message": "OK",
                                            "data": "",
                                        });
                                        let _ = write.send(WsMessage::Text(ack.to_string().into())).await;
                                        debug!("DingTalk: pong sent for messageId={msg_id}");
                                        continue;
                                    }

                                    // Send acknowledgment for the event.
                                    let msg_id = event
                                        .get("headers")
                                        .and_then(|h| h.get("messageId"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    if !msg_id.is_empty() {
                                        let ack = json!({
                                            "code": 200,
                                            "headers": {
                                                "contentType": "application/json",
                                                "messageId": msg_id,
                                            },
                                            "message": "OK",
                                            "data": "",
                                        });
                                        let _ = write.send(WsMessage::Text(ack.to_string().into())).await;
                                    }

                                    self.handle_stream_event(&event).await;
                                }
                                Err(e) => {
                                    warn!("DingTalk: invalid JSON from stream: {e}");
                                }
                            }
                        }
                        Some(Ok(WsMessage::Ping(data))) => {
                            let _ = write.send(WsMessage::Pong(data)).await;
                        }
                        Some(Ok(WsMessage::Close(_))) => {
                            info!("DingTalk: WebSocket close frame received");
                            break;
                        }
                        Some(Err(e)) => {
                            warn!("DingTalk: WebSocket read error: {e}");
                            break;
                        }
                        None => {
                            info!("DingTalk: WebSocket stream ended");
                            break;
                        }
                        _ => {}
                    }
                }
                _ = ping_timer.tick() => {
                    // Send a keep-alive ping.
                    if write.send(WsMessage::Ping(vec![].into())).await.is_err() {
                        warn!("DingTalk: failed to send ping, reconnecting");
                        break;
                    }
                }
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Channel trait implementation
// ---------------------------------------------------------------------------

impl Channel for DingTalkChannel {
    fn name(&self) -> &str {
        "dingtalk"
    }

    fn send(&self, msg: OutboundMessage) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let chunk_cfg = ChunkConfig {
                max_chars: DINGTALK_CHUNK_LIMIT,
                min_chars: 1,
                break_preference: BreakPreference::Paragraph,
            };
            let chunks = chunk_text(&msg.text, &chunk_cfg);

            for chunk in chunks.iter().filter(|c| !c.trim().is_empty()) {
                if msg.is_group {
                    self.send_text_to_group(&msg.target_id, chunk).await?;
                } else {
                    self.send_text_to_user(&msg.target_id, chunk).await?;
                }
            }

            for image_data in &msg.images {
                use base64::Engine;
                let (mime, b64) =
                    if let Some(rest) = image_data.strip_prefix("data:image/png;base64,") {
                        ("image/png", rest)
                    } else if let Some(rest) = image_data.strip_prefix("data:image/jpeg;base64,") {
                        ("image/jpeg", rest)
                    } else if let Some(rest) = image_data.strip_prefix("data:image/webp;base64,") {
                        ("image/webp", rest)
                    } else {
                        warn!("DingTalk: unrecognised image data URI prefix, skipping");
                        continue;
                    };

                let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
                    Ok(b) if !b.is_empty() => b,
                    _ => {
                        warn!("DingTalk: failed to decode base64 image, skipping");
                        continue;
                    }
                };

                let filename = if mime == "image/jpeg" {
                    "image.jpg"
                } else {
                    "image.png"
                };

                // Upload image to DingTalk via OAPI media/upload.
                // The endpoint requires the access_token as a query parameter.
                let token = self.get_access_token().await?;
                let part = match reqwest::multipart::Part::bytes(bytes)
                    .file_name(filename)
                    .mime_str(mime)
                {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("DingTalk: failed to build multipart part: {e}");
                        continue;
                    }
                };
                let form = reqwest::multipart::Form::new()
                    .text("type", "image")
                    .part("media", part);
                let upload_url = format!("{}/media/upload", self.oapi_base);
                let upload_resp = self
                    .client
                    .post(&upload_url)
                    .query(&[("access_token", token.as_str())])
                    .multipart(form)
                    .send()
                    .await;

                let media_id = match upload_resp {
                    Ok(r) => match r.json::<serde_json::Value>().await {
                        Ok(body) => {
                            if let Some(id) = body.get("media_id").and_then(|v| v.as_str()) {
                                id.to_owned()
                            } else {
                                warn!("DingTalk: media upload response missing media_id: {body}");
                                continue;
                            }
                        }
                        Err(e) => {
                            warn!("DingTalk: failed to parse media upload response: {e}");
                            continue;
                        }
                    },
                    Err(e) => {
                        warn!("DingTalk: media upload request failed: {e}");
                        continue;
                    }
                };

                // Send image message via robot.
                // DingTalk `sampleImageMsg` uses `photoURL` which accepts a media_id
                // returned by media/upload (valid for 3 days).
                let token2 = self.get_access_token().await?;
                let msg_param = json!({ "photoURL": media_id }).to_string();

                let send_result = if msg.is_group {
                    self.client
                        .post(format!("{}/v1.0/robot/groupMessages/send", self.api_base))
                        .header("x-acs-dingtalk-access-token", &token2)
                        .json(&json!({
                            "robotCode": self.robot_code,
                            "openConversationId": msg.target_id,
                            "msgKey": "sampleImageMsg",
                            "msgParam": msg_param,
                        }))
                        .send()
                        .await
                } else {
                    self.client
                        .post(format!(
                            "{}/v1.0/robot/oToMessages/batchSend",
                            self.api_base
                        ))
                        .header("x-acs-dingtalk-access-token", &token2)
                        .json(&json!({
                            "robotCode": self.robot_code,
                            "userIds": [msg.target_id],
                            "msgKey": "sampleImageMsg",
                            "msgParam": msg_param,
                        }))
                        .send()
                        .await
                };

                match send_result {
                    Ok(r) if r.status().is_success() => {
                        debug!("DingTalk: image message sent");
                    }
                    Ok(r) => {
                        let status = r.status();
                        let err = r.text().await.unwrap_or_default();
                        warn!("DingTalk: image send failed {status}: {err}");
                    }
                    Err(e) => {
                        warn!("DingTalk: image send request failed: {e}");
                    }
                }
            }

            Ok(())
        })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            info!("DingTalk Stream Mode loop started");
            self.stream_loop().await
        })
    }
}

// ---------------------------------------------------------------------------
// Retry helper (re-use from telegram module)
// ---------------------------------------------------------------------------

fn backoff_delay(attempt: u32, config: &RetryConfig) -> Duration {
    let base = config.min_delay_ms as f64 * 2f64.powi(attempt as i32);
    let clamped = base.min(config.max_delay_ms as f64);
    let jitter = clamped * config.jitter * rand::random::<f64>();
    Duration::from_millis((clamped + jitter) as u64)
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
async fn dingtalk_extract_audio_and_transcribe(
    client: &Client,
    video_bytes: &[u8],
) -> Result<String> {
    let tmp_dir = std::env::temp_dir();
    let video_path = tmp_dir.join(format!("rsclaw_dt_video_{}.mp4", uuid::Uuid::new_v4()));
    let audio_path = tmp_dir.join(format!("rsclaw_dt_video_{}.ogg", uuid::Uuid::new_v4()));

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
        bail!("ffmpeg failed to extract audio from video");
    }

    let audio_bytes = std::fs::read(&audio_path)?;
    let _ = std::fs::remove_file(&audio_path);

    crate::channel::transcription::transcribe_audio(
        client,
        &audio_bytes,
        "video_audio.ogg",
        "audio/ogg",
    )
    .await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_name() {
        let ch = DingTalkChannel::new(
            "key",
            "secret",
            "robot_code",
            None,
            None,
            Arc::new(|_, _, _, _, _| {}),
        );
        assert_eq!(ch.name(), "dingtalk");
    }

    #[test]
    fn chunk_limit() {
        assert_eq!(DINGTALK_CHUNK_LIMIT, 20_000);
    }

    #[test]
    fn token_expiry_check() {
        let cached = CachedToken {
            token: "test".to_owned(),
            obtained_at: Instant::now() - Duration::from_secs(7200),
            expires_in: Duration::from_secs(7200),
        };
        assert!(
            cached.is_expired(),
            "token obtained 2h ago should be expired"
        );

        let fresh = CachedToken {
            token: "test".to_owned(),
            obtained_at: Instant::now(),
            expires_in: Duration::from_secs(7200),
        };
        assert!(
            !fresh.is_expired(),
            "freshly obtained token should not be expired"
        );
    }

    #[test]
    fn backoff_increases() {
        let cfg = RetryConfig::default();
        let d0 = backoff_delay(0, &cfg).as_millis();
        let d1 = backoff_delay(1, &cfg).as_millis();
        assert!(d1 >= d0, "backoff should increase");
    }
}
