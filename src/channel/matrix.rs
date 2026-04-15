//! Matrix channel driver.
//!
//! Two implementations:
//! - `channel-matrix` feature: uses matrix-sdk with E2EE support
//! - default: lightweight reqwest-based (no E2EE, unencrypted rooms only)

// =========================================================================
// matrix-sdk implementation (feature = "channel-matrix")
// =========================================================================

#[cfg(feature = "channel-matrix")]
use std::{sync::Arc, time::Duration, path::PathBuf};

#[cfg(feature = "channel-matrix")]
use anyhow::{Context, Result};
#[cfg(feature = "channel-matrix")]
use futures::future::BoxFuture;
#[cfg(feature = "channel-matrix")]
use matrix_sdk::{
    config::SyncSettings,
    matrix_auth::{MatrixSession, MatrixSessionTokens},
    ruma::events::room::message::{
        FileMessageEventContent, ImageMessageEventContent, MessageType,
        OriginalSyncRoomMessageEvent, RoomMessageEventContent,
    },
    Client as MatrixSdkClient, Room, RoomState, SessionMeta,
};
#[cfg(feature = "channel-matrix")]
use tracing::{debug, info, warn};

#[cfg(feature = "channel-matrix")]
use super::{Channel, OutboundMessage};
#[cfg(feature = "channel-matrix")]
use crate::channel::chunker::{BreakPreference, ChunkConfig, chunk_text, platform_chunk_limit};

#[cfg(feature = "channel-matrix")]
pub struct MatrixChannel {
    homeserver: String,
    access_token: String,
    user_id: String,
    device_id: Option<String>,
    recovery_key: Option<String>,
    store_path: PathBuf,
    on_message: Arc<dyn Fn(String, String, String, bool, Vec<crate::agent::registry::ImageAttachment>, Vec<crate::agent::registry::FileAttachment>) + Send + Sync>,
    client: Arc<tokio::sync::OnceCell<MatrixSdkClient>>,
}

#[cfg(feature = "channel-matrix")]
impl MatrixChannel {
    pub fn new(
        homeserver: impl Into<String>,
        access_token: impl Into<String>,
        user_id: impl Into<String>,
        on_message: Arc<dyn Fn(String, String, String, bool, Vec<crate::agent::registry::ImageAttachment>, Vec<crate::agent::registry::FileAttachment>) + Send + Sync>,
    ) -> Self {
        let base = crate::config::loader::base_dir();
        Self {
            homeserver: homeserver.into().trim_end_matches('/').to_owned(),
            access_token: access_token.into(),
            user_id: user_id.into(),
            device_id: None,
            recovery_key: None,
            store_path: base.join("var/data/matrix-store"),
            on_message,
            client: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    pub fn with_device_id(mut self, device_id: impl Into<String>) -> Self {
        self.device_id = Some(device_id.into());
        self
    }

    pub fn with_recovery_key(mut self, key: impl Into<String>) -> Self {
        self.recovery_key = Some(key.into());
        self
    }

    async fn get_client(&self) -> Result<MatrixSdkClient> {
        let client = self.client.get_or_try_init(|| async {
            // Create store dir
            tokio::fs::create_dir_all(&self.store_path).await?;

            // Build client with sqlite store for E2EE crypto state
            let client = MatrixSdkClient::builder()
                .homeserver_url(&self.homeserver)
                .sqlite_store(&self.store_path, None)
                .build()
                .await
                .context("Matrix: failed to build SDK client")?;

            // Restore session from access token
            let user_id: matrix_sdk::ruma::OwnedUserId = self.user_id.parse()
                .map_err(|e| anyhow::anyhow!("Matrix: invalid user_id: {e}"))?;

            let device_id = self.device_id.clone()
                .unwrap_or_else(|| {
                    let local = &self.user_id[1..self.user_id.find(':').unwrap_or(self.user_id.len())];
                    format!("RSCLAW_{}", local.to_uppercase())
                });

            let session = MatrixSession {
                meta: SessionMeta {
                    user_id,
                    device_id: device_id.into(),
                },
                tokens: MatrixSessionTokens {
                    access_token: self.access_token.clone(),
                    refresh_token: None,
                },
            };
            client.matrix_auth().restore_session(session).await
                .context("Matrix: failed to restore session")?;
            info!("Matrix: session restored");

            // E2EE key recovery if configured
            if let Some(ref key) = self.recovery_key {
                match client.encryption().recovery().recover(key).await {
                    Ok(()) => info!("Matrix: E2EE recovery successful"),
                    Err(e) => warn!("Matrix: E2EE recovery failed: {e} (unencrypted rooms still work)"),
                }
            }

            Ok::<MatrixSdkClient, anyhow::Error>(client)
        }).await?;

        Ok(client.clone())
    }
}

#[cfg(feature = "channel-matrix")]
impl Channel for MatrixChannel {
    fn name(&self) -> &str { "matrix" }

    fn send(&self, msg: OutboundMessage) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let client = self.get_client().await?;
            let room_id: matrix_sdk::ruma::OwnedRoomId = msg.target_id.parse()
                .map_err(|e| anyhow::anyhow!("Matrix: invalid room_id: {e}"))?;
            let room = client.get_room(&room_id)
                .ok_or_else(|| anyhow::anyhow!("Matrix: room not found: {}", msg.target_id))?;

            let chunk_cfg = ChunkConfig {
                max_chars: platform_chunk_limit("matrix"),
                min_chars: 1,
                break_preference: BreakPreference::Paragraph,
            };
            for chunk in &chunk_text(&msg.text, &chunk_cfg) {
                let content = RoomMessageEventContent::text_plain(chunk);
                room.send(content).await
                    .context("Matrix: send message failed")?;
            }

            // Send images via matrix-sdk upload + m.image message
            if !msg.images.is_empty() {
                info!(count = msg.images.len(), "matrix: sending images");
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
                        warn!(idx, "Matrix SDK: base64 decode failed: {e}");
                        continue;
                    }
                };
                let mime = if image_data.starts_with("data:image/jpeg") {
                    mime::IMAGE_JPEG
                } else {
                    mime::IMAGE_PNG
                };
                let upload_resp = client.media().upload(&mime, bytes, None).await;
                match upload_resp {
                    Ok(resp) => {
                        let content_uri = resp.content_uri;
                        let img_content = ImageMessageEventContent::plain(
                            "image.png".to_owned(),
                            content_uri,
                        );
                        let msg_content = RoomMessageEventContent::new(
                            MessageType::Image(img_content),
                        );
                        if let Err(e) = room.send(msg_content).await {
                            warn!(idx, "Matrix SDK: image send failed: {e}");
                        }
                    }
                    Err(e) => {
                        warn!(idx, "Matrix SDK: image upload failed: {e}");
                    }
                }
            }

