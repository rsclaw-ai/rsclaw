//! QQ Official Bot API channel.
//!
//! Connects to the QQ Bot Open Platform via WebSocket gateway to receive
//! messages, and uses the REST API to send replies.
//!
//! Supports:
//!   - Group @bot messages (GROUP_AT_MESSAGE_CREATE)
//!   - C2C direct messages (C2C_MESSAGE_CREATE)
//!   - Guild channel @bot messages (AT_MESSAGE_CREATE)
//!   - Guild direct messages (DIRECT_MESSAGE_CREATE)
//!
//! Authentication: AppID + AppSecret -> access_token (2h TTL, auto-refresh).
//!
//! Key constraint (2025-04 policy): bots can only send passive replies
//! (must include msg_id from the received event). Active push is disabled.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt, future::BoxFuture};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use super::{Channel, OutboundMessage};
use crate::channel::chunker::{ChunkConfig, chunk_text, platform_chunk_limit};

// ---------------------------------------------------------------------------
// API endpoints
// ---------------------------------------------------------------------------

const API_BASE: &str = "https://api.sgroup.qq.com";
const SANDBOX_API_BASE: &str = "https://sandbox.api.sgroup.qq.com";
const TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";

// Intent bits
const INTENT_GUILDS: u32 = 1 << 0;
const INTENT_PUBLIC_GUILD_MESSAGES: u32 = 1 << 30;
const INTENT_GUILD_DM: u32 = 1 << 12;
const INTENT_GROUP_AND_C2C: u32 = 1 << 25;

/// Default intents: guilds + guild @bot + guild DM + group/C2C.
const DEFAULT_INTENTS: u32 =
    INTENT_GUILDS | INTENT_PUBLIC_GUILD_MESSAGES | INTENT_GUILD_DM | INTENT_GROUP_AND_C2C;

/// Token refresh margin: refresh 5 minutes before expiry.
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// API response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(deserialize_with = "de_string_or_u64")]
    expires_in: u64,
}

fn de_string_or_u64<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<u64, D::Error> {
    use serde::de;
    struct Visitor;
    impl de::Visitor<'_> for Visitor {
        type Value = u64;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("u64 or string")
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<u64, E> { Ok(v) }
        fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<u64, E> {
            v.parse().map_err(de::Error::custom)
        }
    }
    d.deserialize_any(Visitor)
}

#[derive(Debug, Deserialize)]
struct GatewayResponse {
    url: String,
}

// ---------------------------------------------------------------------------
// Token cache
// ---------------------------------------------------------------------------

struct TokenCache {
    token: String,
    expires_at: Instant,
}

// ---------------------------------------------------------------------------
// QQBotChannel
// ---------------------------------------------------------------------------

pub struct QQBotChannel {
    app_id: String,
    app_secret: String,
    api_base: String,
    /// Token endpoint URL (overrideable for testing).
    token_url: String,
    intents: u32,
    client: Client,
    token_cache: RwLock<Option<TokenCache>>,
    /// Callback: (sender_id, text, target_id, is_group, msg_id, images, files).
    /// msg_id is required for passive replies.
    #[allow(clippy::type_complexity)]
    on_message: Arc<dyn Fn(String, String, String, bool, String, Vec<crate::agent::registry::ImageAttachment>, Vec<crate::agent::registry::FileAttachment>) + Send + Sync>,
}

impl QQBotChannel {
    #[allow(clippy::type_complexity)]
    pub fn new(
        app_id: impl Into<String>,
        app_secret: impl Into<String>,
        sandbox: bool,
        intents: Option<u32>,
        on_message: Arc<dyn Fn(String, String, String, bool, String, Vec<crate::agent::registry::ImageAttachment>, Vec<crate::agent::registry::FileAttachment>) + Send + Sync>,
    ) -> Self {
        Self::new_with_overrides(app_id, app_secret, sandbox, intents, on_message, None, None)
    }

