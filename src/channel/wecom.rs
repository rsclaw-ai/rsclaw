//! WeCom (企业微信) AI Bot channel driver.
//!
//! Connects to the WeCom AI Bot WebSocket API at
//! `wss://openws.work.weixin.qq.com`.  Receives inbound messages and
//! sends replies via the same WebSocket connection using JSON frames.
//!
//! Reconnects automatically with exponential back-off on disconnect.

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use futures::{SinkExt as _, StreamExt as _, future::BoxFuture};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tracing::{debug, error, info, warn};

use super::{Channel, OutboundMessage};
use crate::channel::chunker::{BreakPreference, ChunkConfig, chunk_text, platform_chunk_limit};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_WS_URL: &str = "wss://openws.work.weixin.qq.com";

/// Heartbeat interval (30 seconds as per protocol).
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum back-off delay on reconnect.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Maximum chunk size for media upload (512 KB before base64 encoding).
const UPLOAD_CHUNK_SIZE: usize = 524_288;

/// Timeout for each upload RPC step.
const UPLOAD_STEP_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// WeComChannel
// ---------------------------------------------------------------------------

pub struct WeComChannel {
    /// Bot ID (from agent_id or app_id config field).
    bot_id: String,
    /// Bot secret.
    secret: String,
    /// WebSocket endpoint URL.
    ws_url: String,
    /// HTTP client for media downloads.
    client: Client,
    /// Sender half for writing frames to the WebSocket.
    /// Populated once `run()` is called.
    ws_tx: mpsc::UnboundedSender<String>,
    /// Receiver half -- moved into the run loop.
    ws_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<String>>>,
    /// Pending upload RPC responses keyed by req_id.
    /// The read loop delivers matching frames here so upload steps can await.
    pending_responses: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    /// Callback: (from_user_id, text, chat_id, is_group, images, files).
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
}