            // Send file attachments via matrix-sdk upload + m.file message
            for (idx, (filename, mime_str, path_or_url)) in msg.files.iter().enumerate() {
                let bytes = if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
                    match reqwest::Client::new().get(path_or_url.as_str()).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            match resp.bytes().await {
                                Ok(b) if !b.is_empty() => b.to_vec(),
                                _ => { warn!(idx, "Matrix SDK: empty file download"); continue; }
                            }
                        }
                        _ => { warn!(idx, "Matrix SDK: file download failed"); continue; }
                    }
                } else {
                    match std::fs::read(path_or_url) {
                        Ok(b) => b,
                        Err(e) => { warn!(idx, "Matrix SDK: read file failed: {e}"); continue; }
                    }
                };

                let mime: mime::Mime = mime_str.parse().unwrap_or(mime::APPLICATION_OCTET_STREAM);
                match client.media().upload(&mime, bytes, None).await {
                    Ok(resp) => {
                        let file_content = FileMessageEventContent::plain(
                            filename.clone(),
                            resp.content_uri,
                        );
                        let msg_content = RoomMessageEventContent::new(
                            MessageType::File(file_content),
                        );
                        if let Err(e) = room.send(msg_content).await {
                            warn!(idx, "Matrix SDK: file send failed: {e}");
                        }
                    }
                    Err(e) => {
                        warn!(idx, "Matrix SDK: file upload failed: {e}");
                    }
                }
            }

            Ok(())
        })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let client = self.get_client().await?;
            info!("Matrix: SDK sync loop starting (E2EE enabled)");

            // Log joined rooms
            let rooms = client.joined_rooms();
            info!(count = rooms.len(), "Matrix: joined rooms");
            for room in &rooms {
                info!(room_id = %room.room_id(), "Matrix: joined room");
            }

            let sync_settings = SyncSettings::default().timeout(Duration::from_secs(30));

            // Register message handler
            let on_msg = Arc::clone(&self.on_message);
            let my_user_id = self.user_id.clone();
            let media_client = client.clone();

            client.add_event_handler(move |event: OriginalSyncRoomMessageEvent, room: Room| {
                let on_msg = Arc::clone(&on_msg);
                let my_user_id = my_user_id.clone();
                let media_client = media_client.clone();
                async move {
                    if event.sender.as_str() == my_user_id {
                        return;
                    }
                    if room.state() != RoomState::Joined {
                        return;
                    }

                    let room_id = room.room_id().to_string();
                    let sender = event.sender.to_string();
                    let is_group = !room.is_direct().await.unwrap_or(false);

                    match event.content.msgtype {
                        MessageType::Text(text) => {
                            info!(from = %sender, room = %room_id, is_group, len = text.body.len(), "Matrix: text message (SDK)");
                            on_msg(sender, text.body, room_id, is_group, vec![], vec![]);
                        }
                        MessageType::Image(image) => {
                            info!(from = %sender, room = %room_id, "Matrix: image message (SDK)");
                            // Download decrypted image via SDK
                            let source = image.source;
                            match media_client.media().get_media_content(
                                &matrix_sdk::media::MediaRequestParameters { source, format: matrix_sdk::media::MediaFormat::File },
                                true,
                            ).await {
                                Ok(bytes) => {
                                    use base64::Engine;
                                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                                    let mime = image.info.as_ref()
                                        .and_then(|i| i.mimetype.as_deref())
                                        .unwrap_or("image/png");
                                    let data_url = format!("data:{mime};base64,{b64}");
                                    info!(from = %sender, room = %room_id, size = bytes.len(), "Matrix: image downloaded (SDK)");
                                    on_msg(
                                        sender, crate::i18n::t("describe_image", crate::i18n::default_lang()), room_id, is_group,
                                        vec![crate::agent::registry::ImageAttachment {
                                            data: data_url,
                                            mime_type: mime.to_owned(),
                                        }],
                                        vec![],
                                    );
                                }
                                Err(e) => {
                                    warn!("Matrix: image download failed (SDK): {e}");
                                    on_msg(sender, crate::i18n::t("image_download_failed", crate::i18n::default_lang()), room_id, is_group, vec![], vec![]);
                                }
                            }
                        }
                        MessageType::Audio(audio) => {
                            info!(from = %sender, room = %room_id, "Matrix: audio message (SDK)");
                            let source = audio.source;
                            match media_client.media().get_media_content(
                                &matrix_sdk::media::MediaRequestParameters { source, format: matrix_sdk::media::MediaFormat::File },
                                true,
                            ).await {
                                Ok(bytes) => {
                                    let mime = audio.info.as_ref()
                                        .and_then(|i| i.mimetype.as_deref())
                                        .unwrap_or("audio/ogg");
                                    match crate::channel::transcription::transcribe_audio(
                                        &reqwest::Client::new(), &bytes, "voice.ogg", mime,
                                    ).await {
                                        Ok(text) => {
                                            info!(from = %sender, room = %room_id, chars = text.len(), "Matrix: voice transcribed (SDK)");
                                            on_msg(sender, text, room_id, is_group, vec![], vec![]);
                                        }
                                        Err(e) => {
                                            warn!("Matrix: voice transcription failed (SDK): {e}");
                                            on_msg(sender, "[voice message - transcription failed]".to_owned(), room_id, is_group, vec![], vec![]);
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("Matrix: audio download failed (SDK): {e}");
                                    on_msg(sender, "[voice message received]".to_owned(), room_id, is_group, vec![], vec![]);
                                }
                            }
                        }
                        MessageType::Video(video) => {
                            info!(from = %sender, room = %room_id, "Matrix: video message (SDK)");
                            // Download video and try to extract audio for transcription
                            let source = video.source;
                            match media_client.media().get_media_content(
                                &matrix_sdk::media::MediaRequestParameters { source, format: matrix_sdk::media::MediaFormat::File },
                                true,
                            ).await {
                                Ok(bytes) => {
                                    // Try transcribing the video's audio track
                                    match crate::channel::transcription::transcribe_audio(
                                        &reqwest::Client::new(), &bytes, "video.mp4", "video/mp4",
                                    ).await {
                                        Ok(text) => {
                                            info!(from = %sender, room = %room_id, chars = text.len(), "Matrix: video transcribed (SDK)");
                                            on_msg(sender, text, room_id, is_group, vec![], vec![]);
                                        }
                                        Err(_) => {
                                            on_msg(sender, crate::i18n::t("video_message_received", crate::i18n::default_lang()), room_id, is_group, vec![], vec![]);
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("Matrix: video download failed (SDK): {e}");
                                    on_msg(sender, crate::i18n::t("video_message_received", crate::i18n::default_lang()), room_id, is_group, vec![], vec![]);
                                }
                            }
                        }
                        MessageType::File(file) => {
                            info!(from = %sender, room = %room_id, name = ?file.body, "Matrix: file message (SDK)");
                            let source = file.source;
                            let filename = file.body.clone();
                            let mime = file.info.as_ref()
                                .and_then(|i| i.mimetype.as_deref())
                                .unwrap_or("application/octet-stream")
                                .to_owned();
                            match media_client.media().get_media_content(
                                &matrix_sdk::media::MediaRequestParameters { source, format: matrix_sdk::media::MediaFormat::File },
                                true,
                            ).await {
                                Ok(bytes) => {
                                    info!(from = %sender, room = %room_id, size = bytes.len(), fname = %filename, "Matrix: file downloaded (SDK)");
                                    let file_attachment = crate::agent::registry::FileAttachment {
                                        filename: filename.clone(),
                                        data: bytes,
                                        mime_type: mime.clone(),
                                    };
                                    on_msg(
                                        sender,
                                        String::new(),
                                        room_id,
                                        true,
                                        vec![],
                                        vec![file_attachment],
                                    );
                                }
                                Err(e) => {
                                    warn!("Matrix: file download failed (SDK): {e}");
                                    on_msg(sender, format!("[File received: {filename} but download failed]"), room_id, is_group, vec![], vec![]);
                                }
                            }
                        }
                        _ => {
                            debug!(msgtype = ?event.content.msgtype, "Matrix: unsupported message type (SDK)");
                        }
                    }
                }
            });

            // Auto-join invited rooms
            client.add_event_handler(
                |event: matrix_sdk::ruma::events::room::member::StrippedRoomMemberEvent,
                 room: Room,
                 client: MatrixSdkClient| async move {
                    if event.state_key != *client.user_id().expect("user_id") {
                        return;
                    }
                    if room.state() == RoomState::Invited {
                        if let Err(e) = client.join_room_by_id(room.room_id()).await {
                            warn!("Matrix: failed to auto-join room {}: {e}", room.room_id());
                        } else {
                            info!("Matrix: auto-joined room {}", room.room_id());
                        }
                    }
                },
            );

            // Also handle encrypted events that failed to decrypt
            client.add_event_handler(|event: matrix_sdk::ruma::events::room::encrypted::OriginalSyncRoomEncryptedEvent, room: Room| async move {
                warn!(
                    room = %room.room_id(),
                    sender = %event.sender,
                    "Matrix: received encrypted event (unable to decrypt - missing keys?)"
                );
            });

            // Run sync loop with callback for visibility
            client.sync_with_callback(sync_settings, |response| async move {
                let room_count = response.rooms.join.len();
                if room_count > 0 {
                    debug!(rooms = room_count, "Matrix: SDK sync response with room events");
                }
                matrix_sdk::LoopCtrl::Continue
            }).await
                .map_err(|e| anyhow::anyhow!("Matrix: sync failed: {e}"))?;

            Ok(())
        })
    }
}