    #[allow(clippy::type_complexity)]
    pub fn new_with_overrides(
        app_id: impl Into<String>,
        app_secret: impl Into<String>,
        sandbox: bool,
        intents: Option<u32>,
        on_message: Arc<dyn Fn(String, String, String, bool, String, Vec<crate::agent::registry::ImageAttachment>, Vec<crate::agent::registry::FileAttachment>) + Send + Sync>,
        api_base_override: Option<String>,
        token_url_override: Option<String>,
    ) -> Self {
        Self {
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            api_base: api_base_override.unwrap_or_else(|| {
                if sandbox {
                    SANDBOX_API_BASE.to_owned()
                } else {
                    API_BASE.to_owned()
                }
            }),
            token_url: token_url_override.unwrap_or_else(|| TOKEN_URL.to_owned()),
            intents: intents.unwrap_or(DEFAULT_INTENTS),
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("http client"),
            token_cache: RwLock::new(None),
            on_message,
        }
    }

    // -----------------------------------------------------------------------
    // Token management
    // -----------------------------------------------------------------------

    async fn get_token(&self) -> Result<String> {
        {
            let cache = self.token_cache.read().await;
            if let Some(ref tc) = *cache
                && Instant::now() < tc.expires_at
            {
                return Ok(tc.token.clone());
            }
        }
        self.refresh_token().await
    }

    async fn refresh_token(&self) -> Result<String> {
        let resp: TokenResponse = self
            .client
            .post(self.token_url.as_str())
            .json(&json!({
                "appId": self.app_id,
                "clientSecret": self.app_secret,
            }))
            .send()
            .await
            .context("qq: token request failed")?
            .json()
            .await
            .context("qq: parse token response")?;

        let expires_at =
            Instant::now() + Duration::from_secs(resp.expires_in) - TOKEN_REFRESH_MARGIN;
        let token = resp.access_token.clone();

        *self.token_cache.write().await = Some(TokenCache {
            token: token.clone(),
            expires_at,
        });

        info!(expires_in = resp.expires_in, "qq: access token refreshed");
        Ok(token)
    }

    // -----------------------------------------------------------------------
    // Send message
    // -----------------------------------------------------------------------