impl WeComChannel {
    /// Create a new WeCom AI Bot channel.
    ///
    /// * `bot_id`  -- WeCom bot / agent ID.
    /// * `secret`  -- Bot secret for authentication.
    /// * `ws_url`  -- WebSocket endpoint (use `None` for the default).
    /// * `on_message` -- Callback invoked for each inbound message.
    #[allow(clippy::type_complexity)]
    pub fn new(
        bot_id: impl Into<String>,
        secret: impl Into<String>,
        ws_url: Option<String>,
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
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            bot_id: bot_id.into(),
            secret: secret.into(),
            ws_url: ws_url.unwrap_or_else(|| DEFAULT_WS_URL.to_owned()),
            client: Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .expect("reqwest client"),
            ws_tx: tx,
            ws_rx: std::sync::Mutex::new(Some(rx)),
            pending_responses: Arc::new(Mutex::new(HashMap::new())),
            on_message,
        }
    }

    // -----------------------------------------------------------------------
    // WebSocket reconnect loop
    // -----------------------------------------------------------------------

    async fn ws_loop(self: &Arc<Self>, mut outbound_rx: mpsc::UnboundedReceiver<String>) -> ! {
        let mut backoff_secs: u64 = 1;

        loop {
            info!(url = %self.ws_url, "WeCom WS: connecting...");

            match self.run_single_connection(&mut outbound_rx).await {
                Ok(()) => {
                    info!("WeCom WS: connection closed normally, reconnecting");
                    backoff_secs = 1; // reset on clean close
                }
                Err(e) => {
                    error!("WeCom WS: connection error: {e:#}");
                }
            }

            let delay = Duration::from_secs(backoff_secs);
            warn!(
                delay_secs = backoff_secs,
                "WeCom WS: reconnecting after delay"
            );
            tokio::time::sleep(delay).await;
            backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF.as_secs());
        }
    }

    /// Run a single WebSocket session: connect, authenticate, heartbeat,
    /// receive.
    async fn run_single_connection(
        self: &Arc<Self>,
        outbound_rx: &mut mpsc::UnboundedReceiver<String>,
    ) -> Result<()> {
        let (ws_stream, _resp) = connect_async(&self.ws_url)
            .await
            .context("WeCom WS: connect failed")?;

        let (mut ws_sink, mut ws_source) = ws_stream.split();

        // --- Authenticate ---
        let auth_req_id = uuid::Uuid::new_v4().to_string();
        let auth_frame = json!({
            "cmd": "aibot_subscribe",
            "headers": { "req_id": &auth_req_id },
            "body": {
                "bot_id": &self.bot_id,
                "secret": &self.secret,
            }
        });
        ws_sink
            .send(WsMessage::Text(auth_frame.to_string().into()))
            .await
            .context("WeCom WS: send auth")?;

        debug!(frame = %auth_frame, "WeCom WS: auth frame sent");
        info!("WeCom WS: auth frame sent, waiting for response...");

        // Wait for auth response (first frame should be the reply).
        let auth_resp = tokio::time::timeout(Duration::from_secs(15), ws_source.next())
            .await
            .context("WeCom WS: auth response timeout")?
            .ok_or_else(|| anyhow::anyhow!("WeCom WS: connection closed before auth response"))?
            .context("WeCom WS: read auth response")?;

        if let WsMessage::Text(ref txt) = auth_resp {
            debug!(raw = %txt, "WeCom WS: auth response raw");
            let v: Value = serde_json::from_str(txt).unwrap_or_default();
            // errcode can be at top level or inside body
            let errcode = v
                .get("errcode")
                .or_else(|| v.get("body").and_then(|b| b.get("errcode")))
                .and_then(|c| c.as_i64())
                .unwrap_or(-1);
            if errcode != 0 {
                let errmsg = v
                    .get("errmsg")
                    .or_else(|| v.get("body").and_then(|b| b.get("errmsg")))
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown");
                bail!("WeCom WS: auth failed ({errcode}): {errmsg}");
            }
            info!("WeCom WS: authenticated successfully");
        } else {
            bail!("WeCom WS: unexpected auth response frame type");
        }

        // --- Heartbeat task ---
        let heartbeat_tx = self.ws_tx.clone();
        let heartbeat_handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(HEARTBEAT_INTERVAL).await;
                let ping = json!({
                    "cmd": "ping",
                    "headers": { "req_id": uuid::Uuid::new_v4().to_string() }
                });
                if heartbeat_tx.send(ping.to_string()).is_err() {
                    break;
                }
            }
        });

        // --- Main loop: multiplex inbound WS frames and outbound sends ---
        let this = Arc::clone(self);
        loop {
            tokio::select! {
                frame = ws_source.next() => {
                    match frame {
                        Some(Ok(WsMessage::Text(txt))) => {
                            let txt_str: &str = &txt;
                            this.handle_frame(txt_str).await;
                        }
                        Some(Ok(WsMessage::Ping(data))) => {
                            ws_sink.send(WsMessage::Pong(data)).await.ok();
                        }
                        Some(Ok(WsMessage::Close(_))) | None => {
                            info!("WeCom WS: connection closed by server");
                            break;
                        }
                        Some(Ok(_)) => {
                            // Binary or other frame types -- ignore.
                        }
                        Some(Err(e)) => {
                            error!("WeCom WS: read error: {e:#}");
                            break;
                        }
                    }
                }
                outbound = outbound_rx.recv() => {
                    match outbound {
                        Some(payload) => {
                            if let Err(e) = ws_sink.send(WsMessage::Text(payload.into())).await {
                                error!("WeCom WS: send error: {e:#}");
                                break;
                            }
                        }
                        None => {
                            // Channel closed -- should not happen while Arc is alive.
                            break;
                        }
                    }
                }
            }
        }

        heartbeat_handle.abort();
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Inbound frame handling
    // -----------------------------------------------------------------------

    async fn handle_frame(self: &Arc<Self>, raw: &str) {
        let frame: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => {
                warn!("WeCom WS: failed to parse frame: {e}");
                return;
            }
        };

        let cmd = frame.get("cmd").and_then(|c| c.as_str()).unwrap_or("");
        let raw_preview = frame.to_string();
        debug!(
            cmd,
            raw = &raw_preview[..raw_preview.len().min(300)],
            "WeCom WS: frame received"
        );

        // Route upload RPC responses back to pending waiters via req_id.
        // These frames are responses to our own requests (errcode present,
        // matching req_id in headers), not server-initiated commands.
        if let Some(req_id) = frame
            .get("headers")
            .and_then(|h| h.get("req_id"))
            .and_then(|r| r.as_str())
        {
            let mut pending = self.pending_responses.lock().await;
            if let Some(tx) = pending.remove(req_id) {
                // Deliver response; ignore error if waiter already dropped.
                let _ = tx.send(frame.clone());
                return;
            }
        }

        match cmd {
            "aibot_msg_callback" => {
                self.handle_message(&frame).await;
            }
            "aibot_event_callback" => {
                debug!("WeCom WS: event callback (ignored)");
            }
            "pong" => {
                debug!("WeCom WS: pong received");
            }
            other => {
                debug!(cmd = other, "WeCom WS: unknown cmd");
            }
        }
    }

    /// Process an inbound `aibot_msg_callback` frame.
    async fn handle_message(self: &Arc<Self>, frame: &Value) {
        let frame_type = frame
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("unknown");
        debug!(frame_type = %frame_type, "WeCom: WS frame received");

        let body = match frame.get("body") {
            Some(b) => b,
            None => return,
        };

        let from_userid = body
            .get("from")
            .and_then(|f| f.get("userid"))
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_owned();

        let chatid = body
            .get("chatid")
            .and_then(|c| c.as_str())
            .unwrap_or(&from_userid)
            .to_owned();

        let chattype = body
            .get("chattype")
            .and_then(|c| c.as_str())
            .unwrap_or("single");
        let is_group = chattype == "group";

        let msgtype = body
            .get("msgtype")
            .and_then(|m| m.as_str())
            .unwrap_or("text");
        // Log body keys for debugging unrecognized message types.
        let body_keys: Vec<&str> = body
            .as_object()
            .map(|m| m.keys().map(|k| k.as_str()).collect())
            .unwrap_or_default();
        info!(msgtype = %msgtype, keys = ?body_keys, "WeCom: message received");

        let mut text = String::new();
        let mut images: Vec<crate::agent::registry::ImageAttachment> = Vec::new();
        let mut files: Vec<crate::agent::registry::FileAttachment> = Vec::new();

        match msgtype {
            "text" => {
                text = body
                    .get("text")
                    .and_then(|t| t.get("content"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_owned();
            }
            "voice" => {
                // Try platform transcription first.
                text = body
                    .get("voice")
                    .and_then(|v| v.get("content"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_owned();
                // If platform transcription is empty, download and transcribe ourselves.
                if text.trim().is_empty() {
                    let url = body
                        .get("voice")
                        .and_then(|v| v.get("url"))
                        .and_then(|u| u.as_str())
                        .unwrap_or("");
                    let aeskey = body
                        .get("voice")
                        .and_then(|v| v.get("aeskey"))
                        .and_then(|k| k.as_str())
                        .unwrap_or("");
                    if !url.is_empty() {
                        match self.download_media(url, aeskey).await {
                            Ok(bytes) => {
                                files.push(crate::agent::registry::FileAttachment {
                                    filename: "voice.amr".to_owned(),
                                    data: bytes,
                                    mime_type: "audio/amr".to_owned(),
                                });
                                text = "[Voice]".to_owned();
                            }
                            Err(e) => {
                                error!("WeCom: voice download failed: {e:#}");
                            }
                        }
                    }
                }
            }
            "image" => {
                let url = body
                    .get("image")
                    .and_then(|i| i.get("url"))
                    .and_then(|u| u.as_str())
                    .unwrap_or("");
                let aeskey = body
                    .get("image")
                    .and_then(|i| i.get("aeskey"))
                    .and_then(|k| k.as_str())
                    .unwrap_or("");

                if !url.is_empty() {
                    match self.download_media(url, aeskey).await {
                        Ok(bytes) => {
                            use base64::Engine;
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                            let data_url = format!("data:image/jpeg;base64,{b64}");
                            images.push(crate::agent::registry::ImageAttachment {
                                data: data_url,
                                mime_type: "image/jpeg".to_string(),
                            });
                            if text.is_empty() {
                                text =
                                    crate::i18n::t("describe_image", crate::i18n::default_lang());
                            }
                        }
                        Err(e) => {
                            error!("WeCom: image download failed: {e:#}");
                        }
                    }
                }
            }
            "file" => {
                let url = body
                    .get("file")
                    .and_then(|f| f.get("url"))
                    .and_then(|u| u.as_str())
                    .unwrap_or("");
                let aeskey = body
                    .get("file")
                    .and_then(|f| f.get("aeskey"))
                    .and_then(|k| k.as_str())
                    .unwrap_or("");
                let filename = body
                    .get("file")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("file.bin");

                if !url.is_empty() {
                    match self.download_media(url, aeskey).await {
                        Ok(bytes) => {
                            let mime = guess_mime(filename);
                            // Detect images sent as file attachments.
                            if crate::channel::is_image_attachment(&mime, filename) {
                                use base64::Engine;
                                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                                let data_url = format!("data:{mime};base64,{b64}");
                                images.push(crate::agent::registry::ImageAttachment {
                                    data: data_url,
                                    mime_type: mime.clone(),
                                });
                                if text.is_empty() {
                                    text = crate::i18n::t(
                                        "describe_image",
                                        crate::i18n::default_lang(),
                                    );
                                }
                            } else {
                                files.push(crate::agent::registry::FileAttachment {
                                    filename: filename.to_owned(),
                                    data: bytes,
                                    mime_type: mime,
                                });
                                if text.is_empty() {
                                    text = format!("[File: {filename}]");
                                }
                            }
                        }
                        Err(e) => {
                            error!("WeCom: file download failed: {e:#}");
                        }
                    }
                }
            }
            "video" => {
                let url = body
                    .get("video")
                    .and_then(|v| v.get("url"))
                    .and_then(|u| u.as_str())
                    .unwrap_or("");
                let aeskey = body
                    .get("video")
                    .and_then(|v| v.get("aeskey"))
                    .and_then(|k| k.as_str())
                    .unwrap_or("");

                if !url.is_empty() {
                    match self.download_media(url, aeskey).await {
                        Ok(bytes) => {
                            files.push(crate::agent::registry::FileAttachment {
                                filename: "video.mp4".to_owned(),
                                data: bytes,
                                mime_type: "video/mp4".to_owned(),
                            });
                            if text.is_empty() {
                                text = "[Video]".to_owned();
                            }
                        }
                        Err(e) => {
                            error!("WeCom: video download failed: {e:#}");
                        }
                    }
                }
            }
            "mixed" => {
                // Mixed messages contain multiple items.
                if let Some(items) = body
                    .get("mixed")
                    .and_then(|m| m.get("msg_item"))
                    .and_then(|a| a.as_array())
                {
                    for item in items {
                        let item_type = item.get("msgtype").and_then(|t| t.as_str()).unwrap_or("");
                        match item_type {
                            "text" => {
                                let t = item
                                    .get("text")
                                    .and_then(|t| t.get("content"))
                                    .and_then(|c| c.as_str())
                                    .unwrap_or("");
                                if !t.is_empty() {
                                    if !text.is_empty() {
                                        text.push('\n');
                                    }
                                    text.push_str(t);
                                }
                            }
                            "image" => {
                                let url = item
                                    .get("image")
                                    .and_then(|i| i.get("url"))
                                    .and_then(|u| u.as_str())
                                    .unwrap_or("");
                                let aeskey = item
                                    .get("image")
                                    .and_then(|i| i.get("aeskey"))
                                    .and_then(|k| k.as_str())
                                    .unwrap_or("");
                                if !url.is_empty() {
                                    match self.download_media(url, aeskey).await {
                                        Ok(bytes) => {
                                            use base64::Engine;
                                            let b64 = base64::engine::general_purpose::STANDARD
                                                .encode(&bytes);
                                            let data_url = format!("data:image/jpeg;base64,{b64}");
                                            images.push(crate::agent::registry::ImageAttachment {
                                                data: data_url,
                                                mime_type: "image/jpeg".to_string(),
                                            });
                                        }
                                        Err(e) => {
                                            error!("WeCom: mixed image download failed: {e:#}");
                                        }
                                    }
                                }
                            }
                            _ => {
                                debug!(item_type, "WeCom: ignoring mixed item type");
                            }
                        }
                    }
                    if text.is_empty() && !images.is_empty() {
                        text = crate::i18n::t("describe_image", crate::i18n::default_lang());
                    }
                }
            }
            "event" => {
                debug!("WeCom: event message type (ignored)");
                return;
            }
            other => {
                debug!(msgtype = other, "WeCom: unsupported message type");
                return;
            }
        }

        if text.is_empty() && images.is_empty() && files.is_empty() {
            return;
        }

        debug!(
            from = %from_userid,
            chat = %chatid,
            is_group,
            text_len = text.len(),
            n_images = images.len(),
            n_files = files.len(),
            "WeCom: dispatching message"
        );

        (self.on_message)(from_userid, text, chatid, is_group, images, files);
    }

    // -----------------------------------------------------------------------
    // Media download
    // -----------------------------------------------------------------------

    /// Download media from a URL.  If `aeskey` is non-empty, AES-256-CBC
    /// decrypt the payload (key=base64(32 bytes), IV=key[0..16]).
    async fn download_media(&self, url: &str, aeskey: &str) -> Result<Vec<u8>> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .context("WeCom: media download request")?;

        if !resp.status().is_success() {
            bail!("WeCom: media download failed: {}", resp.status());
        }

        let bytes = resp.bytes().await.context("WeCom: media download body")?;
        debug!(size = bytes.len(), "WeCom: media downloaded");

        if aeskey.is_empty() {
            return Ok(bytes.to_vec());
        }

        // If data starts with known magic bytes, it's already decrypted.
        // JPEG: FF D8 FF, PNG: 89 50 4E 47, MP4 ftyp: xx xx xx xx 66 74 79 70
        let dominated = bytes.len() >= 4
            && (
                (bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF) // JPEG
            || (bytes[0] == 0x89 && &bytes[1..4] == b"PNG")            // PNG
            || (bytes.len() >= 8 && &bytes[4..8] == b"ftyp")
                // MP4/MOV
            );
        if dominated {
            debug!("WeCom: media already decrypted (magic bytes detected), skipping AES");
            return Ok(bytes.to_vec());
        }

        // AES-256-CBC decrypt: key = base64(32 bytes), IV = key[0..16].
        use aes::cipher::{BlockDecrypt, KeyInit};
        use base64::Engine;

        let key_bytes = base64::engine::general_purpose::STANDARD
            .decode(aeskey)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(aeskey))
            .context("WeCom: invalid aeskey base64")?;
        if key_bytes.len() != 32 {
            warn!(
                len = key_bytes.len(),
                "WeCom: aeskey not 32 bytes, returning raw"
            );
            return Ok(bytes.to_vec());
        }

        // IV = first 16 bytes of key.
        let iv: [u8; 16] = key_bytes[..16].try_into().unwrap();
        let mut data = bytes.to_vec();

        // CBC decrypt: XOR with previous ciphertext block (or IV for first block).
        let cipher =
            aes::Aes256::new_from_slice(&key_bytes).context("WeCom: AES-256 key init failed")?;

        let mut prev_block = iv;
        for chunk in data.chunks_mut(16) {
            if chunk.len() < 16 {
                break;
            }
            let cipher_copy: [u8; 16] = chunk.try_into().unwrap();
            let block = aes::Block::from_mut_slice(chunk);
            cipher.decrypt_block(block);
            // XOR with previous ciphertext (CBC mode).
            for (b, p) in chunk.iter_mut().zip(prev_block.iter()) {
                *b ^= p;
            }
            prev_block = cipher_copy;
        }

        // Remove PKCS7 padding.
        if let Some(&pad_len) = data.last() {
            let pad_len = pad_len as usize;
            if pad_len > 0 && pad_len <= 16 && data.len() >= pad_len {
                if data[data.len() - pad_len..]
                    .iter()
                    .all(|&b| b == pad_len as u8)
                {
                    data.truncate(data.len() - pad_len);
                }
            }
        }

        let header: Vec<u8> = data.iter().take(16).copied().collect();
        info!(decrypted_size = data.len(), header = ?header, "WeCom: media decrypted");
        Ok(data)
    }

    // -----------------------------------------------------------------------
    // Active send via WS
    // -----------------------------------------------------------------------

    /// Send a markdown message to a chat via `aibot_send_msg`.
    fn send_markdown(&self, chat_id: &str, text: &str) {
        let frame = json!({
            "cmd": "aibot_send_msg",
            "headers": { "req_id": uuid::Uuid::new_v4().to_string() },
            "body": {
                "chatid": chat_id,
                "msgtype": "markdown",
                "markdown": { "content": text },
            }
        });
        if self.ws_tx.send(frame.to_string()).is_err() {
            error!("WeCom: failed to enqueue outbound message (WS not connected)");
        }
    }

    // -----------------------------------------------------------------------
    // Media upload helpers
    // -----------------------------------------------------------------------

    /// Send a JSON frame via ws_tx and wait for the matching response.
    ///
    /// Registers a oneshot waiter keyed on `req_id` before sending, so the
    /// read loop can deliver the response back here.
    async fn send_rpc(&self, req_id: &str, frame: Value) -> Result<Value> {
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending_responses.lock().await;
            pending.insert(req_id.to_owned(), tx);
        }
        if self.ws_tx.send(frame.to_string()).is_err() {
            // Remove the pending entry to avoid a leak.
            let mut pending = self.pending_responses.lock().await;
            pending.remove(req_id);
            bail!("WeCom: WS not connected, cannot send RPC");
        }
        let resp = tokio::time::timeout(UPLOAD_STEP_TIMEOUT, rx)
            .await
            .context("WeCom: RPC response timeout")?
            .context("WeCom: RPC waiter channel dropped")?;
        Ok(resp)
    }

    /// Upload raw image bytes via the 3-step WeCom media upload protocol and
    /// return the resulting `media_id`.
    async fn upload_media(&self, bytes: &[u8]) -> Result<String> {
        let total_size = bytes.len();
        let chunk_count = (total_size + UPLOAD_CHUNK_SIZE - 1).max(1) / UPLOAD_CHUNK_SIZE;

        // Compute MD5 using sha2 crate via hex encoding of SHA-256 as a
        // checksum stand-in.  WeCom validates this field; send empty string
        // if a real md5 crate is unavailable.  In practice the server may
        // only use it for deduplication and accepts an empty value.
        let md5_hex = {
            use md5::{Digest, Md5};
            hex::encode(Md5::digest(bytes))
        };

        // --- Step 1: Init ---
        let init_req_id = uuid::Uuid::new_v4().to_string();
        let init_frame = json!({
            "cmd": "aibot_upload_media_init",
            "headers": { "req_id": &init_req_id },
            "body": {
                "type": "image",
                "filename": "image.png",
                "total_size": total_size,
                "total_chunks": chunk_count,
                "md5": md5_hex,
            }
        });
        debug!(req_id = %init_req_id, total_size, chunk_count, "WeCom: upload init");
        let init_resp = self
            .send_rpc(&init_req_id, init_frame)
            .await
            .context("WeCom: upload init RPC")?;

        let errcode = init_resp
            .get("errcode")
            .and_then(|c| c.as_i64())
            .unwrap_or(-1);
        if errcode != 0 {
            let errmsg = init_resp
                .get("errmsg")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown");
            bail!("WeCom: upload init failed ({errcode}): {errmsg}");
        }
        let upload_id = init_resp
            .get("body")
            .and_then(|b| b.get("upload_id"))
            .and_then(|u| u.as_str())
            .context("WeCom: upload init response missing upload_id")?
            .to_owned();
        debug!(upload_id = %upload_id, "WeCom: upload init OK");

        // --- Step 2: Chunks ---
        for (chunk_index, chunk) in bytes.chunks(UPLOAD_CHUNK_SIZE).enumerate() {
            let b64 = base64::engine::general_purpose::STANDARD.encode(chunk);
            let chunk_req_id = uuid::Uuid::new_v4().to_string();
            let chunk_frame = json!({
                "cmd": "aibot_upload_media_chunk",
                "headers": { "req_id": &chunk_req_id },
                "body": {
                    "upload_id": &upload_id,
                    "chunk_index": chunk_index,
                    "base64_data": b64,
                }
            });
            debug!(
                req_id = %chunk_req_id,
                chunk_index,
                chunk_size = chunk.len(),
                "WeCom: upload chunk"
            );
            let chunk_resp = self
                .send_rpc(&chunk_req_id, chunk_frame)
                .await
                .with_context(|| format!("WeCom: upload chunk {chunk_index} RPC"))?;
            let errcode = chunk_resp
                .get("errcode")
                .and_then(|c| c.as_i64())
                .unwrap_or(-1);
            if errcode != 0 {
                let errmsg = chunk_resp
                    .get("errmsg")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown");
                bail!("WeCom: upload chunk {chunk_index} failed ({errcode}): {errmsg}");
            }
        }

        // --- Step 3: Finish ---
        let finish_req_id = uuid::Uuid::new_v4().to_string();
        let finish_frame = json!({
            "cmd": "aibot_upload_media_finish",
            "headers": { "req_id": &finish_req_id },
            "body": {
                "upload_id": &upload_id,
            }
        });
        debug!(req_id = %finish_req_id, upload_id = %upload_id, "WeCom: upload finish");
        let finish_resp = self
            .send_rpc(&finish_req_id, finish_frame)
            .await
            .context("WeCom: upload finish RPC")?;
        let errcode = finish_resp
            .get("errcode")
            .and_then(|c| c.as_i64())
            .unwrap_or(-1);
        if errcode != 0 {
            let errmsg = finish_resp
                .get("errmsg")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown");
            bail!("WeCom: upload finish failed ({errcode}): {errmsg}");
        }
        let media_id = finish_resp
            .get("body")
            .and_then(|b| b.get("media_id"))
            .and_then(|m| m.as_str())
            .context("WeCom: upload finish response missing media_id")?
            .to_owned();
        info!(media_id = %media_id, "WeCom: media upload complete");
        Ok(media_id)
    }

    /// Send an image message using a previously uploaded `media_id`.
    async fn send_image_message(&self, chat_id: &str, media_id: &str) -> Result<()> {
        let req_id = uuid::Uuid::new_v4().to_string();
        let frame = json!({
            "cmd": "aibot_send_msg",
            "headers": { "req_id": &req_id },
            "body": {
                "chatid": chat_id,
                "msgtype": "image",
                "image": { "media_id": media_id },
            }
        });
        debug!(req_id = %req_id, chat_id, media_id, "WeCom: sending image message");
        let resp = self
            .send_rpc(&req_id, frame)
            .await
            .context("WeCom: send image message RPC")?;
        let errcode = resp.get("errcode").and_then(|c| c.as_i64()).unwrap_or(-1);
        if errcode != 0 {
            let errmsg = resp
                .get("errmsg")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown");
            bail!("WeCom: send image message failed ({errcode}): {errmsg}");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Channel trait implementation
// ---------------------------------------------------------------------------

impl Channel for WeComChannel {
    fn name(&self) -> &str {
        "wecom"
    }

    fn send(&self, msg: OutboundMessage) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let limit = platform_chunk_limit("wecom");
            let cfg = ChunkConfig {
                max_chars: limit,
                min_chars: 1,
                break_preference: BreakPreference::Paragraph,
            };
            for chunk in &chunk_text(&msg.text, &cfg) {
                self.send_markdown(&msg.target_id, chunk);
            }
            // Upload and send each image via the WS media upload protocol.
            for (idx, image) in msg.images.iter().enumerate() {
                // Images arrive as data URLs: "data:<mime>;base64,<data>"
                let raw_bytes = if let Some(comma_pos) = image.find(',') {
                    let b64 = &image[comma_pos + 1..];
                    match base64::engine::general_purpose::STANDARD.decode(b64) {
                        Ok(bytes) => bytes,
                        Err(e) => {
                            warn!(idx, "WeCom: failed to decode image base64: {e}");
                            continue;
                        }
                    }
                } else {
                    warn!(idx, "WeCom: image data is not a data URL, skipping");
                    continue;
                };

                match self.upload_media(&raw_bytes).await {
                    Ok(media_id) => {
                        if let Err(e) = self.send_image_message(&msg.target_id, &media_id).await {
                            error!(idx, "WeCom: send image message failed: {e:#}");
                        }
                    }
                    Err(e) => {
                        error!(idx, "WeCom: media upload failed: {e:#}");
                    }
                }
            }
            Ok(())
        })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            // Take the outbound receiver -- this can only be called once.
            let outbound_rx = self
                .ws_rx
                .lock()
                .expect("ws_rx lock")
                .take()
                .expect("WeComChannel::run() called more than once");

            info!(bot_id = %self.bot_id, "WeCom AI Bot WS channel starting");
            self.ws_loop(outbound_rx).await;
            #[allow(unreachable_code)]
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Simple MIME type guess from filename extension.
fn guess_mime(filename: &str) -> String {
    let lower = filename.to_lowercase();
    if lower.ends_with(".pdf") {
        "application/pdf"
    } else if lower.ends_with(".doc") || lower.ends_with(".docx") {
        "application/msword"
    } else if lower.ends_with(".xls") || lower.ends_with(".xlsx") {
        "application/vnd.ms-excel"
    } else if lower.ends_with(".ppt") || lower.ends_with(".pptx") {
        "application/vnd.ms-powerpoint"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".txt") {
        "text/plain"
    } else if lower.ends_with(".json") {
        "application/json"
    } else if lower.ends_with(".zip") {
        "application/zip"
    } else {
        "application/octet-stream"
    }
    .to_owned()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_name() {
        let ch = WeComChannel::new(
            "bot_123",
            "secret_abc",
            None,
            Arc::new(|_, _, _, _, _, _| {}),
        );
        assert_eq!(ch.name(), "wecom");
    }

    #[test]
    fn default_ws_url() {
        let ch = WeComChannel::new(
            "bot_123",
            "secret_abc",
            None,
            Arc::new(|_, _, _, _, _, _| {}),
        );
        assert_eq!(ch.ws_url, DEFAULT_WS_URL);
    }

    #[test]
    fn custom_ws_url() {
        let ch = WeComChannel::new(
            "bot_123",
            "secret_abc",
            Some("wss://custom.example.com".to_owned()),
            Arc::new(|_, _, _, _, _, _| {}),
        );
        assert_eq!(ch.ws_url, "wss://custom.example.com");
    }

    #[test]
    fn send_markdown_enqueues() {
        let ch = WeComChannel::new(
            "bot_123",
            "secret_abc",
            None,
            Arc::new(|_, _, _, _, _, _| {}),
        );
        ch.send_markdown("chat_1", "hello");
        // Verify the frame was enqueued (rx still exists in ws_rx).
        let mut rx = ch.ws_rx.lock().unwrap().take().unwrap();
        let frame_str = rx.try_recv().expect("should have a queued frame");
        let frame: Value = serde_json::from_str(&frame_str).unwrap();
        assert_eq!(frame["cmd"], "aibot_send_msg");
        assert_eq!(frame["body"]["chatid"], "chat_1");
        assert_eq!(frame["body"]["msgtype"], "markdown");
        assert_eq!(frame["body"]["markdown"]["content"], "hello");
    }
}