// =========================================================================
// reqwest fallback implementation (no feature)
// =========================================================================

#[cfg(not(feature = "channel-matrix"))]
use std::{sync::Arc, time::Duration};

#[cfg(not(feature = "channel-matrix"))]
use anyhow::{Context, Result};
#[cfg(not(feature = "channel-matrix"))]
use futures::future::BoxFuture;
#[cfg(not(feature = "channel-matrix"))]
use reqwest::Client;
#[cfg(not(feature = "channel-matrix"))]
use serde_json::json;
#[cfg(not(feature = "channel-matrix"))]
use tracing::{debug, info, warn};

#[cfg(not(feature = "channel-matrix"))]
use super::{Channel, OutboundMessage};
#[cfg(not(feature = "channel-matrix"))]
use crate::channel::chunker::{ChunkConfig, chunk_text, platform_chunk_limit};

#[cfg(not(feature = "channel-matrix"))]
pub struct MatrixChannel {
    homeserver: String,
    access_token: String,
    user_id: String, // bot's own user ID, to skip own messages
    client: Client,
    on_message: Arc<dyn Fn(String, String, String, bool, Vec<crate::agent::registry::ImageAttachment>, Vec<crate::agent::registry::FileAttachment>) + Send + Sync>,
    // (sender_id, text, room_id, is_room, images, files)
}