    async fn send_text(
        &self,
        target_id: &str,
        text: &str,
        is_group: bool,
        msg_id: &str,
    ) -> Result<()> {
        let token = self.get_token().await?;

        let url = if is_group {
            format!("{}/v2/groups/{}/messages", self.api_base, target_id)
        } else {
            // C2C direct message
            format!("{}/v2/users/{}/messages", self.api_base, target_id)
        };

        let body = json!({
            "content": text,
            "msg_type": 0,
            "msg_id": msg_id,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("QQBot {token}"))
            .json(&body)
            .send()
            .await
            .context("qq: send message request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("qq: send_message failed {status}: {body}");
        }

        debug!(target_id, is_group, "qq: message sent");
        Ok(())
    }

    /// Send to a guild channel (different endpoint format).
    async fn send_guild_text(&self, channel_id: &str, text: &str, msg_id: &str) -> Result<()> {
        let token = self.get_token().await?;
        let url = format!("{}/channels/{}/messages", self.api_base, channel_id);

        let body = json!({
            "content": text,
            "msg_id": msg_id,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("QQBot {token}"))
            .json(&body)
            .send()
            .await
            .context("qq: guild send failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("qq: guild send failed {status}: {body}");
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // WebSocket gateway
    // -----------------------------------------------------------------------

    async fn get_gateway_url(&self) -> Result<String> {
        let token = self.get_token().await?;
        let resp: GatewayResponse = self
            .client
            .get(format!("{}/gateway/bot", self.api_base))
            .header("Authorization", format!("QQBot {token}"))
            .send()
            .await
            .context("qq: gateway request failed")?
            .json()
            .await
            .context("qq: parse gateway response")?;

        Ok(resp.url)
    }

    async fn ws_connect_loop(&self) -> Result<()> {
        let gateway_url = self.get_gateway_url().await?;
        info!(url = %gateway_url, "qq: connecting to WebSocket gateway");

        let (ws_stream, _) = tokio_tungstenite::connect_async(&gateway_url)
            .await
            .context("qq: WebSocket connect failed")?;

        let (mut write, mut read) = ws_stream.split();

        // 1. Wait for Hello (op=10)
        let heartbeat_interval = match read.next().await {
            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                let val: Value = serde_json::from_str(&text).context("qq: parse hello")?;
                if val["op"].as_u64() != Some(10) {
                    bail!("qq: expected Hello (op=10), got: {text}");
                }
                val["d"]["heartbeat_interval"].as_u64().unwrap_or(41250)
            }
            other => bail!("qq: unexpected first frame: {other:?}"),
        };

        info!(heartbeat_interval, "qq: received Hello");

        // 2. Send Identify (op=2)
        let token = self.get_token().await?;
        let identify = json!({
            "op": 2,
            "d": {
                "token": format!("QQBot {token}"),
                "intents": self.intents,
                "shard": [0, 1],
                "properties": {},
            }
        });
        write
            .send(tokio_tungstenite::tungstenite::Message::Text(
                serde_json::to_string(&identify)?.into(),
            ))
            .await
            .context("qq: send identify")?;

        // 3. Wait for Ready (op=0, t=READY)
        let session_id = match read.next().await {
            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                let val: Value = serde_json::from_str(&text).context("qq: parse ready")?;
                if val["t"].as_str() != Some("READY") {
                    bail!("qq: expected READY, got: {}", val["t"]);
                }
                val["d"]["session_id"].as_str().unwrap_or("").to_owned()
            }
            other => bail!("qq: unexpected frame waiting for READY: {other:?}"),
        };

        info!(session_id, "qq: WebSocket ready");

        // 4. Heartbeat + event loop
        let mut last_seq: Option<u64> = None;
        let heartbeat_dur = Duration::from_millis(heartbeat_interval);
        let mut heartbeat_timer = tokio::time::interval(heartbeat_dur);
        heartbeat_timer.tick().await; // skip first immediate tick

        loop {
            tokio::select! {
                _ = heartbeat_timer.tick() => {
                    let hb = json!({
                        "op": 1,
                        "d": last_seq,
                    });
                    if write.send(tokio_tungstenite::tungstenite::Message::Text(
                        serde_json::to_string(&hb)?.into(),
                    )).await.is_err() {
                        warn!("qq: heartbeat send failed, reconnecting");
                        break;
                    }
                }

                msg = read.next() => {
                    match msg {
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                            let val: Value = match serde_json::from_str(&text) {
                                Ok(v) => v,
                                Err(e) => {
                                    warn!("qq: invalid JSON frame: {e}");
                                    continue;
                                }
                            };

                            let op = val["op"].as_u64().unwrap_or(0);
                            match op {
                                0 => {
                                    // Dispatch event
                                    if let Some(s) = val["s"].as_u64() {
                                        last_seq = Some(s);
                                    }
                                    self.handle_dispatch(&val).await;
                                }
                                11 => {
                                    // Heartbeat ACK
                                    debug!("qq: heartbeat ACK");
                                }
                                7 => {
                                    // Reconnect requested
                                    info!("qq: server requested reconnect");
                                    break;
                                }
                                9 => {
                                    // Invalid session
                                    warn!("qq: invalid session, reconnecting");
                                    break;
                                }
                                _ => {
                                    debug!(op, "qq: unhandled opcode");
                                }
                            }
                        }
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => {
                            info!("qq: WebSocket closed by server");
                            break;
                        }
                        Some(Err(e)) => {
                            warn!("qq: WebSocket error: {e}");
                            break;
                        }
                        None => {
                            info!("qq: WebSocket stream ended");
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Event dispatch
    // -----------------------------------------------------------------------

    async fn handle_dispatch(&self, val: &Value) {
        let event_type = val["t"].as_str().unwrap_or("");
        let data = &val["d"];

        match event_type {
            "GROUP_AT_MESSAGE_CREATE" => {
                // Group @bot message
                let sender = data["author"]["member_openid"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let mut text = data["content"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let group_openid = data["group_openid"].as_str().unwrap_or_default().to_owned();
                let msg_id = data["id"].as_str().unwrap_or_default().to_owned();

                let (images, files) = self.process_attachments(data, &mut text).await;

                if !text.is_empty() || !images.is_empty() || !files.is_empty() {
                    info!(sender = %sender, group = %group_openid, "qq: group message received");
                    (self.on_message)(sender, text, group_openid, true, msg_id, images, files);
                }
            }
            "C2C_MESSAGE_CREATE" => {
                // Direct message
                let sender = data["author"]["user_openid"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let mut text = data["content"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let msg_id = data["id"].as_str().unwrap_or_default().to_owned();

                let has_attachments = data.get("attachments").is_some();
                let (images, files) = self.process_attachments(data, &mut text).await;
                info!(
                    sender = %sender,
                    text_len = text.len(),
                    has_attachments,
                    images = images.len(),
                    "qq: C2C message received"
                );

                if !text.is_empty() || !images.is_empty() || !files.is_empty() {
                    (self.on_message)(sender.clone(), text, sender, false, msg_id, images, files);
                }
            }
            "AT_MESSAGE_CREATE" => {
                // Guild channel @bot message
                let sender = data["author"]["id"].as_str().unwrap_or_default().to_owned();
                let mut text = data["content"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let channel_id = data["channel_id"].as_str().unwrap_or_default().to_owned();
                let msg_id = data["id"].as_str().unwrap_or_default().to_owned();

                let (images, files) = self.process_attachments(data, &mut text).await;

                if !text.is_empty() || !images.is_empty() || !files.is_empty() {
                    info!(sender = %sender, channel = %channel_id, "qq: guild message received");
                    // Prefix channel_id with "guild:" to distinguish from group openid
                    (self.on_message)(sender, text, format!("guild:{channel_id}"), false, msg_id, images, files);
                }
            }
            "DIRECT_MESSAGE_CREATE" => {
                // Guild DM
                let sender = data["author"]["id"].as_str().unwrap_or_default().to_owned();
                let mut text = data["content"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let guild_id = data["guild_id"].as_str().unwrap_or_default().to_owned();
                let msg_id = data["id"].as_str().unwrap_or_default().to_owned();

                let (images, files) = self.process_attachments(data, &mut text).await;

                if !text.is_empty() || !images.is_empty() || !files.is_empty() {
                    info!(sender = %sender, "qq: guild DM received");
                    (self.on_message)(sender, text, format!("guild_dm:{guild_id}"), false, msg_id, images, files);
                }
            }
            "RESUMED" => {
                info!("qq: session resumed");
            }
            _ => {
                debug!(event_type, "qq: unhandled event");
            }
        }
    }

    /// Process attachments (image/audio/video/file) from a QQ message.
    async fn process_attachments(
        &self,
        data: &Value,
        text: &mut String,
    ) -> (Vec<crate::agent::registry::ImageAttachment>, Vec<crate::agent::registry::FileAttachment>) {
        let mut images = Vec::new();
        let mut file_attachments: Vec<crate::agent::registry::FileAttachment> = Vec::new();
        let attachments = match data.get("attachments").and_then(|v| v.as_array()) {
            Some(a) => a,
            None => return (images, file_attachments),
        };

        for att in attachments {
            let url = att.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let content_type = att.get("content_type").and_then(|v| v.as_str()).unwrap_or("");
            let filename = att.get("filename").and_then(|v| v.as_str()).unwrap_or("");
            if url.is_empty() {
                continue;
            }

            // Ensure URL has scheme (QQ sometimes returns scheme-less URLs)
            let full_url = if url.starts_with("//") {
                format!("https:{url}")
            } else {
                url.to_owned()
            };

            info!(url = %full_url, content_type, filename, "qq: processing attachment");

            if super::is_image_attachment(content_type, filename) {
                // Download and encode for vision
                match crate::channel::transcription::download_file(&self.client, &full_url).await {
                    Ok(bytes) => {
                        use base64::Engine;
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        images.push(crate::agent::registry::ImageAttachment {
                            data: format!("data:{content_type};base64,{b64}"),
                            mime_type: content_type.to_owned(),
                        });
                        if text.is_empty() {
                            *text = crate::i18n::t("describe_image", crate::i18n::default_lang());
                        }
                    }
                    Err(e) => warn!("qq: failed to download image: {e:#}"),
                }
            } else if super::is_audio_attachment(content_type, filename) {
                // Transcribe voice message
                match crate::channel::transcription::download_file(&self.client, &full_url).await {
                    Ok(bytes) => {
                        match crate::channel::transcription::transcribe_audio(
                            &self.client,
                            &bytes,
                            "voice.ogg",
                            content_type,
                        )
                        .await
                        {
                            Ok(t) if !t.is_empty() => {
                                info!(chars = t.len(), "qq: voice transcribed");
                                *text = t;
                            }
                            Ok(_) => warn!("qq: voice transcription returned empty"),
                            Err(e) => warn!("qq: voice transcription failed: {e:#}"),
                        }
                    }
                    Err(e) => warn!("qq: failed to download audio: {e:#}"),
                }
            } else if super::is_video_attachment(content_type, filename) {
                // Send as FileAttachment — runtime decides vision vs transcription
                match crate::channel::transcription::download_file(&self.client, &full_url).await {
                    Ok(bytes) => {
                        info!(size = bytes.len(), "qq: video downloaded");
                        file_attachments.push(crate::agent::registry::FileAttachment {
                            filename: filename.to_owned(),
                            data: bytes,
                            mime_type: content_type.to_owned(),
                        });
                    }
                    Err(e) => warn!("qq: failed to download video: {e:#}"),
                }
            } else {
                // File attachment -- route through agent file handling.
                let fname = if filename.is_empty() { "file.bin" } else { filename };
                match crate::channel::transcription::download_file(&self.client, &full_url).await {
                    Ok(bytes) => {
                        info!(size = bytes.len(), fname, "qq: file downloaded");
                        let mime = if content_type == "file" || content_type.is_empty() {
                            "application/octet-stream"
                        } else {
                            content_type
                        };
                        file_attachments.push(crate::agent::registry::FileAttachment {
                            filename: fname.to_owned(),
                            data: bytes,
                            mime_type: mime.to_owned(),
                        });
                    }
                    Err(e) => warn!("qq: failed to download file: {e:#}"),
                }
            }
        }

        (images, file_attachments)
    }
}

// ---------------------------------------------------------------------------
// Channel trait impl
// ---------------------------------------------------------------------------

impl Channel for QQBotChannel {
    fn name(&self) -> &str {
        "qq"
    }

    fn send(&self, msg: OutboundMessage) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let msg_id = msg.reply_to.as_deref().unwrap_or("");

            let chunk_cfg = ChunkConfig {
                max_chars: platform_chunk_limit("qq"),
                min_chars: 1,
                break_preference: super::chunker::BreakPreference::Paragraph,
            };
            let chunks = chunk_text(&msg.text, &chunk_cfg);

            for chunk in chunks.iter().filter(|c| !c.trim().is_empty()) {
                if msg.target_id.starts_with("guild:") {
                    let channel_id = msg
                        .target_id
                        .strip_prefix("guild:")
                        .unwrap_or(&msg.target_id);
                    self.send_guild_text(channel_id, chunk, msg_id).await?;
                } else if msg.target_id.starts_with("guild_dm:") {
                    // Guild DM uses the same guild channel endpoint
                    let guild_id = msg
                        .target_id
                        .strip_prefix("guild_dm:")
                        .unwrap_or(&msg.target_id);
                    let token = self.get_token().await?;
                    let url = format!("{}/dms/{}/messages", self.api_base, guild_id);
                    let body = json!({
                        "content": chunk,
                        "msg_id": msg_id,
                    });
                    let resp = self
                        .client
                        .post(&url)
                        .header("Authorization", format!("QQBot {token}"))
                        .json(&body)
                        .send()
                        .await
                        .context("qq: guild DM send failed")?;
                    if !resp.status().is_success() {
                        let body = resp.text().await.unwrap_or_default();
                        bail!("qq: guild DM send failed: {body}");
                    }
                } else {
                    self.send_text(&msg.target_id, chunk, msg.is_group, msg_id)
                        .await?;
                }
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
                    warn!("qq: unrecognised image data URI prefix, skipping");
                    continue;
                };

                let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
                    Ok(b) if !b.is_empty() => b,
                    _ => {
                        warn!("qq: failed to decode base64 image, skipping");
                        continue;
                    }
                };

                let filename = if mime == "image/jpeg" { "image.jpg" } else { "image.png" };

                // QQ Bot API: two-step image send:
                // 1. POST /v2/users|groups/{id}/files -> get file_info
                // 2. POST /v2/users|groups/{id}/messages with msg_type=7 + media
                let token = match self.get_token().await {
                    Ok(t) => t,
                    Err(e) => {
                        warn!("qq: failed to get token for image send: {e}");
                        continue;
                    }
                };

                // Step 1: upload file to get file_info
                let upload_url = if msg.is_group {
                    format!("{}/v2/groups/{}/files", self.api_base, msg.target_id)
                } else {
                    format!("{}/v2/users/{}/files", self.api_base, msg.target_id)
                };
                let upload_body = json!({
                    "file_type": 1,  // 1 = image
                    "file_data": base64::engine::general_purpose::STANDARD.encode(&bytes),
                    "srv_send_msg": false,
                });
                let upload_resp = match self
                    .client
                    .post(&upload_url)
                    .header("Authorization", format!("QQBot {token}"))
                    .json(&upload_body)
                    .send()
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("qq: image upload request failed: {e}");
                        continue;
                    }
                };
                let upload_status = upload_resp.status();
                let upload_text = upload_resp.text().await.unwrap_or_default();
                if !upload_status.is_success() {
                    warn!("qq: image upload failed {upload_status}: {upload_text}");
                    continue;
                }
                info!(response = &upload_text[..upload_text.len().min(500)], "qq: image upload response");
                let file_info: serde_json::Value = match serde_json::from_str(&upload_text) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("qq: failed to parse upload response: {e}");
                        continue;
                    }
                };

                // Step 2: send message with media reference
                let send_url = if msg.is_group {
                    format!("{}/v2/groups/{}/messages", self.api_base, msg.target_id)
                } else {
                    format!("{}/v2/users/{}/messages", self.api_base, msg.target_id)
                };
                // QQ API expects media as { file_info: "..." } where file_info is the string value
                let file_info_str = file_info.get("file_info")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let send_body = json!({
                    "msg_type": 7,
                    "media": {
                        "file_info": file_info_str,
                    },
                    "msg_id": msg_id,
                });
                debug!(body = %serde_json::to_string(&send_body).unwrap_or_default(), "qq: image send body");
                match self
                    .client
                    .post(&send_url)
                    .header("Authorization", format!("QQBot {token}"))
                    .json(&send_body)
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => {
                        debug!("qq: image message sent");
                    }
                    Ok(r) => {
                        let status = r.status();
                        let err = r.text().await.unwrap_or_default();
                        warn!("qq: image message send failed {status}: {err}");
                    }
                    Err(e) => {
                        warn!("qq: image message send request failed: {e}");
                    }
                }
            }

            Ok(())
        })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            loop {
                match self.ws_connect_loop().await {
                    Ok(()) => {
                        info!("qq: WebSocket disconnected, reconnecting in 5s");
                    }
                    Err(e) => {
                        error!("qq: WebSocket error: {e:#}, reconnecting in 10s");
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        continue;
                    }
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
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
async fn extract_audio_and_transcribe(client: &Client, video_bytes: &[u8]) -> Result<String> {
    let tmp_dir = std::env::temp_dir();
    let video_path = tmp_dir.join(format!("rsclaw_video_{}.mp4", uuid::Uuid::new_v4()));
    let audio_path = tmp_dir.join(format!("rsclaw_video_{}.ogg", uuid::Uuid::new_v4()));

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

    crate::channel::transcription::transcribe_audio(client, &audio_bytes, "video_audio.ogg", "audio/ogg").await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qq_chunk_limit() {
        let limit = platform_chunk_limit("qq");
        assert!(
            limit >= 2000,
            "qq chunk limit should be >= 2000, got {limit}"
        );
    }

    #[test]
    fn default_intents_cover_all_message_types() {
        assert_ne!(DEFAULT_INTENTS & INTENT_GROUP_AND_C2C, 0);
        assert_ne!(DEFAULT_INTENTS & INTENT_PUBLIC_GUILD_MESSAGES, 0);
        assert_ne!(DEFAULT_INTENTS & INTENT_GUILD_DM, 0);
    }
}
