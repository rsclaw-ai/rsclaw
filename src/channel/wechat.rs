//! Personal WeChat (个人微信) channel driver via Tencent ilink API.
//!
//! Uses the same backend as the official `openclaw-weixin` plugin:
//!   Base URL: https://ilinkai.weixin.qq.com
//!
//! Flow:
//!   1. QR code login: get_bot_qrcode → scan → get_qrcode_status → bot_token
//!   2. Long-poll: getupdates with get_updates_buf cursor
//!   3. Send: sendmessage with to_user_id + content
//!
//! Config in openclaw.json:
//!   channels.wechat.enabled: true

use std::{sync::Arc, time::Duration};

use aes::cipher::{BlockEncrypt, KeyInit};
use anyhow::{Context, Result, bail};
use base64::Engine;
use futures::future::BoxFuture;
use md5::{Digest, Md5};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use super::{Channel, OutboundMessage};
use crate::channel::chunker::{ChunkConfig, chunk_text};

const ILINK_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
const DEFAULT_BOT_TYPE: &str = "3";
const LONG_POLL_TIMEOUT_MS: u64 = 35_000;
const SEND_TIMEOUT_MS: u64 = 15_000;

/// Build the common ilink API headers.
fn ilink_headers(token: &str, body_len: usize) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("Content-Type", "application/json".parse().unwrap());
    headers.insert("AuthorizationType", "ilink_bot_token".parse().unwrap());
    headers.insert("Content-Length", body_len.to_string().parse().unwrap());
    if !token.is_empty() {
        headers.insert("Authorization", format!("Bearer {token}").parse().unwrap());
    }
    // X-WECHAT-UIN: random uint32 → decimal string → base64 (simple inline)
    let uin: u32 = rand::random();
    let uin_str = uin.to_string();
    // Minimal base64 encode for a short decimal string
    const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut b64 = String::new();
    for chunk in uin_str.as_bytes().chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let t = (b0 << 16) | (b1 << 8) | b2;
        b64.push(B64[((t >> 18) & 0x3F) as usize] as char);
        b64.push(B64[((t >> 12) & 0x3F) as usize] as char);
        b64.push(if chunk.len() > 1 {
            B64[((t >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        b64.push(if chunk.len() > 2 {
            B64[(t & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    headers.insert("X-WECHAT-UIN", b64.parse().unwrap());
    headers
}

// ---------------------------------------------------------------------------
// API types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct QrCodeResponse {
    qrcode: Option<String>,
    qrcode_img_content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct QrStatusResponse {
    status: Option<String>,
    bot_token: Option<String>,
    ilink_bot_id: Option<String>,
    ilink_user_id: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct GetUpdatesResponse {
    ret: Option<i32>,
    msgs: Option<Vec<WeixinMessage>>,
    get_updates_buf: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
struct WeixinMessage {
    from_user_id: Option<String>,
    to_user_id: Option<String>,
    message_type: Option<i32>,
    message_state: Option<i32>,
    item_list: Option<Vec<MessageItem>>,
    context_token: Option<String>,
}

// Message item types (ilink API):
// 0 = NONE, 1 = TEXT, 2 = IMAGE, 3 = VOICE, 4 = FILE, 5 = VIDEO

#[derive(Debug, Clone, Deserialize)]
struct MessageItem {
    #[serde(rename = "type")]
    item_type: Option<i32>,
    text_item: Option<TextItem>,
    voice_item: Option<VoiceItem>,
    file_item: Option<FileItem>,
    image_item: Option<ImageItem>,
    video_item: Option<VideoItem>,
}

#[derive(Debug, Clone, Deserialize)]
struct TextItem {
    text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct VoiceItem {
    voice_url: Option<String>,
    /// Voice-to-text result from WeChat (if available, skip transcription).
    text: Option<String>,
    /// ilink v2: nested media object with encrypt_query_param
    media: Option<MediaRef>,
}

#[derive(Debug, Clone, Deserialize)]
struct MediaRef {
    encrypt_query_param: Option<String>,
    aes_key: Option<String>,
}

/// Default WeChat CDN base URL for media download.
const WECHAT_CDN_BASE: &str = "https://novac2c.cdn.weixin.qq.com/c2c";

/// Media source: direct URL or CDN media (encrypt_query_param + aes_key).
#[derive(Debug)]
enum MediaSource {
    Url(String),
    Cdn { encrypt_query_param: String, aes_key: String },
}

#[derive(Debug, Clone, Deserialize)]
struct FileItem {
    file_url: Option<String>,
    file_name: Option<String>,
    media: Option<MediaRef>,
}

#[derive(Debug, Clone, Deserialize)]
struct ImageItem {
    image_url: Option<String>,
    /// Top-level hex AES key (image-specific, takes priority over media.aes_key).
    aeskey: Option<String>,
    /// ilink v2: nested media object with encrypt_query_param
    media: Option<MediaRef>,
}

#[derive(Debug, Clone, Deserialize)]
struct VideoItem {
    media: Option<MediaRef>,
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
struct SendMessageReq {
    to_user_id: String,
    content: String,
    msg_type: i32,
    base_info: serde_json::Value,
}

/// Response from ilink/bot/getuploadurl.
/// API may return either `upload_param` (raw param) or `upload_full_url` (full CDN URL).
#[derive(Debug, Deserialize)]
struct GetUploadUrlResponse {
    upload_param: Option<String>,
    /// Some API versions return the full upload URL instead of just the param.
    upload_full_url: Option<String>,
    #[allow(dead_code)]
    thumb_upload_param: Option<String>,
}

/// Info about a successfully uploaded file on the WeChat CDN.
#[derive(Debug)]
#[allow(dead_code)]
struct UploadedFileInfo {
    /// CDN download encrypted_query_param (returned by CDN after upload).
    download_param: String,
    /// AES-128-ECB key as hex string (32 hex chars = 16 bytes).
    aes_key_hex: String,
    /// Plaintext file size in bytes.
    file_size: usize,
    /// Ciphertext (AES-padded) file size in bytes.
    file_size_ciphertext: usize,
}

/// ilink media type for getuploadurl.
#[derive(Debug, Clone, Copy)]
#[repr(i32)]
#[allow(dead_code)]
enum UploadMediaType {
    Image = 1,
    #[allow(dead_code)]
    Video = 2,
    File = 3,
    #[allow(dead_code)]
    Voice = 4,
}

const UPLOAD_MAX_RETRIES: u32 = 3;

// ---------------------------------------------------------------------------
// WeChatPersonalChannel
// ---------------------------------------------------------------------------

pub struct WeChatPersonalChannel {
    base_url: String,
    bot_token: String,
    client: Client,
    #[allow(clippy::type_complexity)]
    on_message: Arc<dyn Fn(String, String, Vec<crate::agent::registry::ImageAttachment>, Vec<crate::agent::registry::FileAttachment>) + Send + Sync>,
}

impl WeChatPersonalChannel {
    /// Create from a saved bot_token (after QR login).
    pub fn new(bot_token: String, on_message: Arc<dyn Fn(String, String, Vec<crate::agent::registry::ImageAttachment>, Vec<crate::agent::registry::FileAttachment>) + Send + Sync>) -> Self {
        Self {
            base_url: ILINK_BASE_URL.to_owned(),
            bot_token,
            client: Client::builder()
                .timeout(Duration::from_millis(LONG_POLL_TIMEOUT_MS + 5000))
                .build()
                .expect("http client"),
            on_message,
        }
    }

    /// Override the ilink API base URL. Used for testing with a mock server.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_owned();
        self
    }

    // -----------------------------------------------------------------------
    // QR code login (static — used before creating the channel)
    // -----------------------------------------------------------------------

    /// Start QR code login. Returns (qrcode_url, session_qrcode) for polling.
    pub async fn start_qr_login(client: &Client) -> Result<(String, String)> {
        let url = format!(
            "{}/ilink/bot/get_bot_qrcode?bot_type={}",
            ILINK_BASE_URL, DEFAULT_BOT_TYPE
        );
        let resp: QrCodeResponse = client
            .post(&url)
            .header("Content-Type", "application/json")
            .body("{}")
            .send()
            .await?
            .json()
            .await?;

        let qrcode = resp.qrcode.context("no qrcode in response")?;
        // qrcode_img_content is the URL to display as QR code (for scanning).
        // qrcode is the token to poll get_qrcode_status with.
        let qrcode_url = resp
            .qrcode_img_content
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("https://login.weixin.qq.com/qrcode/{}", qrcode));

        // Display QR code in terminal
        crate::channel::auth::display_qr_terminal(&qrcode_url)?;

        Ok((qrcode_url, qrcode))
    }

    /// Poll for QR code scan result. Returns (bot_token, bot_id) on success.
    pub async fn wait_qr_login(client: &Client, qrcode: &str) -> Result<(String, String)> {
        let url = format!(
            "{}/ilink/bot/get_qrcode_status?qrcode={}",
            ILINK_BASE_URL, qrcode
        );

        println!("Waiting for WeChat scan...");

        for attempt in 0..60 {
            let resp: QrStatusResponse = client
                .post(&url)
                .header("Content-Type", "application/json")
                .body("{}")
                .timeout(Duration::from_millis(LONG_POLL_TIMEOUT_MS))
                .send()
                .await?
                .json()
                .await?;

            match resp.status.as_deref() {
                Some("confirmed") => {
                    let token = resp.bot_token.context("no bot_token after confirmed")?;
                    let bot_id = resp
                        .ilink_bot_id
                        .context("no ilink_bot_id after confirmed")?;
                    info!(bot_id = %bot_id, "WeChat login confirmed");

                    // Save token
                    crate::channel::auth::save_token(
                        "wechat",
                        &json!({
                            "bot_token": token,
                            "ilink_bot_id": bot_id,
                            "ilink_user_id": resp.ilink_user_id,
                        }),
                    )?;

                    return Ok((token, bot_id));
                }
                Some("scaned") => {
                    if attempt == 0 {
                        println!("Scanned! Please confirm on your phone...");
                    }
                }
                Some("expired") => {
                    bail!("QR code expired");
                }
                _ => {}
            }

            sleep(Duration::from_secs(2)).await;
        }

        bail!("QR login timed out (2 minutes)")
    }

    /// Single-shot poll for QR scan status. Returns:
    /// - Ok(Some((bot_token, bot_id))) if confirmed
    /// - Ok(None) if still waiting/scanned
    /// - Err if expired or failed
    pub async fn poll_qr_status(client: &Client, qrcode: &str) -> Result<Option<(String, String)>> {
        let url = format!(
            "{}/ilink/bot/get_qrcode_status?qrcode={}",
            ILINK_BASE_URL, qrcode
        );
        let resp: QrStatusResponse = client
            .post(&url)
            .header("Content-Type", "application/json")
            .body("{}")
            .timeout(Duration::from_millis(LONG_POLL_TIMEOUT_MS))
            .send()
            .await?
            .json()
            .await?;

        match resp.status.as_deref() {
            Some("confirmed") => {
                let token = resp.bot_token.context("no bot_token after confirmed")?;
                let bot_id = resp
                    .ilink_bot_id
                    .context("no ilink_bot_id after confirmed")?;

                crate::channel::auth::save_token(
                    "wechat",
                    &json!({
                        "bot_token": token,
                        "ilink_bot_id": bot_id,
                        "ilink_user_id": resp.ilink_user_id,
                    }),
                )?;

                Ok(Some((token, bot_id)))
            }
            Some("expired") => bail!("QR code expired"),
            _ => Ok(None), // waiting or scanned
        }
    }

    // -----------------------------------------------------------------------
    // Message sending
    // -----------------------------------------------------------------------

    async fn send_text(&self, to_user_id: &str, text: &str) -> Result<()> {
        let url = format!("{}/ilink/bot/sendmessage", self.base_url);
        let client_id = uuid::Uuid::new_v4().to_string();
        let body = json!({
            "msg": {
                "from_user_id": "",
                "to_user_id": to_user_id,
                "client_id": client_id,
                "message_type": 2,
                "message_state": 2,
                "item_list": [{
                    "type": 1,
                    "text_item": { "text": text }
                }]
            },
            "base_info": base_info(),
        });

        let body_str = serde_json::to_string(&body).unwrap_or_default();
        let headers = ilink_headers(&self.bot_token, body_str.len());
        let resp = self
            .client
            .post(&url)
            .headers(headers)
            .timeout(Duration::from_millis(SEND_TIMEOUT_MS))
            .body(body_str)
            .send()
            .await?;

        let status = resp.status();
        let resp_body = resp.text().await.unwrap_or_default();
        debug!(status = %status, "wechat: sendmessage ok");
        if !status.is_success() {
            bail!("sendmessage failed: {status} {resp_body}");
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Long-polling
    // -----------------------------------------------------------------------

    async fn poll_loop(self: Arc<Self>) -> Result<()> {
        let mut updates_buf = String::new();

        // Try to load saved state
        if let Some(saved) = crate::channel::auth::load_token("wechat")
            && let Some(buf) = saved.get("get_updates_buf").and_then(|v| v.as_str())
        {
            updates_buf = buf.to_owned();
            debug!(
                buf_len = updates_buf.len(),
                "restored updates_buf from saved state"
            );
        }

        info!("WeChat personal long-poll loop started");

        loop {
            match self.get_updates(&updates_buf).await {
                Ok(resp) => {
                    if let Some(new_buf) = resp.get_updates_buf {
                        updates_buf = new_buf;
                    }

                    if let Some(msgs) = resp.msgs {
                        info!(count = msgs.len(), "wechat: received updates");
                        for msg in &msgs {
                            // Log raw item types for debugging
                            if let Some(items) = &msg.item_list {
                                for item in items {
                                    debug!(
                                        item_type = ?item.item_type,
                                        has_text = item.text_item.is_some(),
                                        has_voice = item.voice_item.is_some(),
                                        has_file = item.file_item.is_some(),
                                        has_image = item.image_item.is_some(),
                                        "wechat: message item"
                                    );
                                }
                            }
                        }
                        for msg in msgs {
                            let from = msg.from_user_id.unwrap_or_default();
                            // message_type: 1 = user, 2 = bot (skip bot messages to avoid echo)
                            if msg.message_type == Some(2) {
                                continue;
                            }

                            // Process items: text, voice, image, file, video
                            let items = msg.item_list.as_deref().unwrap_or(&[]);

                            // 1. Text (type 1)
                            if let Some(t) = items.iter().find_map(|i| {
                                i.text_item.as_ref().and_then(|t| t.text.clone())
                            }) {
                                if !from.is_empty() && !t.is_empty() {
                                    info!(from = %from, text_len = t.len(), "wechat: text message");
                                    (self.on_message)(from.clone(), t, vec![], vec![]);
                                }
                                continue;
                            }

                            // 2. Voice (type 3) -- prefer WeChat STT, else download+decode+transcribe
                            if let Some(v) = items.iter().find_map(|i| i.voice_item.as_ref()) {
                                // WeChat's own speech-to-text
                                if let Some(stt) = &v.text {
                                    if !stt.is_empty() {
                                        info!(chars = stt.len(), "wechat: using WeChat voice-to-text");
                                        if !from.is_empty() {
                                            (self.on_message)(from.clone(), stt.clone(), vec![], vec![]);
                                        }
                                        continue;
                                    }
                                }
                                // Download and transcribe
                                let src = resolve_media_source_voice(v);
                                if let Some(src) = src {
                                    let audio = self.download_media_source(&src).await;
                                    match audio {
                                        Ok(bytes) => {
                                            match crate::channel::transcription::transcribe_audio(
                                                &self.client, &bytes, "voice.silk", "audio/silk",
                                            ).await {
                                                Ok(t) if !t.is_empty() => {
                                                    info!(chars = t.len(), "wechat: voice transcribed");
                                                    if !from.is_empty() {
                                                        (self.on_message)(from.clone(), t, vec![], vec![]);
                                                    }
                                                }
                                                Ok(_) => warn!("wechat: voice transcription returned empty"),
                                                Err(e) => warn!("wechat: voice transcription failed: {e:#}"),
                                            }
                                        }
                                        Err(e) => warn!("wechat: voice download failed: {e:#}"),
                                    }
                                }
                                continue;
                            }

                            // 3. Image (type 2)
                            if let Some(img) = items.iter().find_map(|i| i.image_item.as_ref()) {
                                let src = resolve_media_source_image(img);
                                debug!(
                                    has_image_url = img.image_url.is_some(),
                                    has_media = img.media.is_some(),
                                    has_aeskey = img.aeskey.is_some(),
                                    has_src = src.is_some(),
                                    "wechat: image item"
                                );
                                if let Some(src) = src {
                                    match self.download_media_source(&src).await {
                                        Ok(bytes) => {
                                            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                                            let data_url = format!("data:image/jpeg;base64,{b64}");
                                            let images = vec![crate::agent::registry::ImageAttachment {
                                                data: data_url,
                                                mime_type: "image/jpeg".to_string(),
                                            }];
                                            info!(size = bytes.len(), "wechat: image received");
                                            if !from.is_empty() {
                                                (self.on_message)(from.clone(), crate::i18n::t("describe_image", crate::i18n::default_lang()), images, vec![]);
                                            }
                                        }
                                        Err(e) => warn!("wechat: image download failed: {e:#}"),
                                    }
                                }
                                continue;
                            }

                            // 4. File (type 4) -- pass raw bytes as FileAttachment
                            if let Some(f) = items.iter().find_map(|i| i.file_item.as_ref()) {
                                let src = resolve_media_source_file(f);
                                if let Some(src) = src {
                                    match self.download_media_source(&src).await {
                                        Ok(bytes) => {
                                            let fname = f.file_name.as_deref().unwrap_or("file.bin");
                                            info!(size = bytes.len(), fname, "wechat: file received, routing to agent");
                                            let fa = crate::agent::registry::FileAttachment {
                                                filename: fname.to_owned(),
                                                data: bytes,
                                                mime_type: "application/octet-stream".to_owned(),
                                            };
                                            if !from.is_empty() {
                                                (self.on_message)(from.clone(), String::new(), vec![], vec![fa]);
                                            }
                                        }
                                        Err(e) => warn!("wechat: file download failed: {e:#}"),
                                    }
                                }
                                continue;
                            }

                            // 5. Video (type 5) -- download, decrypt, save as file
                            if let Some(vid) = items.iter().find_map(|i| i.video_item.as_ref()) {
                                if let Some(m) = &vid.media {
                                    if let Some(param) = &m.encrypt_query_param {
                                        let aes_key = m.aes_key.clone().unwrap_or_default();
                                        let src = MediaSource::Cdn {
                                            encrypt_query_param: param.clone(),
                                            aes_key,
                                        };
                                        match self.download_media_source(&src).await {
                                            Ok(bytes) => {
                                                info!(size = bytes.len(), "wechat: video downloaded");
                                                // Send as both ImageAttachment (for vision models
                                                // that support video) and FileAttachment (for
                                                // audio transcription fallback on other models).
                                                use base64::Engine;
                                                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                                                let data_uri = format!("data:video/mp4;base64,{b64}");
                                                let img = crate::agent::registry::ImageAttachment {
                                                    data: data_uri,
                                                    mime_type: "video/mp4".to_owned(),
                                                };
                                                let fa = crate::agent::registry::FileAttachment {
                                                    filename: "video.mp4".to_owned(),
                                                    data: bytes,
                                                    mime_type: "video/mp4".to_owned(),
                                                };
                                                if !from.is_empty() {
                                                    (self.on_message)(from.clone(), crate::i18n::t("describe_video", crate::i18n::default_lang()), vec![img], vec![fa]);
                                                }
                                            }
                                            Err(e) => warn!("wechat: video download failed: {e:#}"),
                                        }
                                    }
                                }
                                continue;
                            }

                            debug!("wechat: unhandled message item types: {:?}", items.iter().map(|i| i.item_type).collect::<Vec<_>>());
                        }
                    }
                }
                Err(e) => {
                    warn!("wechat getupdates error: {e:#}");
                    sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn get_updates(&self, updates_buf: &str) -> Result<GetUpdatesResponse> {
        let url = format!("{}/ilink/bot/getupdates", self.base_url);
        let body = json!({
            "get_updates_buf": updates_buf,
            "base_info": base_info(),
        });

        let body_str = serde_json::to_string(&body).unwrap_or_default();
        let headers = ilink_headers(&self.bot_token, body_str.len());
        debug!(buf_len = updates_buf.len(), "wechat: calling getupdates");

        let resp = self
            .client
            .post(&url)
            .headers(headers)
            .timeout(Duration::from_millis(LONG_POLL_TIMEOUT_MS + 5000))
            .body(body_str)
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            info!(status = %status, body = &text[..text.len().min(200)], "wechat: getupdates error");
            bail!("getupdates failed: {status} {text}");
        }

        let parsed: GetUpdatesResponse = serde_json::from_str(&text).with_context(|| {
            format!(
                "wechat: parse getupdates response: {}",
                &text[..text.len().min(300)]
            )
        })?;

        let msg_count = parsed.msgs.as_ref().map(|m| m.len()).unwrap_or(0);
        if msg_count > 0 {
            info!(msgs = msg_count, "wechat: received updates");
        }

        Ok(parsed)
    }

    /// Download media from WeChat CDN and decrypt with AES-128-ECB.
    ///
    /// Flow: GET cdn_url?encrypted_query_param=... -> AES-128-ECB decrypt -> raw bytes
    async fn download_cdn_media(&self, encrypt_query_param: &str, aes_key_b64: &str) -> Result<Vec<u8>> {
        use aes::cipher::{BlockDecrypt, KeyInit};

        let url = format!(
            "{}/download?encrypted_query_param={}",
            WECHAT_CDN_BASE,
            percent_encode(encrypt_query_param),
        );
        debug!(
            url_len = url.len(),
            param_len = encrypt_query_param.len(),
            aes_key_len = aes_key_b64.len(),
            "wechat: CDN download attempt"
        );

        let resp = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(30))
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            warn!(
                status = %status,
                body = &text[..text.len().min(500)],
                param_len = encrypt_query_param.len(),
                has_aes_key = !aes_key_b64.is_empty(),
                "wechat: CDN download failed"
            );
            bail!("CDN download failed: {status} {}", &text[..text.len().min(200)]);
        }

        let encrypted = resp.bytes().await?;
        info!(size = encrypted.len(), "wechat: CDN download ok");

        if aes_key_b64.is_empty() {
            // No AES key -- return raw bytes (might already be decrypted)
            return Ok(encrypted.to_vec());
        }

        // Decode AES key: base64 -> try as hex string (voice/video) or raw 16 bytes (image)
        let key_decoded = base64::engine::general_purpose::STANDARD
            .decode(aes_key_b64)
            .context("invalid base64 aes_key")?;

        let key_bytes: Vec<u8> = if key_decoded.len() == 32 {
            // It's a hex string of 16 bytes: "aabbccdd..." -> [0xaa, 0xbb, ...]
            (0..16)
                .map(|i| {
                    let hex = std::str::from_utf8(&key_decoded[i * 2..i * 2 + 2])
                        .unwrap_or("00");
                    u8::from_str_radix(hex, 16).unwrap_or(0)
                })
                .collect()
        } else if key_decoded.len() == 16 {
            // Raw 16 bytes
            key_decoded
        } else {
            bail!("unexpected aes_key length: {} (expected 16 or 32)", key_decoded.len());
        };

        // AES-128-ECB decryption
        let cipher = aes::Aes128::new_from_slice(&key_bytes)
            .context("AES key init failed")?;

        let mut data = encrypted.to_vec();
        // Pad to 16-byte boundary if needed
        let pad = (16 - data.len() % 16) % 16;
        data.extend(std::iter::repeat(0u8).take(pad));

        for chunk in data.chunks_mut(16) {
            let block = aes::Block::from_mut_slice(chunk);
            cipher.decrypt_block(block);
        }

        // Remove PKCS7 padding
        if let Some(&last) = data.last() {
            let pad_len = last as usize;
            if pad_len > 0 && pad_len <= 16 && data.len() >= pad_len {
                let valid = data[data.len() - pad_len..].iter().all(|&b| b == last);
                if valid {
                    data.truncate(data.len() - pad_len);
                }
            }
        }

        info!(decrypted_size = data.len(), "wechat: media decrypted");
        Ok(data)
    }

    /// Download from either a URL or CDN media source.
    async fn download_media_source(&self, src: &MediaSource) -> Result<Vec<u8>> {
        match src {
            MediaSource::Url(url) => {
                crate::channel::transcription::download_file(&self.client, url).await
            }
            MediaSource::Cdn { encrypt_query_param, aes_key } => {
                self.download_cdn_media(encrypt_query_param, aes_key).await
            }
        }
    }

    // -----------------------------------------------------------------------
    // CDN upload (mirror of download_cdn_media)
    // -----------------------------------------------------------------------

    /// AES-128-ECB encrypt with PKCS7 padding (mirror of decryption in download_cdn_media).
    fn aes_ecb_encrypt(plaintext: &[u8], key: &[u8; 16]) -> Vec<u8> {
        let cipher = aes::Aes128::new_from_slice(key).expect("valid 16-byte key");
        // PKCS7 padding
        let pad_len = 16 - (plaintext.len() % 16);
        let mut data = plaintext.to_vec();
        data.extend(std::iter::repeat(pad_len as u8).take(pad_len));
        // Encrypt each 16-byte block in-place
        for chunk in data.chunks_mut(16) {
            let block = aes::Block::from_mut_slice(chunk);
            cipher.encrypt_block(block);
        }
        data
    }

    /// Compute AES-128-ECB ciphertext size (PKCS7 padding to 16-byte boundary).
    fn aes_ecb_padded_size(plaintext_size: usize) -> usize {
        ((plaintext_size + 1 + 15) / 16) * 16
    }

    /// Call ilink/bot/getuploadurl to get an upload_param for CDN upload.
    async fn get_upload_url(
        &self,
        filekey: &str,
        media_type: UploadMediaType,
        to_user_id: &str,
        rawsize: usize,
        rawfilemd5: &str,
        filesize: usize,
        aeskey_hex: &str,
    ) -> Result<GetUploadUrlResponse> {
        let url = format!("{}/ilink/bot/getuploadurl", self.base_url);
        let body = json!({
            "filekey": filekey,
            "media_type": media_type as i32,
            "to_user_id": to_user_id,
            "rawsize": rawsize,
            "rawfilemd5": rawfilemd5,
            "filesize": filesize,
            "no_need_thumb": true,
            "aeskey": aeskey_hex,
            "base_info": base_info(),
        });

        let body_str = serde_json::to_string(&body).unwrap_or_default();
        let headers = ilink_headers(&self.bot_token, body_str.len());

        let resp = self
            .client
            .post(&url)
            .headers(headers)
            .timeout(Duration::from_millis(SEND_TIMEOUT_MS))
            .body(body_str)
            .send()
            .await
            .context("getuploadurl request failed")?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("getuploadurl failed: {status} {}", &text[..text.len().min(300)]);
        }

        debug!(response = &text[..text.len().min(500)], "wechat: getuploadurl response");
        let parsed: GetUploadUrlResponse = serde_json::from_str(&text)
            .with_context(|| format!("parse getuploadurl response: {}", &text[..text.len().min(300)]))?;
        if parsed.upload_param.is_none() {
            warn!(response = &text[..text.len().min(500)], "wechat: getuploadurl returned no upload_param");
        }
        Ok(parsed)
    }

    /// Upload a buffer to the WeChat CDN with AES-128-ECB encryption.
    ///
    /// Returns the CDN download `encrypted_query_param` from the `x-encrypted-param`
    /// response header.
    async fn upload_to_cdn_url(
        &self,
        plaintext: &[u8],
        cdn_url: &str,
        aes_key: &[u8; 16],
    ) -> Result<String> {
        let ciphertext = Self::aes_ecb_encrypt(plaintext, aes_key);
        debug!(
            ciphertext_len = ciphertext.len(),
            "wechat: CDN upload POST"
        );

        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 1..=UPLOAD_MAX_RETRIES {
            let resp = self
                .client
                .post(cdn_url)
                .header("Content-Type", "application/octet-stream")
                .timeout(Duration::from_secs(120))
                .body(ciphertext.clone())
                .send()
                .await;

            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    error!(attempt, "wechat: CDN upload network error: {e:#}");
                    last_err = Some(e.into());
                    continue;
                }
            };

            let status = resp.status();
            if status.as_u16() >= 400 && status.as_u16() < 500 {
                let err_msg = resp
                    .headers()
                    .get("x-error-message")
                    .and_then(|v| v.to_str().ok())
                    .map(String::from)
                    .unwrap_or_else(|| resp.status().to_string());
                bail!("CDN upload client error {status}: {err_msg}");
            }

            if status.as_u16() != 200 {
                let err_msg = resp
                    .headers()
                    .get("x-error-message")
                    .and_then(|v| v.to_str().ok())
                    .map(String::from)
                    .unwrap_or_else(|| format!("status {status}"));
                error!(attempt, "wechat: CDN upload server error: {err_msg}");
                last_err = Some(anyhow::anyhow!("CDN upload server error: {err_msg}"));
                continue;
            }

            // Success: extract download param from x-encrypted-param header
            let download_param = resp
                .headers()
                .get("x-encrypted-param")
                .and_then(|v| v.to_str().ok())
                .map(String::from)
                .context("CDN upload response missing x-encrypted-param header")?;

            debug!(attempt, "wechat: CDN upload success");
            return Ok(download_param);
        }

        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("CDN upload failed after {UPLOAD_MAX_RETRIES} attempts")))
    }

    /// Full upload pipeline: read bytes -> hash -> gen keys -> getuploadurl -> CDN upload.
    async fn upload_media(
        &self,
        plaintext: &[u8],
        to_user_id: &str,
        media_type: UploadMediaType,
    ) -> Result<UploadedFileInfo> {
        let rawsize = plaintext.len();
        let rawfilemd5 = hex::encode(Md5::digest(plaintext));
        let filesize = Self::aes_ecb_padded_size(rawsize);

        // Random filekey (32 hex chars) and AES key (16 bytes)
        let filekey = hex::encode(rand::random::<[u8; 16]>());
        let aes_key: [u8; 16] = rand::random();
        let aes_key_hex = hex::encode(aes_key);

        debug!(
            rawsize,
            filesize,
            md5 = %rawfilemd5,
            filekey = %filekey,
            "wechat: upload_media starting"
        );

        // Step 1: get upload_param from ilink API
        let upload_resp = self
            .get_upload_url(&filekey, media_type, to_user_id, rawsize, &rawfilemd5, filesize, &aes_key_hex)
            .await?;

        // API may return upload_param (raw) or upload_full_url (full CDN URL).
        let (cdn_upload_url, _upload_param_for_download) = if let Some(p) = upload_resp.upload_param.clone() {
            // Build URL ourselves.
            let url = format!(
                "{}/upload?encrypted_query_param={}&filekey={}",
                WECHAT_CDN_BASE,
                percent_encode(&p),
                percent_encode(&filekey),
            );
            (url, p)
        } else if let Some(full_url) = upload_resp.upload_full_url {
            // Use the full URL directly, add filekey if missing.
            let url = if full_url.contains("filekey=") {
                full_url.clone()
            } else {
                format!("{}&filekey={}", full_url, percent_encode(&filekey))
            };
            // Extract param for download_param response parsing.
            let param = upload_resp.upload_param.unwrap_or_default();
            (url, param)
        } else {
            bail!("getuploadurl returned neither upload_param nor upload_full_url");
        };

        // Step 2: AES encrypt + POST to CDN
        let download_param = self
            .upload_to_cdn_url(plaintext, &cdn_upload_url, &aes_key)
            .await?;

        info!(
            filekey = %filekey,
            rawsize,
            filesize,
            "wechat: media upload complete"
        );

        Ok(UploadedFileInfo {
            download_param,
            aes_key_hex,
            file_size: rawsize,
            file_size_ciphertext: filesize,
        })
    }

    // -----------------------------------------------------------------------
    // Outbound image/file sending
    // -----------------------------------------------------------------------

    /// Send an image message referencing a previously uploaded file.
    async fn send_image_message(
        &self,
        to_user_id: &str,
        uploaded: &UploadedFileInfo,
    ) -> Result<()> {
        let url = format!("{}/ilink/bot/sendmessage", self.base_url);
        let client_id = uuid::Uuid::new_v4().to_string();

        // Convert hex aeskey string to base64 (encode the hex string as UTF-8 bytes,
        // NOT hex-decoded -- matches openclaw's Buffer.from(hexstr).toString("base64"))
        let aes_key_b64 = base64::engine::general_purpose::STANDARD.encode(uploaded.aes_key_hex.as_bytes());

        let body = json!({
            "msg": {
                "from_user_id": "",
                "to_user_id": to_user_id,
                "client_id": client_id,
                "message_type": 2,
                "message_state": 2,
                "item_list": [{
                    "type": 2,
                    "image_item": {
                        "media": {
                            "encrypt_query_param": uploaded.download_param,
                            "aes_key": aes_key_b64,
                            "encrypt_type": 1
                        },
                        "mid_size": uploaded.file_size_ciphertext
                    }
                }]
            },
            "base_info": base_info(),
        });

        let body_str = serde_json::to_string(&body).unwrap_or_default();
        let headers = ilink_headers(&self.bot_token, body_str.len());
        let resp = self
            .client
            .post(&url)
            .headers(headers)
            .timeout(Duration::from_millis(SEND_TIMEOUT_MS))
            .body(body_str)
            .send()
            .await?;

        let status = resp.status();
        let resp_body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("send image message failed: {status} {resp_body}");
        }
        debug!(status = %status, "wechat: send_image_message ok");
        Ok(())
    }

    /// Parse a base64 data URI or raw URL image, upload to CDN, and send as image message.
    async fn send_outbound_image(&self, to_user_id: &str, image_data_uri: &str) -> Result<()> {
        // Decode base64 data URI: "data:image/png;base64,<data>"
        // Or download from a plain URL.
        let image_bytes = if let Some(rest) = image_data_uri.strip_prefix("data:") {
            // data:<mime>;base64,<payload>
            let b64_start = rest.find(",").context("invalid data URI: no comma")?;
            let payload = &rest[b64_start + 1..];
            base64::engine::general_purpose::STANDARD
                .decode(payload)
                .context("decode base64 image payload")?
        } else {
            // Treat as URL -- download it
            crate::channel::transcription::download_file(&self.client, image_data_uri).await?
        };

        if image_bytes.is_empty() {
            bail!("empty image payload");
        }

        let uploaded = self
            .upload_media(&image_bytes, to_user_id, UploadMediaType::Image)
            .await?;

        self.send_image_message(to_user_id, &uploaded).await
    }

    /// Send a file attachment message referencing a previously uploaded file.
    async fn send_file_message(
        &self,
        to_user_id: &str,
        file_name: &str,
        uploaded: &UploadedFileInfo,
    ) -> Result<()> {
        let url = format!("{}/ilink/bot/sendmessage", self.base_url);
        let client_id = uuid::Uuid::new_v4().to_string();

        let aes_key_bytes = hex::decode(&uploaded.aes_key_hex)
            .context("invalid hex aes_key")?;
        let aes_key_b64 = base64::engine::general_purpose::STANDARD.encode(&aes_key_bytes);

        let body = json!({
            "msg": {
                "from_user_id": "",
                "to_user_id": to_user_id,
                "client_id": client_id,
                "message_type": 2,
                "message_state": 2,
                "item_list": [{
                    "type": 4,
                    "file_item": {
                        "media": {
                            "encrypt_query_param": uploaded.download_param,
                            "aes_key": aes_key_b64,
                            "encrypt_type": 1
                        },
                        "file_name": file_name,
                        "len": uploaded.file_size.to_string()
                    }
                }]
            },
            "base_info": base_info(),
        });

        let body_str = serde_json::to_string(&body).unwrap_or_default();
        let headers = ilink_headers(&self.bot_token, body_str.len());
        let resp = self
            .client
            .post(&url)
            .headers(headers)
            .timeout(Duration::from_millis(SEND_TIMEOUT_MS))
            .body(body_str)
            .send()
            .await?;

        let status = resp.status();
        let resp_body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("send file message failed: {status} {resp_body}");
        }
        debug!(status = %status, "wechat: send_file_message ok");
        Ok(())
    }
}

/// Resolve media source from VoiceItem.
fn resolve_media_source_voice(v: &VoiceItem) -> Option<MediaSource> {
    if let Some(url) = &v.voice_url {
        return Some(MediaSource::Url(url.clone()));
    }
    if let Some(m) = &v.media {
        if let Some(param) = &m.encrypt_query_param {
            return Some(MediaSource::Cdn {
                encrypt_query_param: param.clone(),
                aes_key: m.aes_key.clone().unwrap_or_default(),
            });
        }
    }
    None
}

/// Resolve media source from ImageItem (hex aeskey -> base64 conversion).
fn resolve_media_source_image(img: &ImageItem) -> Option<MediaSource> {
    if let Some(url) = &img.image_url {
        return Some(MediaSource::Url(url.clone()));
    }
    if let Some(m) = &img.media {
        if let Some(param) = &m.encrypt_query_param {
            let key = if let Some(hex_key) = &img.aeskey {
                // Image aeskey is hex -> convert to base64 for AES
                let bytes: Vec<u8> = (0..hex_key.len() / 2)
                    .filter_map(|i| u8::from_str_radix(&hex_key[i * 2..i * 2 + 2], 16).ok())
                    .collect();
                base64::engine::general_purpose::STANDARD.encode(&bytes)
            } else {
                m.aes_key.clone().unwrap_or_default()
            };
            return Some(MediaSource::Cdn {
                encrypt_query_param: param.clone(),
                aes_key: key,
            });
        }
    }
    None
}

/// Resolve media source from FileItem.
fn resolve_media_source_file(f: &FileItem) -> Option<MediaSource> {
    if let Some(url) = &f.file_url {
        return Some(MediaSource::Url(url.clone()));
    }
    if let Some(m) = &f.media {
        if let Some(param) = &m.encrypt_query_param {
            return Some(MediaSource::Cdn {
                encrypt_query_param: param.clone(),
                aes_key: m.aes_key.clone().unwrap_or_default(),
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Channel trait
// ---------------------------------------------------------------------------

impl Channel for WeChatPersonalChannel {
    fn name(&self) -> &str {
        "wechat"
    }

    fn send(&self, msg: OutboundMessage) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let chunk_cfg = ChunkConfig {
                max_chars: 4096,
                ..Default::default()
            };
            let chunks = chunk_text(&msg.text, &chunk_cfg);
            for chunk in &chunks {
                self.send_text(&msg.target_id, chunk).await?;
            }

            // Upload and send each image
            for (i, image_data_uri) in msg.images.iter().enumerate() {
                match self.send_outbound_image(&msg.target_id, image_data_uri).await {
                    Ok(()) => info!(index = i, "wechat: image sent"),
                    Err(e) => warn!(index = i, "wechat: image send failed: {e:#}"),
                }
            }

            // Send file attachments
            for (idx, (filename, _mime, path_or_url)) in msg.files.iter().enumerate() {
                let bytes = if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
                    match self.client.get(path_or_url.as_str()).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            match resp.bytes().await {
                                Ok(b) if !b.is_empty() => b.to_vec(),
                                _ => { warn!(index = idx, "wechat: empty file download"); continue; }
                            }
                        }
                        _ => { warn!(index = idx, "wechat: file download failed: {path_or_url}"); continue; }
                    }
                } else {
                    match std::fs::read(path_or_url) {
                        Ok(b) => b,
                        Err(e) => { warn!(index = idx, "wechat: failed to read file {path_or_url}: {e}"); continue; }
                    }
                };

                match self.upload_media(&bytes, &msg.target_id, UploadMediaType::File).await {
                    Ok(uploaded) => {
                        if let Err(e) = self.send_file_message(&msg.target_id, filename, &uploaded).await {
                            warn!(index = idx, "wechat: send file message failed: {e:#}");
                        } else {
                            info!(index = idx, filename = %filename, "wechat: file sent");
                        }
                    }
                    Err(e) => {
                        warn!(index = idx, "wechat: file upload failed: {e:#}");
                    }
                }
            }

            Ok(())
        })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move { self.poll_loop().await })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Percent-encode a string matching JS `encodeURIComponent` behavior.
///
/// JS encodeURIComponent preserves: A-Z a-z 0-9 - _ . ~ ! ' ( ) *
/// Everything else is percent-encoded as %XX (uppercase hex).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'!'
            | b'\''
            | b'('
            | b')'
            | b'*' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

fn base_info() -> serde_json::Value {
    json!({
        "channel_version": env!("CARGO_PKG_VERSION")
    })
}

// ---------------------------------------------------------------------------
// Tests (require mock_wechat.py running on port 19987)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn init_crypto() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }

    /// Build a WeChatPersonalChannel pointing at the mock server.
    fn mock_channel(received: Arc<Mutex<Vec<String>>>) -> Arc<WeChatPersonalChannel> {
        let rx = Arc::clone(&received);
        let on_message = Arc::new(move |_from: String, text: String, _images: Vec<crate::agent::registry::ImageAttachment>, _files: Vec<crate::agent::registry::FileAttachment>| {
            rx.lock().unwrap().push(text);
        });
        Arc::new(
            WeChatPersonalChannel::new("mock-token".to_owned(), on_message)
                .with_base_url("http://127.0.0.1:19987"),
        )
    }

    /// Verify `with_base_url` overrides the default.
    #[test]
    fn with_base_url_overrides_default() {
        init_crypto();
        let ch = WeChatPersonalChannel::new(
            "tok".to_owned(),
            Arc::new(|_, _, _, _| {}),
        )
        .with_base_url("http://127.0.0.1:19987");
        assert_eq!(ch.base_url, "http://127.0.0.1:19987");
    }

    /// Verify trailing slash is stripped.
    #[test]
    fn with_base_url_strips_trailing_slash() {
        init_crypto();
        let ch = WeChatPersonalChannel::new(
            "tok".to_owned(),
            Arc::new(|_, _, _, _| {}),
        )
        .with_base_url("http://127.0.0.1:19987/");
        assert_eq!(ch.base_url, "http://127.0.0.1:19987");
    }

    // -------------------------------------------------------------------
    // Unit tests (no network required)
    // -------------------------------------------------------------------

    /// percent_encode must match JS encodeURIComponent behavior.
    /// JS encodeURIComponent preserves: A-Z a-z 0-9 - _ . ~ ! ' ( ) *
    #[test]
    fn percent_encode_matches_js_encode_uri_component() {
        // Base64 string with +, =, /
        let input = "abc+def/ghi=jkl==";
        let encoded = percent_encode(input);
        assert_eq!(encoded, "abc%2Bdef%2Fghi%3Djkl%3D%3D");

        // Characters that JS encodeURIComponent preserves (should NOT be encoded)
        let preserved = "ABCxyz019-_.~!'()*";
        assert_eq!(percent_encode(preserved), preserved);

        // Space and special chars must be encoded
        assert_eq!(percent_encode(" "), "%20");
        assert_eq!(percent_encode("@"), "%40");
        assert_eq!(percent_encode("#"), "%23");
        assert_eq!(percent_encode("&"), "%26");

        // Full base64-like param
        let b64 = "dGVzdA==";
        assert_eq!(percent_encode(b64), "dGVzdA%3D%3D");
    }

    /// AES-128-ECB encrypt then decrypt roundtrip must recover original plaintext.
    #[test]
    fn aes_ecb_encrypt_decrypt_roundtrip() {
        use aes::cipher::{BlockDecrypt, KeyInit};

        let key: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
        ];
        let plaintext = b"Hello, WeChat CDN AES-128-ECB test data!";

        // Encrypt (uses PKCS7 padding)
        let ciphertext = WeChatPersonalChannel::aes_ecb_encrypt(plaintext, &key);
        // Ciphertext length should be padded to 16-byte boundary
        assert_eq!(ciphertext.len() % 16, 0);
        assert_ne!(&ciphertext[..], &plaintext[..]);

        // Decrypt (mirror of download_cdn_media logic)
        let cipher = aes::Aes128::new_from_slice(&key).unwrap();
        let mut data = ciphertext.clone();
        for chunk in data.chunks_mut(16) {
            let block = aes::Block::from_mut_slice(chunk);
            cipher.decrypt_block(block);
        }
        // Remove PKCS7 padding
        if let Some(&last) = data.last() {
            let pad_len = last as usize;
            if pad_len > 0 && pad_len <= 16 && data.len() >= pad_len {
                let valid = data[data.len() - pad_len..].iter().all(|&b| b == last);
                if valid {
                    data.truncate(data.len() - pad_len);
                }
            }
        }
        assert_eq!(&data, plaintext);
    }

    /// AES-128-ECB encrypt with known values to verify ciphertext.
    #[test]
    fn aes_ecb_encrypt_known_vector() {
        // 16 bytes of plaintext (no padding needed beyond PKCS7)
        let key: [u8; 16] = [0u8; 16];
        let plaintext = [0u8; 16];
        let ciphertext = WeChatPersonalChannel::aes_ecb_encrypt(&plaintext, &key);
        // With PKCS7, 16 bytes of input gets 16 bytes padding -> 32 bytes ciphertext
        assert_eq!(ciphertext.len(), 32);
    }

    /// aes_ecb_padded_size matches openclaw's Math.ceil((size + 1) / 16) * 16.
    #[test]
    fn aes_ecb_padded_size_matches_openclaw() {
        // openclaw: Math.ceil((plaintextSize + 1) / 16) * 16
        assert_eq!(WeChatPersonalChannel::aes_ecb_padded_size(0), 16);
        assert_eq!(WeChatPersonalChannel::aes_ecb_padded_size(1), 16);
        assert_eq!(WeChatPersonalChannel::aes_ecb_padded_size(15), 16);
        assert_eq!(WeChatPersonalChannel::aes_ecb_padded_size(16), 32);
        assert_eq!(WeChatPersonalChannel::aes_ecb_padded_size(31), 32);
        assert_eq!(WeChatPersonalChannel::aes_ecb_padded_size(32), 48);
        assert_eq!(WeChatPersonalChannel::aes_ecb_padded_size(100), 112);
    }

    /// AES key parsing: hex string (32 chars) -> 16 raw bytes.
    /// openclaw: parseAesKey decodes base64 -> if 32 ASCII hex chars -> parse hex -> 16 bytes.
    /// rsclaw: download_cdn_media does the same.
    #[test]
    fn aes_key_hex_to_bytes() {
        let hex_key = "00112233445566778899aabbccddeeff";
        assert_eq!(hex_key.len(), 32);

        // Parse hex -> raw bytes (same as rsclaw's download_cdn_media logic)
        let key_bytes: Vec<u8> = (0..16)
            .map(|i| {
                let hex = &hex_key[i * 2..i * 2 + 2];
                u8::from_str_radix(hex, 16).unwrap()
            })
            .collect();

        assert_eq!(key_bytes.len(), 16);
        assert_eq!(key_bytes[0], 0x00);
        assert_eq!(key_bytes[1], 0x11);
        assert_eq!(key_bytes[15], 0xFF);
    }

    /// Image aeskey conversion: hex -> base64(raw bytes).
    /// openclaw: Buffer.from(img.aeskey, "hex").toString("base64")
    /// rsclaw: resolve_media_source_image does the same.
    #[test]
    fn aes_key_hex_to_base64_conversion() {
        let hex_key = "00112233445566778899aabbccddeeff";
        // rsclaw conversion (same as resolve_media_source_image)
        let bytes: Vec<u8> = (0..hex_key.len() / 2)
            .filter_map(|i| u8::from_str_radix(&hex_key[i * 2..i * 2 + 2], 16).ok())
            .collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

        // Verify: base64-decode should give 16 raw bytes
        let decoded = base64::engine::general_purpose::STANDARD.decode(&b64).unwrap();
        assert_eq!(decoded.len(), 16);
        assert_eq!(decoded, bytes);

        // And those bytes should match hex parsing
        assert_eq!(hex::encode(&decoded), hex_key);
    }

    /// base64(hex string) -> base64(raw bytes) key conversion.
    /// openclaw parseAesKey: decode base64 -> if 32 hex chars -> parse to 16 bytes.
    #[test]
    fn aes_key_base64_hex_roundtrip() {
        let hex_key = "aabbccdd11223344aabbccdd11223344";
        // base64 encode the hex string (as openclaw voice/file aes_key format)
        let b64_of_hex = base64::engine::general_purpose::STANDARD
            .encode(hex_key.as_bytes());

        // Decode base64 -> get 32-byte hex string
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&b64_of_hex)
            .unwrap();
        assert_eq!(decoded.len(), 32);

        // It's ASCII hex -> parse to 16 raw bytes (rsclaw download_cdn_media logic)
        let ascii = std::str::from_utf8(&decoded).unwrap();
        assert!(ascii.chars().all(|c| c.is_ascii_hexdigit()));
        let key_bytes: Vec<u8> = (0..16)
            .map(|i| u8::from_str_radix(&ascii[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        assert_eq!(key_bytes.len(), 16);
        assert_eq!(hex::encode(&key_bytes), hex_key);
    }

    /// resolve_media_source_image: prefers image_url, then media with aeskey conversion.
    #[test]
    fn resolve_media_source_image_prefers_url() {
        let img = ImageItem {
            image_url: Some("https://example.com/img.jpg".to_string()),
            aeskey: None,
            media: Some(MediaRef {
                encrypt_query_param: Some("param123".to_string()),
                aes_key: Some("key123".to_string()),
            }),
        };
        match resolve_media_source_image(&img) {
            Some(MediaSource::Url(url)) => assert_eq!(url, "https://example.com/img.jpg"),
            other => panic!("expected Url, got {:?}", other),
        }
    }

    /// resolve_media_source_image: with media.encrypt_query_param + aeskey hex conversion.
    #[test]
    fn resolve_media_source_image_cdn_with_hex_aeskey() {
        let hex_key = "aabbccdd11223344aabbccdd11223344";
        let img = ImageItem {
            image_url: None,
            aeskey: Some(hex_key.to_string()),
            media: Some(MediaRef {
                encrypt_query_param: Some("encrypted_param_value".to_string()),
                aes_key: Some("media_aes_key_ignored".to_string()),
            }),
        };
        match resolve_media_source_image(&img) {
            Some(MediaSource::Cdn { encrypt_query_param, aes_key }) => {
                assert_eq!(encrypt_query_param, "encrypted_param_value");
                // aes_key should be base64 of the raw bytes parsed from hex
                let decoded = base64::engine::general_purpose::STANDARD.decode(&aes_key).unwrap();
                assert_eq!(decoded.len(), 16);
                assert_eq!(hex::encode(&decoded), hex_key);
            }
            other => panic!("expected Cdn, got {:?}", other),
        }
    }

    /// resolve_media_source_image: with media only (no aeskey), uses media.aes_key directly.
    #[test]
    fn resolve_media_source_image_cdn_media_aes_key() {
        let img = ImageItem {
            image_url: None,
            aeskey: None,
            media: Some(MediaRef {
                encrypt_query_param: Some("param_xyz".to_string()),
                aes_key: Some("base64_media_key".to_string()),
            }),
        };
        match resolve_media_source_image(&img) {
            Some(MediaSource::Cdn { encrypt_query_param, aes_key }) => {
                assert_eq!(encrypt_query_param, "param_xyz");
                assert_eq!(aes_key, "base64_media_key");
            }
            other => panic!("expected Cdn, got {:?}", other),
        }
    }

    /// resolve_media_source_image: no image_url, no media -> None.
    #[test]
    fn resolve_media_source_image_returns_none() {
        let img = ImageItem {
            image_url: None,
            aeskey: None,
            media: None,
        };
        assert!(resolve_media_source_image(&img).is_none());
    }

    /// GetUploadUrlResponse deserialization with upload_param.
    #[test]
    fn get_upload_url_response_with_upload_param() {
        let json = r#"{"upload_param": "abc123", "thumb_upload_param": "thumb456"}"#;
        let resp: GetUploadUrlResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.upload_param.as_deref(), Some("abc123"));
        assert_eq!(resp.thumb_upload_param.as_deref(), Some("thumb456"));
        assert!(resp.upload_full_url.is_none());
    }

    /// GetUploadUrlResponse deserialization with upload_full_url.
    #[test]
    fn get_upload_url_response_with_upload_full_url() {
        let json = r#"{"upload_full_url": "https://cdn.example.com/upload?encrypted_query_param=xyz%3D%3D&filekey=abc"}"#;
        let resp: GetUploadUrlResponse = serde_json::from_str(json).unwrap();
        assert!(resp.upload_param.is_none());
        assert_eq!(
            resp.upload_full_url.as_deref(),
            Some("https://cdn.example.com/upload?encrypted_query_param=xyz%3D%3D&filekey=abc")
        );
    }

    /// CDN download URL construction matches openclaw cdn-url.ts buildCdnDownloadUrl.
    #[test]
    fn cdn_download_url_construction() {
        let param = "some+param/with=chars";
        let url = format!(
            "{}/download?encrypted_query_param={}",
            WECHAT_CDN_BASE,
            percent_encode(param),
        );
        assert_eq!(
            url,
            "https://novac2c.cdn.weixin.qq.com/c2c/download?encrypted_query_param=some%2Bparam%2Fwith%3Dchars"
        );
    }

    /// CDN upload URL construction matches openclaw cdn-url.ts buildCdnUploadUrl.
    #[test]
    fn cdn_upload_url_construction() {
        let upload_param = "enc+param==";
        let filekey = "abc123def456";
        let url = format!(
            "{}/upload?encrypted_query_param={}&filekey={}",
            WECHAT_CDN_BASE,
            percent_encode(upload_param),
            percent_encode(filekey),
        );
        assert_eq!(
            url,
            "https://novac2c.cdn.weixin.qq.com/c2c/upload?encrypted_query_param=enc%2Bparam%3D%3D&filekey=abc123def456"
        );
    }

    /// Extract encrypted_query_param from upload_full_url for CDN URL construction.
    #[test]
    fn cdn_url_from_upload_full_url_extract_param() {
        let full_url = "https://cdn.example.com/upload?encrypted_query_param=xyz%3D%3D&extra=1";
        // When upload_full_url is provided, it is used directly (with filekey appended if missing)
        let filekey = "myfilekey";
        let url = if full_url.contains("filekey=") {
            full_url.to_string()
        } else {
            format!("{}&filekey={}", full_url, percent_encode(filekey))
        };
        assert_eq!(url, "https://cdn.example.com/upload?encrypted_query_param=xyz%3D%3D&extra=1&filekey=myfilekey");

        // When filekey is already present
        let full_url2 = "https://cdn.example.com/upload?encrypted_query_param=abc&filekey=existing";
        let url2 = if full_url2.contains("filekey=") {
            full_url2.to_string()
        } else {
            format!("{}&filekey={}", full_url2, percent_encode(filekey))
        };
        assert_eq!(url2, "https://cdn.example.com/upload?encrypted_query_param=abc&filekey=existing");
    }

    // -------------------------------------------------------------------
    // Integration tests (require mock_wechat.py on port 19987)
    // -------------------------------------------------------------------

    /// Call getupdates against mock_wechat.py and expect 2 messages on first call.
    ///
    /// Run: python3 /tmp/mock_wechat.py  (in a separate terminal)
    /// Then: cargo test -p rsclaw wechat::tests::mock_getupdates -- --ignored
    #[tokio::test]
    #[ignore]
    async fn mock_getupdates_returns_messages() {
        let received = Arc::new(Mutex::new(Vec::<String>::new()));
        let ch = mock_channel(Arc::clone(&received));

        let resp = ch.get_updates("").await.expect("getupdates failed");
        let msgs = resp.msgs.unwrap_or_default();
        assert_eq!(msgs.len(), 2, "expected 2 mock messages on first call");

        // First message should be a text item
        let first = &msgs[0];
        let items = first.item_list.as_deref().unwrap_or(&[]);
        let text = items
            .iter()
            .find_map(|i| i.text_item.as_ref().and_then(|t| t.text.as_deref()))
            .expect("first message should have text_item");
        assert_eq!(text, "Hello from mock WeChat");

        // Second message should be a voice item
        let second = &msgs[1];
        let items2 = second.item_list.as_deref().unwrap_or(&[]);
        let voice = items2
            .iter()
            .find_map(|i| i.voice_item.as_ref())
            .expect("second message should have voice_item");
        assert_eq!(voice.text.as_deref(), Some("Transcribed voice text"));
    }

    /// Call getupdates twice; second call should return empty msgs.
    ///
    /// Run: python3 /tmp/mock_wechat.py  (restart before test)
    /// Then: cargo test -p rsclaw wechat::tests::mock_getupdates_empty_second -- --ignored
    #[tokio::test]
    #[ignore]
    async fn mock_getupdates_empty_second_call() {
        let received = Arc::new(Mutex::new(Vec::<String>::new()));
        let ch = mock_channel(Arc::clone(&received));

        let first = ch.get_updates("").await.expect("first call failed");
        let buf = first.get_updates_buf.unwrap_or_default();

        let second = ch.get_updates(&buf).await.expect("second call failed");
        let msgs = second.msgs.unwrap_or_default();
        assert!(msgs.is_empty(), "second call should return no messages");
    }

    /// Send a text reply to the mock server.
    ///
    /// Run: python3 /tmp/mock_wechat.py
    /// Then: cargo test -p rsclaw wechat::tests::mock_send_text -- --ignored
    #[tokio::test]
    #[ignore]
    async fn mock_send_text() {
        let ch = WeChatPersonalChannel::new(
            "mock-token".to_owned(),
            Arc::new(|_, _, _, _| {}),
        )
        .with_base_url("http://127.0.0.1:19987");

        ch.send_text("user_abc", "Hello from rsclaw test")
            .await
            .expect("send_text failed");
    }
}