#[cfg(not(feature = "channel-matrix"))]
impl MatrixChannel {
    pub fn new(
        homeserver: impl Into<String>,
        access_token: impl Into<String>,
        user_id: impl Into<String>,
        on_message: Arc<dyn Fn(String, String, String, bool, Vec<crate::agent::registry::ImageAttachment>, Vec<crate::agent::registry::FileAttachment>) + Send + Sync>,
    ) -> Self {
        Self {
            homeserver: homeserver.into().trim_end_matches('/').to_owned(),
            access_token: access_token.into(),
            user_id: user_id.into(),
            client: crate::config::build_proxy_client()
                .timeout(Duration::from_secs(60))
                .build()
                .expect("reqwest client"),
            on_message,
        }
    }

    async fn send_text(&self, room_id: &str, text: &str) -> Result<()> {
        let txn_id = uuid::Uuid::new_v4().to_string();
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            self.homeserver, room_id, txn_id
        );
        let body = json!({
            "msgtype": "m.text",
            "body": text,
        });
        let resp = self
            .client
            .put(&url)
            .bearer_auth(&self.access_token)
            .json(&body)
            .send()
            .await
            .context("Matrix: send message failed")?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Matrix: send failed: {body}");
        }
        Ok(())
    }

    async fn send_image(&self, room_id: &str, image_data: &str) -> Result<()> {
        // Upload image to Matrix content repo first
        let b64 = image_data
            .strip_prefix("data:image/png;base64,")
            .or_else(|| image_data.strip_prefix("data:image/jpeg;base64,"))
            .unwrap_or(image_data);
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;

        let upload_url = format!("{}/_matrix/client/v1/media/upload", self.homeserver);
        let resp = self
            .client
            .post(&upload_url)
            .bearer_auth(&self.access_token)
            .header("content-type", "image/png")
            .body(bytes)
            .send()
            .await
            .context("Matrix: upload image failed")?;

        if let Ok(body) = resp.json::<serde_json::Value>().await {
            if let Some(content_uri) = body.get("content_uri").and_then(|v| v.as_str()) {
                let txn_id = uuid::Uuid::new_v4().to_string();
                let url = format!(
                    "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
                    self.homeserver, room_id, txn_id
                );
                let msg = json!({
                    "msgtype": "m.image",
                    "body": "image.png",
                    "url": content_uri,
                    "info": { "mimetype": "image/png" }
                });
                let _ = self
                    .client
                    .put(&url)
                    .bearer_auth(&self.access_token)
                    .json(&msg)
                    .send()
                    .await;
            }
        }
        Ok(())
    }
}

#[cfg(not(feature = "channel-matrix"))]
impl Channel for MatrixChannel {
    fn name(&self) -> &str {
        "matrix"
    }

    fn send(&self, msg: OutboundMessage) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let chunk_cfg = ChunkConfig {
                max_chars: platform_chunk_limit("matrix"),
                min_chars: 1,
                break_preference: super::chunker::BreakPreference::Paragraph,
            };
            for chunk in &chunk_text(&msg.text, &chunk_cfg) {
                self.send_text(&msg.target_id, chunk).await?;
            }
            if !msg.images.is_empty() {
                info!(count = msg.images.len(), "matrix: sending images");
            }
            for (idx, image_data) in msg.images.iter().enumerate() {
                if let Err(e) = self.send_image(&msg.target_id, image_data).await {
                    warn!(idx, "matrix: send_image failed: {e}");
                }
            }

            // Send file attachments via Matrix upload + m.file message
            for (idx, (filename, mime_str, path_or_url)) in msg.files.iter().enumerate() {
                let bytes = if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
                    match self.client.get(path_or_url.as_str()).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            match resp.bytes().await {
                                Ok(b) if !b.is_empty() => b.to_vec(),
                                _ => { warn!(idx, "matrix: empty file download"); continue; }
                            }
                        }
                        _ => { warn!(idx, "matrix: file download failed"); continue; }
                    }
                } else {
                    match std::fs::read(path_or_url) {
                        Ok(b) => b,
                        Err(e) => { warn!(idx, "matrix: read file failed: {e}"); continue; }
                    }
                };

                // Upload via /_matrix/media/v3/upload
                let upload_url = format!(
                    "{}/_matrix/media/v3/upload?filename={}",
                    self.homeserver,
                    urlencoding::encode(filename),
                );
                match self.client
                    .post(&upload_url)
                    .bearer_auth(&self.access_token)
                    .header("content-type", mime_str.as_str())
                    .body(bytes)
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        if let Ok(body) = resp.json::<serde_json::Value>().await {
                            if let Some(content_uri) = body.get("content_uri").and_then(|v| v.as_str()) {
                                // Send m.file message
                                let event = serde_json::json!({
                                    "msgtype": "m.file",
                                    "body": filename,
                                    "url": content_uri,
                                    "info": { "mimetype": mime_str },
                                });
                                let send_url = format!(
                                    "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
                                    self.homeserver,
                                    urlencoding::encode(&msg.target_id),
                                    uuid::Uuid::new_v4(),
                                );
                                if let Err(e) = self.client
                                    .put(&send_url)
                                    .bearer_auth(&self.access_token)
                                    .json(&event)
                                    .send()
                                    .await
                                {
                                    warn!(idx, "matrix: file send failed: {e}");
                                }
                            }
                        }
                    }
                    Ok(r) => {
                        let err = r.text().await.unwrap_or_default();
                        warn!(idx, "matrix: file upload failed: {err}");
                    }
                    Err(e) => { warn!(idx, "matrix: file upload error: {e}"); }
                }
            }

            Ok(())
        })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            info!("Matrix long-poll sync loop started");
            let mut since: Option<String> = None;

            loop {
                let mut url = format!(
                    "{}/_matrix/client/v3/sync?timeout=30000",
                    self.homeserver
                );
                if let Some(ref s) = since {
                    url.push_str(&format!("&since={s}"));
                }
                // First sync: filter to only get recent messages, not full history
                if since.is_none() {
                    url.push_str("&filter=%7B%22room%22%3A%7B%22timeline%22%3A%7B%22limit%22%3A0%7D%7D%7D");
                }

                debug!(url = &url[..url.len().min(120)], "Matrix: sync request");

                match self
                    .client
                    .get(&url)
                    .bearer_auth(&self.access_token)
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        match resp.json::<serde_json::Value>().await {
                            Ok(body) => {
                                // Update since token
                                if let Some(nb) =
                                    body.get("next_batch").and_then(|v| v.as_str())
                                {
                                    debug!(next_batch = nb, "Matrix: sync ok");
                                    since = Some(nb.to_owned());
                                }

                                // Process room events
                                let room_count = body.pointer("/rooms/join")
                                    .and_then(|v| v.as_object())
                                    .map(|o| o.len())
                                    .unwrap_or(0);
                                if room_count > 0 {
                                    debug!(rooms = room_count, "Matrix: rooms with events");
                                }

                                if let Some(rooms) =
                                    body.pointer("/rooms/join").and_then(|v| v.as_object())
                                {
                                    for (room_id, room_data) in rooms {
                                        let events = room_data
                                            .pointer("/timeline/events")
                                            .and_then(|v| v.as_array());
                                        if let Some(events) = events {
                                            for event in events {
                                                let event_type = event
                                                    .get("type")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("");
                                                let sender = event
                                                    .get("sender")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("");

                                                debug!(
                                                    event_type,
                                                    sender,
                                                    self_id = %self.user_id,
                                                    "Matrix: event in room {}", room_id
                                                );

                                                if event_type != "m.room.message" {
                                                    continue;
                                                }

                                                // Skip own messages
                                                if sender == self.user_id {
                                                    debug!("Matrix: skipping own message");
                                                    continue;
                                                }

                                                let content = event
                                                    .get("content")
                                                    .unwrap_or(&serde_json::Value::Null);
                                                let msgtype = content
                                                    .get("msgtype")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("");

                                                match msgtype {
                                                    "m.text" => {
                                                        let text = content
                                                            .get("body")
                                                            .and_then(|v| v.as_str())
                                                            .unwrap_or("");
                                                        if text.is_empty() { continue; }

                                                        info!(from = %sender, room = %room_id, text_len = text.len(), "Matrix: text message");
                                                        (self.on_message)(
                                                            sender.to_owned(),
                                                            text.to_owned(),
                                                            room_id.clone(),
                                                            true,
                                                            vec![],
                                                            vec![],
                                                        );
                                                    }
                                                    "m.image" => {
                                                        // Download image from Matrix media repo
                                                        let mxc_url = content.get("url").and_then(|v| v.as_str()).unwrap_or("");
                                                        if mxc_url.is_empty() { continue; }

                                                        // Convert mxc://server/media_id to https://server/_matrix/media/v3/download/server/media_id
                                                        let download_url = if let Some(rest) = mxc_url.strip_prefix("mxc://") {
                                                            format!("{}/_matrix/client/v1/media/download/{}", self.homeserver, rest)
                                                        } else {
                                                            continue;
                                                        };

                                                        debug!(url = %download_url, "Matrix: downloading image");
                                                        // Try authenticated download (spec v1.11+)
                                                        let resp_result = self.client.get(&download_url)
                                                            .bearer_auth(&self.access_token)
                                                            .send().await;
                                                        // If auth fails, try unauthenticated (older servers)
                                                        let resp_result = match &resp_result {
                                                            Ok(r) if !r.status().is_success() => {
                                                                let unauth_url = download_url.replace("/_matrix/client/v1/media/", "/_matrix/media/v3/");
                                                                self.client.get(&unauth_url).send().await
                                                            }
                                                            _ => resp_result,
                                                        };
                                                        match resp_result {
                                                            Ok(resp) if resp.status().is_success() => {
                                                                if let Ok(bytes) = resp.bytes().await {
                                                                    use base64::Engine;
                                                                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                                                                    let data_url = format!("data:image/png;base64,{b64}");
                                                                    info!(from = %sender, room = %room_id, size = bytes.len(), "Matrix: image received");
                                                                    (self.on_message)(
                                                                        sender.to_owned(),
                                                                        crate::i18n::t("describe_image", crate::i18n::default_lang()),
                                                                        room_id.clone(),
                                                                        true,
                                                                        vec![crate::agent::registry::ImageAttachment {
                                                                            data: data_url,
                                                                            mime_type: "image/png".to_owned(),
                                                                        }],
                                                                        vec![],
                                                                    );
                                                                }
                                                            }
                                                            Ok(resp) => {
                                                                warn!("Matrix: image download failed: {}", resp.status());
                                                            }
                                                            Err(e) => {
                                                                warn!("Matrix: image download error: {e}");
                                                            }
                                                        }
                                                    }
                                                    "m.audio" | "m.video" => {
                                                        // Download audio and transcribe
                                                        let mxc_url = content.get("url").and_then(|v| v.as_str()).unwrap_or("");
                                                        if mxc_url.is_empty() { continue; }

                                                        let download_url = if let Some(rest) = mxc_url.strip_prefix("mxc://") {
                                                            format!("{}/_matrix/client/v1/media/download/{}", self.homeserver, rest)
                                                        } else {
                                                            continue;
                                                        };

                                                        match self.client.get(&download_url).bearer_auth(&self.access_token).send().await {
                                                            Ok(resp) if resp.status().is_success() => {
                                                                if let Ok(bytes) = resp.bytes().await {
                                                                    let mime = content.get("info")
                                                                        .and_then(|i| i.get("mimetype"))
                                                                        .and_then(|v| v.as_str())
                                                                        .unwrap_or("audio/ogg");
                                                                    match crate::channel::transcription::transcribe_audio(
                                                                        &self.client, &bytes, "voice.ogg", mime,
                                                                    ).await {
                                                                        Ok(text) => {
                                                                            info!(from = %sender, room = %room_id, chars = text.len(), "Matrix: voice transcribed");
                                                                            (self.on_message)(
                                                                                sender.to_owned(),
                                                                                text,
                                                                                room_id.clone(),
                                                                                true,
                                                                                vec![],
                                                                                vec![],
                                                                            );
                                                                        }
                                                                        Err(e) => { warn!("Matrix: voice transcription failed: {e:#}"); }
                                                                    }
                                                                }
                                                            }
                                                            _ => { warn!("Matrix: audio download failed"); }
                                                        }
                                                    }
                                                    _ => {
                                                        debug!(msgtype, "Matrix: unsupported message type");
                                                        continue;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Err(e) => warn!("Matrix: sync parse error: {e}"),
                        }
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp.text().await.unwrap_or_default();
                        warn!("Matrix: sync error {} -- {}", status, &body[..body.len().min(200)]);
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                    Err(e) => {
                        warn!("Matrix: sync request failed: {e}");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(feature = "channel-matrix"))]
    use std::sync::Arc;

    #[cfg(not(feature = "channel-matrix"))]
    use super::super::Channel;

    fn init_crypto() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }

    #[test]
    fn channel_name() {
        init_crypto();
        let ch = MatrixChannel::new(
            "https://matrix.org",
            "token",
            "@bot:matrix.org",
            Arc::new(|_, _, _, _, _, _| {}),
        );
        assert_eq!(ch.name(), "matrix");
    }

    // -----------------------------------------------------------------------
    // Mock server tests (require mock_matrix.py running on port 19986)
    // -----------------------------------------------------------------------

    /// Build a MatrixChannel pointing at the mock server.
    #[cfg(not(feature = "channel-matrix"))]
    fn mock_channel(received: std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> MatrixChannel {
        let rx = std::sync::Arc::clone(&received);
        MatrixChannel::new(
            "http://127.0.0.1:19986",
            "mock-access-token",
            "@bot:mock.local",
            Arc::new(move |_sender, text, _room, _is_group, _images, _files| {
                rx.lock().unwrap().push(text);
            }),
        )
    }

    /// Call sync once against mock_matrix.py and expect room events with text messages.
    ///
    /// Run: python3 /tmp/mock_matrix.py  (in a separate terminal)
    /// Then: cargo test -p rsclaw matrix::tests::mock_sync_returns_events -- --ignored
    #[cfg(not(feature = "channel-matrix"))]
    #[tokio::test]
    #[ignore]
    async fn mock_sync_returns_events() {
        let ch = mock_channel(std::sync::Arc::new(std::sync::Mutex::new(vec![])));

        let url = format!(
            "{}/_matrix/client/v3/sync?timeout=30000",
            ch.homeserver
        );
        let resp = ch
            .client
            .get(&url)
            .bearer_auth(&ch.access_token)
            .send()
            .await
            .expect("sync request failed");

        assert!(resp.status().is_success(), "sync should return 200");

        let body: serde_json::Value = resp.json().await.expect("parse sync response");
        let next_batch = body.get("next_batch").and_then(|v| v.as_str());
        assert!(next_batch.is_some(), "sync response should have next_batch");

        let rooms = body
            .pointer("/rooms/join")
            .and_then(|v| v.as_object())
            .expect("sync should have rooms.join");
        assert!(!rooms.is_empty(), "should have at least one room");

        // Extract first event text
        let events = rooms
            .values()
            .next()
            .and_then(|r| r.pointer("/timeline/events"))
            .and_then(|v| v.as_array())
            .expect("room should have timeline events");
        assert!(!events.is_empty(), "room should have events");

        let text = events[0]
            .pointer("/content/body")
            .and_then(|v| v.as_str())
            .expect("first event should have body");
        assert_eq!(text, "Hello from mock Matrix");
    }

    /// Call sync twice; second call (with since token) should return empty events.
    ///
    /// Run: python3 /tmp/mock_matrix.py  (restart before test)
    /// Then: cargo test -p rsclaw matrix::tests::mock_sync_empty_second -- --ignored
    #[cfg(not(feature = "channel-matrix"))]
    #[tokio::test]
    #[ignore]
    async fn mock_sync_empty_second_call() {
        let ch = mock_channel(std::sync::Arc::new(std::sync::Mutex::new(vec![])));

        // First sync
        let url = format!(
            "{}/_matrix/client/v3/sync?timeout=30000",
            ch.homeserver
        );
        let first: serde_json::Value = ch
            .client
            .get(&url)
            .bearer_auth(&ch.access_token)
            .send()
            .await
            .expect("first sync failed")
            .json()
            .await
            .expect("parse first sync");

        let since = first
            .get("next_batch")
            .and_then(|v| v.as_str())
            .expect("next_batch missing");

        // Second sync with since token
        let url2 = format!(
            "{}/_matrix/client/v3/sync?timeout=30000&since={}",
            ch.homeserver, since
        );
        let second: serde_json::Value = ch
            .client
            .get(&url2)
            .bearer_auth(&ch.access_token)
            .send()
            .await
            .expect("second sync failed")
            .json()
            .await
            .expect("parse second sync");

        let rooms = second
            .pointer("/rooms/join")
            .and_then(|v| v.as_object());
        let event_count = rooms
            .map(|r| {
                r.values()
                    .filter_map(|rd| rd.pointer("/timeline/events").and_then(|v| v.as_array()))
                    .map(|evs| evs.len())
                    .sum::<usize>()
            })
            .unwrap_or(0);
        assert_eq!(event_count, 0, "second sync should return no new events");
    }

    /// Send a message to the mock Matrix server.
    ///
    /// Run: python3 /tmp/mock_matrix.py
    /// Then: cargo test -p rsclaw matrix::tests::mock_send_message -- --ignored
    #[cfg(not(feature = "channel-matrix"))]
    #[tokio::test]
    #[ignore]
    async fn mock_send_message() {
        let ch = mock_channel(std::sync::Arc::new(std::sync::Mutex::new(vec![])));
        ch.send_text("!mockroom:mock.local", "Hello from rsclaw test")
            .await
            .expect("send_text failed");
    }
}
