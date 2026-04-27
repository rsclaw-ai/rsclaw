//! Feishu (飞书/Lark) Bot channel driver.
//!
//! Implements a WebSocket-based event loop using the Feishu Open API:
//!   - Tenant access token management with automatic refresh.
//!   - WebSocket connection via `/callback/ws/endpoint` (like official SDK).
//!   - Send/receive text messages via `im/v1/messages`.
//!   - Voice message download and transcription via shared Whisper module.
//!   - Text chunking (4000-char limit).
//!   - Auto-reconnect on disconnect.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt, future::BoxFuture};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tokio::{sync::RwLock, time::sleep};
use tracing::{debug, info, warn};

use super::{Channel, OutboundMessage};
use crate::channel::{
    chunker::{ChunkConfig, chunk_text, platform_chunk_limit},
    transcription::transcribe_audio,
};

// ---------------------------------------------------------------------------
// Feishu API base URL
// ---------------------------------------------------------------------------

const FEISHU_API_BASE: &str = "https://open.feishu.cn/open-apis";
const LARK_API_BASE: &str = "https://open.larksuite.com/open-apis";
const LARK_DOMAIN: &str = "https://open.larksuite.com";
const FEISHU_DOMAIN: &str = "https://open.feishu.cn";

/// Token refresh margin (seconds before expiry to trigger refresh).
const TOKEN_REFRESH_MARGIN: u64 = 300;

// ---------------------------------------------------------------------------
// Feishu API response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FeishuTokenResponse {
    code: i32,
    msg: String,
    tenant_access_token: Option<String>,
    expire: Option<u64>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct FeishuApiResponse<T> {
    code: i32,
    msg: String,
    data: Option<T>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct MessageListData {
    items: Option<Vec<FeishuMessage>>,
    has_more: Option<bool>,
    page_token: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct FeishuMessage {
    message_id: String,
    #[serde(default)]
    msg_type: String,
    #[serde(default)]
    body: Option<MessageBody>,
    #[serde(default)]
    sender: Option<MessageSender>,
    chat_id: Option<String>,
    #[serde(default)]
    create_time: String,
}

#[derive(Debug, Deserialize)]
struct MessageBody {
    content: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct MessageSender {
    sender_id: Option<SenderIdInfo>,
    sender_type: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct SenderIdInfo {
    open_id: Option<String>,
    user_id: Option<String>,
    union_id: Option<String>,
}

/// Parsed text content from Feishu message body JSON.
#[derive(Debug, Deserialize)]
struct TextContent {
    text: Option<String>,
}

/// Parsed file content from Feishu voice/audio message body JSON.
#[derive(Debug, Deserialize)]
struct FileContent {
    file_key: Option<String>,
    #[allow(dead_code)]
    duration: Option<i64>,
}

// ---------------------------------------------------------------------------
// Token cache
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct TokenCache {
    token: String,
    expires_at: Instant,
}

// ---------------------------------------------------------------------------
// FeishuChannel
// ---------------------------------------------------------------------------

pub struct FeishuChannel {
    app_id: String,
    app_secret: String,
    /// "feishu" (China) or "lark" (international).
    pub brand: String,
    /// Chat IDs (retained for potential REST fallback; not used by WS mode).
    #[allow(dead_code)]
    chat_ids: Vec<String>,
    client: Client,
    token_cache: RwLock<Option<TokenCache>>,
    /// Event dedup: recently processed event IDs (prevents duplicate processing
    /// on retry).
    seen_events: RwLock<std::collections::HashSet<String>>,
    /// REST API base URL override (for testing).
    pub api_base_override: Option<String>,
    /// WS endpoint request domain override (for testing).
    pub ws_url_override: Option<String>,
    /// Max file size for downloads (from config tools.upload.maxFileSize).
    pub max_file_size: usize,
    /// Callback: (sender_open_id, text, chat_id, is_group, images, files).
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

/// Build feishu message content: use interactive card with markdown for rich
/// text, fall back to plain text for short simple messages.
/// Convert markdown text to Feishu post (rich text) format.
/// Supports: bold(**), code(`), links, paragraphs.
#[allow(dead_code)]
fn markdown_to_feishu_post(text: &str) -> serde_json::Value {
    let mut content: Vec<Vec<serde_json::Value>> = Vec::new();

    for line in text.split('\n') {
        let mut elements: Vec<serde_json::Value> = Vec::new();
        let trimmed = line;

        if trimmed.is_empty() {
            content.push(vec![json!({"tag": "text", "text": "\n"})]);
            continue;
        }

        // Check for code block markers
        if trimmed.starts_with("```") {
            // Just skip code block delimiters, content lines come through as text
            continue;
        }

        // Parse inline elements (bold, code, links)
        let mut chars = trimmed.char_indices().peekable();
        let mut buf = String::new();

        while let Some(&(i, ch)) = chars.peek() {
            if ch == '*' && trimmed[i..].starts_with("**") {
                // Flush buffer
                if !buf.is_empty() {
                    elements.push(json!({"tag": "text", "text": buf.clone()}));
                    buf.clear();
                }
                // Skip **
                chars.next();
                chars.next();
                let mut bold = String::new();
                while let Some(&(_, c)) = chars.peek() {
                    if c == '*' && chars.clone().nth(1).map(|(_, c2)| c2) == Some('*') {
                        chars.next();
                        chars.next();
                        break;
                    }
                    bold.push(c);
                    chars.next();
                }
                elements.push(json!({"tag": "text", "text": bold, "style": ["bold"]}));
            } else if ch == '`' && !trimmed[i..].starts_with("```") {
                if !buf.is_empty() {
                    elements.push(json!({"tag": "text", "text": buf.clone()}));
                    buf.clear();
                }
                chars.next();
                let mut code = String::new();
                while let Some(&(_, c)) = chars.peek() {
                    if c == '`' {
                        chars.next();
                        break;
                    }
                    code.push(c);
                    chars.next();
                }
                elements.push(json!({"tag": "text", "text": code, "style": ["bold"]}));
            } else if ch == '[' {
                // Try to parse [text](url)
                let rest = &trimmed[i..];
                if let Some(close_bracket) = rest.find("](") {
                    if let Some(close_paren) = rest[close_bracket + 2..].find(')') {
                        if !buf.is_empty() {
                            elements.push(json!({"tag": "text", "text": buf.clone()}));
                            buf.clear();
                        }
                        let link_text = &rest[1..close_bracket];
                        let link_url = &rest[close_bracket + 2..close_bracket + 2 + close_paren];
                        elements.push(json!({"tag": "a", "text": link_text, "href": link_url}));
                        // Skip past the entire [text](url)
                        let skip = close_bracket + 2 + close_paren + 1;
                        for _ in 0..skip {
                            chars.next();
                        }
                        continue;
                    }
                }
                buf.push(ch);
                chars.next();
            } else {
                buf.push(ch);
                chars.next();
            }
        }

        if !buf.is_empty() {
            elements.push(json!({"tag": "text", "text": buf}));
        }

        if elements.is_empty() {
            elements.push(json!({"tag": "text", "text": trimmed}));
        }

        content.push(elements);
    }

    json!({
        "zh_cn": {
            "content": content
        }
    })
}

/// Build feishu message payload. Returns (msg_type, content_or_card_json).
/// For interactive cards, the second value is the raw card JSON (not
/// stringified).
fn build_feishu_card(text: &str, brand: &str) -> serde_json::Value {
    let cleaned = text;

    let title = if brand == "lark" {
        "\u{1F980}rsclaw.ai | RsClaw AI Agent Engine"
    } else {
        "\u{1F980}rsclaw.ai | \u{8783}\u{87F9}AI\u{667A}\u{80FD}\u{4F53}\u{5F15}\u{64CE}"
    };
    json!({
        "msg_type": "interactive",
        "card": {
            "schema": "2.0",
            "header": {
                "title": {
                    "content": title,
                    "tag": "plain_text"
                },
                "template": "blue"
            },
            "body": {
                "elements": [
                    {
                        "tag": "markdown",
                        "content": cleaned.trim()
                    }
                ]
            }
        }
    })
}

#[allow(dead_code)]
impl FeishuChannel {
    fn api_base(&self) -> &str {
        if let Some(ref ov) = self.api_base_override {
            return ov.as_str();
        }
        if self.brand == "lark" {
            LARK_API_BASE
        } else {
            FEISHU_API_BASE
        }
    }
    fn ws_domain(&self) -> &str {
        if let Some(ref ov) = self.ws_url_override {
            return ov.as_str();
        }
        if self.brand == "lark" {
            LARK_DOMAIN
        } else {
            FEISHU_DOMAIN
        }
    }

    #[allow(clippy::type_complexity)]
    pub fn new(
        app_id: impl Into<String>,
        app_secret: impl Into<String>,
        chat_ids: Vec<String>,
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
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            brand: "feishu".to_owned(),
            chat_ids,
            client: crate::config::build_proxy_client()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            token_cache: RwLock::new(None),
            seen_events: RwLock::new(std::collections::HashSet::new()),
            api_base_override: None,
            ws_url_override: None,
            max_file_size: 128_000_000, // default 128MB, overridden by startup
            on_message,
        }
    }

    // -----------------------------------------------------------------------
    // Token management
    // -----------------------------------------------------------------------

    /// Obtain a valid tenant access token, refreshing if needed.
    async fn get_token(&self) -> Result<String> {
        // Fast path: cached token still valid.
        {
            let cache = self.token_cache.read().await;
            if let Some(ref tc) = *cache
                && Instant::now() < tc.expires_at
            {
                return Ok(tc.token.clone());
            }
        }

        // Slow path: refresh.
        self.refresh_token().await
    }

    /// Request a new tenant access token from Feishu.
    async fn refresh_token(&self) -> Result<String> {
        let url = format!("{}/auth/v3/tenant_access_token/internal", self.api_base());

        let resp = self
            .client
            .post(&url)
            .json(&json!({
                "app_id": self.app_id,
                "app_secret": self.app_secret,
            }))
            .send()
            .await
            .context("feishu: request tenant_access_token")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("feishu: token request failed {status}: {body}");
        }

        let token_resp: FeishuTokenResponse =
            resp.json().await.context("feishu: parse token response")?;

        if token_resp.code != 0 {
            anyhow::bail!(
                "feishu: token error code={}: {}",
                token_resp.code,
                token_resp.msg
            );
        }

        let token = token_resp
            .tenant_access_token
            .context("feishu: missing tenant_access_token in response")?;
        let expire_secs = token_resp.expire.unwrap_or(7200);

        let expires_at =
            Instant::now() + Duration::from_secs(expire_secs.saturating_sub(TOKEN_REFRESH_MARGIN));

        debug!(expire_secs, "feishu: tenant token refreshed");

        let mut cache = self.token_cache.write().await;
        *cache = Some(TokenCache {
            token: token.clone(),
            expires_at,
        });

        Ok(token)
    }

    // -----------------------------------------------------------------------
    // Send message
    // -----------------------------------------------------------------------

    /// Send a single text chunk to a target as a card with markdown.
    async fn send_text_chunk(&self, target_id: &str, text: &str) -> Result<()> {
        let token = self.get_token().await?;
        let id_type = if target_id.starts_with("ou_") { "open_id" }
            else if target_id.starts_with("on_") { "union_id" }
            else if target_id.starts_with("oc_") { "chat_id" }
            else { "chat_id" };
        let url = format!("{}/im/v1/messages?receive_id_type={id_type}", self.api_base());

        let card_payload = build_feishu_card(text, &self.brand);
        let card_str =
            serde_json::to_string(&card_payload["card"]).context("feishu: serialize card")?;

        let body = json!({
            "receive_id": target_id,
            "msg_type": "interactive",
            "content": card_str,
        });

        info!(target_id, text_preview = %text.chars().take(100).collect::<String>(), "feishu: send_text_chunk sending");

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .context("feishu: send message")?;

        let status = resp.status();
        info!(target_id, status = %status.as_u16(), "feishu: send_text_chunk response");
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("feishu: send_message failed {status}: {body}");
        }

        let api_resp: FeishuApiResponse<serde_json::Value> =
            resp.json().await.context("feishu: parse send response")?;

        if api_resp.code != 0 {
            anyhow::bail!(
                "feishu: send_message error code={}: {}",
                api_resp.code,
                api_resp.msg
            );
        }

        Ok(())
    }

    /// Reply to a specific message by message_id.
    async fn reply_text_chunk(&self, message_id: &str, text: &str) -> Result<()> {
        let token = self.get_token().await?;
        let url = format!("{}/im/v1/messages/{message_id}/reply", self.api_base(),);

        let card_payload = build_feishu_card(text, &self.brand);
        let card_str =
            serde_json::to_string(&card_payload["card"]).context("feishu: serialize card")?;

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&token)
            .json(&json!({
                "msg_type": "interactive",
                "content": card_str,
            }))
            .send()
            .await
            .context("feishu: reply message")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("feishu: reply failed {status}: {body}");
        }

        let api_resp: FeishuApiResponse<serde_json::Value> =
            resp.json().await.context("feishu: parse reply response")?;

        if api_resp.code != 0 {
            anyhow::bail!(
                "feishu: reply error code={}: {}",
                api_resp.code,
                api_resp.msg
            );
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // WebSocket connection loop
    // -----------------------------------------------------------------------

    /// Obtain WS endpoint URL, connect, and process events until disconnect.
    async fn ws_connect_loop(&self) -> Result<()> {
        // 1. Get WS endpoint URL via Feishu callback API
        let resp = self
            .client
            .post(format!("{}/callback/ws/endpoint", self.ws_domain()))
            .json(&json!({
                "AppID": self.app_id,
                "AppSecret": self.app_secret,
            }))
            .send()
            .await
            .context("feishu: WS endpoint request failed")?;

        let body: serde_json::Value = resp
            .json()
            .await
            .context("feishu: parse WS endpoint response")?;

        let code = body.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            anyhow::bail!(
                "feishu: WS endpoint error code={}: {}",
                code,
                body.get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
            );
        }

        let ws_url = body
            .pointer("/data/URL")
            .and_then(|v| v.as_str())
            .context("feishu: no WS URL in endpoint response")?;

        info!(url = %ws_url, "feishu: connecting to WebSocket");

        // 2. Connect WebSocket
        let (ws_stream, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .context("feishu: WS connect failed")?;

        let (mut write, mut read) = ws_stream.split();

        info!("feishu: WebSocket connected");

        // 3. Read events with idle timeout (detect half-open connections).
        // Feishu sends pings every ~30s; if we hear nothing for 90s, reconnect.
        const WS_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
        loop {
            let msg = match tokio::time::timeout(WS_IDLE_TIMEOUT, read.next()).await {
                Ok(Some(msg)) => msg,
                Ok(None) => { info!("feishu: WS stream ended"); break; }
                Err(_) => { warn!("feishu: WS idle timeout ({}s), reconnecting", WS_IDLE_TIMEOUT.as_secs()); break; }
            };
            match msg {
                Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                    info!(
                        len = text.len(),
                        "feishu: WS frame received: {}",
                        &text[..text.len().min(300)]
                    );
                    self.handle_ws_event(&text).await;
                }
                Ok(tokio_tungstenite::tungstenite::Message::Binary(data)) => {
                    // Decode protobuf frame (pbbp2 format)
                    use prost::Message as ProstMessage;
                    match lark_websocket_protobuf::pbbp2::Frame::decode(&data[..]) {
                        Ok(frame) => {
                            // method=0 is CONTROL (ping), method=1 is DATA
                            if frame.method == 1
                                && let Some(payload) = frame.payload
                                && let Ok(text) = String::from_utf8(payload.clone())
                            {
                                info!(len = text.len(), "feishu: WS event received");
                                self.handle_ws_event(&text).await;
                            }
                        }
                        Err(e) => {
                            // Fallback: try as UTF-8 text
                            if let Ok(text) = String::from_utf8(data.to_vec()) {
                                self.handle_ws_event(&text).await;
                            } else {
                                debug!(len = data.len(), error = %e, "feishu: WS binary decode failed");
                            }
                        }
                    }
                }
                Ok(tokio_tungstenite::tungstenite::Message::Ping(data)) => {
                    info!("feishu: WS ping received");

                    let _ = write
                        .send(tokio_tungstenite::tungstenite::Message::Pong(data))
                        .await;
                }
                Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => {
                    info!("feishu: WS closed by server");
                    break;
                }
                Err(e) => {
                    let err_str = format!("{e:#}");
                    if err_str.contains("UTF-8") || err_str.contains("utf-8") {
                        warn!("feishu: WS frame UTF-8 error (skipping): {e:#}");
                        continue;
                    }
                    warn!("feishu: WS read error: {e:#}");
                    break;
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Parse and dispatch a single WebSocket frame from Feishu.
    ///
    /// Feishu WS frames may have several forms:
    ///   - `{"type":"pong"}` -- heartbeat response, ignored.
    ///   - `{"type":"event","data":"{...}"}` -- event with JSON-string data.
    ///   - `{"header":{"type":"event",...},"data":"<base64>"}` --
    ///     base64-encoded event payload (possibly chunked via sum/seq).
    ///   - Raw event JSON with `header.event_type` at the top level.
    async fn handle_ws_event(&self, raw: &str) {
        let val: serde_json::Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => return,
        };

        // Check frame-level type (top-level "type" or "header.type")
        let frame_type = val
            .get("type")
            .and_then(|v| v.as_str())
            .or_else(|| val.pointer("/header/type").and_then(|v| v.as_str()))
            .unwrap_or("");

        if frame_type == "pong" {
            return; // heartbeat response, ignore
        }

        // Extract event data from the "data" field
        let event_data = if let Some(data_str) = val.get("data").and_then(|v| v.as_str()) {
            // Try parsing as JSON first
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(data_str) {
                parsed
            } else {
                // Try base64 decode
                match base64_decode_json(data_str) {
                    Some(decoded) => decoded,
                    None => {
                        debug!("feishu: WS data field is neither JSON nor valid base64");
                        return;
                    }
                }
            }
        } else if val.get("data").is_some() {
            // "data" is an object, not a string
            val.get("data").cloned().unwrap_or_default()
        } else {
            // No "data" field -- might be a raw event (header + event at top level)
            val.clone()
        };

        // Dispatch through the existing webhook handler
        let event_str = serde_json::to_string(&event_data).unwrap_or_default();
        if let Err(e) = self.handle_webhook_event(&event_str).await {
            warn!("feishu: WS event handling error: {e:#}");
        }
    }

    // -----------------------------------------------------------------------
    // Webhook handler (for event subscription -- supports private chat)
    // -----------------------------------------------------------------------

    /// Handle an incoming webhook event from Feishu.
    /// Returns the response body to send back (for challenge verification).
    pub async fn handle_webhook_event(&self, body: &str) -> Result<Option<String>> {
        let val: serde_json::Value =
            serde_json::from_str(body).context("feishu: invalid webhook JSON")?;

        // Debug: log raw event for troubleshooting
        let raw_preview = body.chars().take(500).collect::<String>();
        debug!(raw = %raw_preview, "feishu: raw webhook event");

        // 1. URL verification challenge
        if let Some(challenge) = val.get("challenge").and_then(|v| v.as_str()) {
            info!("feishu: webhook verification challenge");
            return Ok(Some(
                serde_json::json!({"challenge": challenge}).to_string(),
            ));
        }

        // 2. Event dedup — Feishu retries unacknowledged events.
        if let Some(event_id) = val.pointer("/header/event_id").and_then(|v| v.as_str()) {
            let mut seen = self.seen_events.write().await;
            if seen.contains(event_id) {
                debug!(event_id, "feishu: duplicate event, skipping");
                return Ok(None);
            }
            seen.insert(event_id.to_owned());
            // Cap the set size to prevent unbounded growth
            if seen.len() > 1000 {
                seen.clear();
            }
        }

        // 3. Event callback
        let event_type = val
            .pointer("/header/event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if event_type != "im.message.receive_v1" {
            debug!(event_type, "feishu: ignoring non-message event");
            return Ok(None);
        }

        // Extract message fields
        let event = val.get("event").context("feishu: missing event field")?;
        let message = event
            .get("message")
            .context("feishu: missing message field")?;

        // Dedup by message_id (second line of defense after event_id dedup)
        if let Some(msg_id) = message.get("message_id").and_then(|v| v.as_str()) {
            let mut seen = self.seen_events.write().await;
            if seen.contains(msg_id) {
                debug!(msg_id, "feishu: duplicate message_id, skipping");
                return Ok(None);
            }
            seen.insert(msg_id.to_owned());
            if seen.len() > 2000 {
                seen.clear();
            }
        }

        // Skip stale messages (older than 5 minutes) to prevent replay storms.
        // Large file uploads can take minutes before the event arrives.
        if let Some(create_time) = message.get("create_time").and_then(|v| v.as_str()) {
            if let Ok(ts_ms) = create_time.parse::<u64>() {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                if now_ms > ts_ms && (now_ms - ts_ms) > 300_000 {
                    debug!(
                        create_time,
                        age_ms = now_ms - ts_ms,
                        "feishu: skipping stale message"
                    );
                    return Ok(None);
                }
            }
        }

        let msg_type = message
            .get("message_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let chat_id = message
            .get("chat_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let chat_type = message
            .get("chat_type")
            .and_then(|v| v.as_str())
            .unwrap_or("p2p"); // p2p = private, group = group

        let sender_id = event
            .pointer("/sender/sender_id/open_id")
            .or_else(|| event.pointer("/sender/sender_id/user_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        // Skip bot messages
        let sender_type = event
            .pointer("/sender/sender_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if sender_type == "app" {
            return Ok(None);
        }

        // Extract text content (text or voice/audio transcription), images, and files
        let mut images: Vec<crate::agent::registry::ImageAttachment> = Vec::new();
        let mut file_attachments: Vec<crate::agent::registry::FileAttachment> = Vec::new();
        let text = match msg_type {
            "text" => {
                let content_str = message
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                let content: serde_json::Value =
                    serde_json::from_str(content_str).unwrap_or_default();
                content
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned()
            }
            "audio" => {
                let message_id = message
                    .get("message_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let content_str = message
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                let content: serde_json::Value =
                    serde_json::from_str(content_str).unwrap_or_default();
                let file_key = content
                    .get("file_key")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if message_id.is_empty() || file_key.is_empty() {
                    warn!("feishu: audio message missing message_id or file_key");
                    return Ok(None);
                }
                match self.transcribe_voice(message_id, file_key).await {
                    Ok(t) => {
                        info!(chars = t.len(), "feishu: voice transcribed");
                        t
                    }
                    Err(e) => {
                        warn!("feishu: voice transcription failed: {e:#}");
                        return Ok(None);
                    }
                }
            }
            "image" => {
                let message_id = message
                    .get("message_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let content_str = message
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                let content: serde_json::Value =
                    serde_json::from_str(content_str).unwrap_or_default();
                let image_key = content
                    .get("image_key")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !message_id.is_empty() && !image_key.is_empty() {
                    match self.download_image(message_id, image_key).await {
                        Ok(bytes) => {
                            use base64::Engine;
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                            let data_url = format!("data:image/png;base64,{b64}");
                            images.push(crate::agent::registry::ImageAttachment {
                                data: data_url,
                                mime_type: "image/png".to_string(),
                            });
                            info!(size = bytes.len(), "feishu: image downloaded for vision");
                        }
                        Err(e) => {
                            warn!("feishu: image download failed: {e:#}");
                            return Ok(None);
                        }
                    }
                }
                // Image with no text — empty (runtime handles save notification).
                String::new()
            }
            "media" => {
                // Video: download and transcribe audio track
                let message_id = message
                    .get("message_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let content_str = message
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                let content: serde_json::Value =
                    serde_json::from_str(content_str).unwrap_or_default();
                let file_key = content
                    .get("file_key")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if message_id.is_empty() || file_key.is_empty() {
                    return Ok(None);
                }
                match self
                    .download_resource(message_id, file_key, self.max_file_size)
                    .await
                {
                    Ok(bytes) => {
                        // Send as FileAttachment — runtime decides vision vs transcription
                        info!(size = bytes.len(), "feishu: video downloaded");
                        file_attachments.push(crate::agent::registry::FileAttachment {
                            filename: "video.mp4".to_owned(),
                            data: bytes,
                            mime_type: "video/mp4".to_owned(),
                        });
                        String::new()
                    }
                    Err(e) => {
                        warn!("feishu: video download failed: {e:#}");
                        "[video message]".to_owned()
                    }
                }
            }
            "file" => {
                // File attachment: download raw bytes and pass through FileAttachment
                let message_id = message
                    .get("message_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let content_str = message
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                let content: serde_json::Value =
                    serde_json::from_str(content_str).unwrap_or_default();
                let file_key = content
                    .get("file_key")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let file_name = content
                    .get("file_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("file");
                if message_id.is_empty() || file_key.is_empty() {
                    return Ok(None);
                }
                match self
                    .download_resource(message_id, file_key, self.max_file_size)
                    .await
                {
                    Ok(bytes) => {
                        file_attachments.push(crate::agent::registry::FileAttachment {
                            filename: file_name.to_owned(),
                            data: bytes,
                            mime_type: "application/octet-stream".to_owned(),
                        });
                        String::new()
                    }
                    Err(e) => {
                        let err_str = e.to_string();
                        if err_str.starts_with("file_too_large:") {
                            let parts: Vec<&str> = err_str.split(':').collect();
                            let actual = parts.get(1).unwrap_or(&"?");
                            let limit = parts.get(2).unwrap_or(&"?");
                            format!(
                                "__DIRECT_REPLY__File too large ({actual} MB, limit {limit} MB). Adjust via /config_upload_size <MB>"
                            )
                        } else {
                            format!("[file download failed: {e}]")
                        }
                    }
                }
            }
            _ => {
                debug!(msg_type, "feishu: unsupported message type, skipping");
                return Ok(None);
            }
        };

        if (text.is_empty() && file_attachments.is_empty()) || sender_id.is_empty() {
            return Ok(None);
        }

        let is_group = chat_type == "group";
        info!(from = %sender_id, chat = %chat_id, is_group, text_len = text.len(), files = file_attachments.len(), "feishu: message received");

        (self.on_message)(sender_id, text, chat_id, is_group, images, file_attachments);

        Ok(None)
    }

    // -----------------------------------------------------------------------
    // Voice / audio download
    // -----------------------------------------------------------------------

    /// Download a voice/file resource attached to a message.
    #[allow(dead_code)]
    /// Download a file resource. `max_size` is checked against Content-Length
    /// before downloading to avoid wasting bandwidth/memory on oversized files.
    async fn download_resource(
        &self,
        message_id: &str,
        file_key: &str,
        max_size: usize,
    ) -> Result<Vec<u8>> {
        let token = self.get_token().await?;
        let url = format!(
            "{}/im/v1/messages/{message_id}/resources/{file_key}?type=file",
            self.api_base()
        );

        // Use a longer timeout for file downloads (5 min)
        let dl_client = crate::config::build_proxy_client()
            .timeout(Duration::from_secs(300))
            .build()
            .unwrap_or_else(|_| self.client.clone());

        let resp = dl_client
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .context("feishu: download resource")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("feishu: download_resource failed {status}: {body}");
        }

        // Check Content-Length before downloading
        if let Some(cl) = resp.content_length() {
            debug!(content_length = cl, "feishu: resource content-length");
            if cl > max_size as u64 {
                anyhow::bail!(
                    "file_too_large:{:.1}:{:.1}",
                    cl as f64 / 1e6,
                    max_size as f64 / 1e6
                );
            }
        }

        let bytes = resp.bytes().await.context("feishu: read resource bytes")?;
        debug!(
            size = bytes.len(),
            message_id, file_key, "feishu: resource downloaded"
        );
        Ok(bytes.to_vec())
    }

    /// Download an image resource attached to a message.
    async fn download_image(&self, message_id: &str, file_key: &str) -> Result<Vec<u8>> {
        let token = self.get_token().await?;
        let url = format!(
            "{}/im/v1/messages/{message_id}/resources/{file_key}?type=image",
            self.api_base()
        );

        let resp = self
            .client
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .context("feishu: download image")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("feishu: download_image failed {status}: {body}");
        }

        let bytes = resp.bytes().await.context("feishu: read image bytes")?;
        debug!(
            size = bytes.len(),
            message_id, file_key, "feishu: image downloaded"
        );
        Ok(bytes.to_vec())
    }

    /// Download and transcribe a voice message.
    #[allow(dead_code)]
    async fn transcribe_voice(&self, message_id: &str, file_key: &str) -> Result<String> {
        let audio_bytes = self
            .download_resource(message_id, file_key, self.max_file_size)
            .await?;
        transcribe_audio(&self.client, &audio_bytes, "voice.ogg", "audio/ogg").await
    }

    // -----------------------------------------------------------------------
    // Message parsing (retained for potential REST fallback)
    // -----------------------------------------------------------------------

    /// Extract text from a Feishu message, handling text and voice types.
    #[allow(dead_code)]
    async fn extract_message_text(&self, msg: &FeishuMessage) -> Option<String> {
        match msg.msg_type.as_str() {
            "text" => {
                let content_str = msg.body.as_ref()?.content.as_ref()?;
                let parsed: TextContent = serde_json::from_str(content_str).ok()?;
                let text = parsed.text?;
                if text.is_empty() { None } else { Some(text) }
            }
            "audio" => {
                let content_str = msg.body.as_ref()?.content.as_ref()?;
                let parsed: FileContent = serde_json::from_str(content_str).ok()?;
                let file_key = parsed.file_key?;
                match self.transcribe_voice(&msg.message_id, &file_key).await {
                    Ok(text) => {
                        info!(chars = text.len(), "feishu: voice transcribed");
                        Some(text)
                    }
                    Err(e) => {
                        warn!("feishu: voice transcription failed: {e:#}");
                        None
                    }
                }
            }
            other => {
                debug!(
                    msg_type = other,
                    "feishu: unsupported message type, skipping"
                );
                None
            }
        }
    }

    /// Determine sender open_id from a message.
    fn sender_id(msg: &FeishuMessage) -> String {
        msg.sender
            .as_ref()
            .and_then(|s| s.sender_id.as_ref())
            .and_then(|id| {
                id.open_id
                    .clone()
                    .or_else(|| id.user_id.clone())
                    .or_else(|| id.union_id.clone())
            })
            .unwrap_or_default()
    }

    /// Check if the sender is a bot (to avoid echo loops).
    fn is_bot_sender(msg: &FeishuMessage) -> bool {
        msg.sender
            .as_ref()
            .and_then(|s| s.sender_type.as_deref())
            .is_some_and(|t| t == "app")
    }
}

/// Try to base64-decode a string and parse it as JSON.
fn base64_decode_json(s: &str) -> Option<serde_json::Value> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(s).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    serde_json::from_str(&text).ok()
}

// ---------------------------------------------------------------------------
// Channel trait
// ---------------------------------------------------------------------------

impl Channel for FeishuChannel {
    fn name(&self) -> &str {
        "feishu"
    }

    fn send(&self, msg: OutboundMessage) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let chunk_cfg = ChunkConfig {
                max_chars: platform_chunk_limit("feishu"),
                min_chars: 1,
                break_preference: super::chunker::BreakPreference::Paragraph,
            };
            if !msg.text.is_empty() {
                let chunks = chunk_text(&msg.text, &chunk_cfg);
                for (i, chunk) in chunks.iter().enumerate() {
                    if i == 0
                        && let Some(ref reply_id) = msg.reply_to
                    {
                        self.reply_text_chunk(reply_id, chunk).await?;
                        continue;
                    }
                    self.send_text_chunk(&msg.target_id, chunk).await?;
                }
            }

            // Send image attachments: upload to Feishu, then send image message.
            for image_data in &msg.images {
                use base64::Engine;
                let (mime, bytes) =
                    if let Some(rest) = image_data.strip_prefix("data:image/png;base64,") {
                        match base64::engine::general_purpose::STANDARD.decode(rest) {
                            Ok(b) if !b.is_empty() => ("image/png", b),
                            _ => { warn!("feishu: failed to decode base64 image"); continue; }
                        }
                    } else if let Some(rest) = image_data.strip_prefix("data:image/jpeg;base64,") {
                        match base64::engine::general_purpose::STANDARD.decode(rest) {
                            Ok(b) if !b.is_empty() => ("image/jpeg", b),
                            _ => { warn!("feishu: failed to decode base64 image"); continue; }
                        }
                    } else if let Some(rest) = image_data.strip_prefix("data:image/webp;base64,") {
                        match base64::engine::general_purpose::STANDARD.decode(rest) {
                            Ok(b) if !b.is_empty() => ("image/webp", b),
                            _ => { warn!("feishu: failed to decode base64 image"); continue; }
                        }
                    } else if image_data.starts_with("http://") || image_data.starts_with("https://") {
                        // URL image — download first
                        match self.client.get(image_data.as_str()).send().await {
                            Ok(resp) if resp.status().is_success() => {
                                let ct = resp.headers().get("content-type")
                                    .and_then(|v| v.to_str().ok())
                                    .unwrap_or("image/png")
                                    .to_owned();
                                let mime = if ct.contains("jpeg") || ct.contains("jpg") { "image/jpeg" }
                                    else if ct.contains("webp") { "image/webp" }
                                    else { "image/png" };
                                match resp.bytes().await {
                                    Ok(b) if !b.is_empty() => (mime, b.to_vec()),
                                    _ => { warn!("feishu: empty image download"); continue; }
                                }
                            }
                            Ok(resp) => { warn!(status = %resp.status(), "feishu: image download failed"); continue; }
                            Err(e) => { warn!(error = %e, "feishu: image download error"); continue; }
                        }
                    } else {
                        warn!("feishu: unrecognised image data, skipping");
                        continue;
                    };

                let filename = if mime == "image/jpeg" {
                    "image.jpg"
                } else {
                    "image.png"
                };

                // Upload image to Feishu to get image_key.
                let token = match self.get_token().await {
                    Ok(t) => t,
                    Err(e) => {
                        warn!("feishu: failed to get token for image upload: {e}");
                        continue;
                    }
                };
                let part = match reqwest::multipart::Part::bytes(bytes)
                    .file_name(filename)
                    .mime_str(mime)
                {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("feishu: failed to build multipart part: {e}");
                        continue;
                    }
                };
                let form = reqwest::multipart::Form::new()
                    .text("image_type", "message")
                    .part("image", part);
                let upload_url = format!("{}/im/v1/images", self.api_base());
                let upload_resp = self
                    .client
                    .post(&upload_url)
                    .bearer_auth(&token)
                    .multipart(form)
                    .send()
                    .await;

                let image_key = match upload_resp {
                    Ok(r) => match r.json::<serde_json::Value>().await {
                        Ok(body) => {
                            if let Some(k) =
                                body.pointer("/data/image_key").and_then(|v| v.as_str())
                            {
                                k.to_owned()
                            } else {
                                warn!("feishu: image upload response missing image_key: {body}");
                                continue;
                            }
                        }
                        Err(e) => {
                            warn!("feishu: failed to parse image upload response: {e}");
                            continue;
                        }
                    },
                    Err(e) => {
                        warn!("feishu: image upload request failed: {e}");
                        continue;
                    }
                };

                // Send image message using image_key.
                let id_type = if msg.target_id.starts_with("ou_") { "open_id" }
                    else if msg.target_id.starts_with("on_") { "union_id" }
                    else if msg.target_id.starts_with("oc_") { "chat_id" }
                    else { "chat_id" };
                let send_url =
                    format!("{}/im/v1/messages?receive_id_type={id_type}", self.api_base());
                let token2 = match self.get_token().await {
                    Ok(t) => t,
                    Err(e) => {
                        warn!("feishu: failed to get token for image send: {e}");
                        continue;
                    }
                };
                match self
                    .client
                    .post(&send_url)
                    .bearer_auth(&token2)
                    .json(&serde_json::json!({
                        "receive_id": msg.target_id,
                        "msg_type": "image",
                        "content": serde_json::json!({"image_key": image_key}).to_string(),
                    }))
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => {
                        debug!("feishu: image message sent");
                    }
                    Ok(r) => {
                        let status = r.status();
                        let err = r.text().await.unwrap_or_default();
                        warn!("feishu: image send failed {status}: {err}");
                    }
                    Err(e) => {
                        warn!("feishu: image send request failed: {e}");
                    }
                }
            }

            // Send file attachments: upload to Feishu, then send file/media message.
            for (filename, mime, path_or_url) in &msg.files {
                let bytes = if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
                    match self.client.get(path_or_url.as_str()).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            match resp.bytes().await {
                                Ok(b) if !b.is_empty() => b.to_vec(),
                                _ => { warn!("feishu: empty file download"); continue; }
                            }
                        }
                        _ => { warn!("feishu: file download failed: {path_or_url}"); continue; }
                    }
                } else {
                    match std::fs::read(path_or_url) {
                        Ok(b) => b,
                        Err(e) => { warn!("feishu: failed to read file {path_or_url}: {e}"); continue; }
                    }
                };

                let token = match self.get_token().await {
                    Ok(t) => t,
                    Err(e) => { warn!("feishu: token error for file upload: {e}"); continue; }
                };

                // Feishu separates media (video/audio) from files (pdf/doc/xls).
                let is_media = mime.starts_with("video/") || mime.starts_with("audio/");

                // Feishu requires opus for audio. Convert mp3/wav/aiff to ogg-opus (pure Rust).
                let (bytes, filename, mime_override) = if mime.starts_with("audio/") && !filename.ends_with(".ogg") && !filename.ends_with(".opus") {
                    let ext = filename.rsplit('.').next().unwrap_or("mp3");
                    match crate::channel::transcription::encode_audio_to_ogg_opus(&bytes, Some(ext)) {
                        Ok(opus_bytes) => {
                            let opus_name = filename.rsplit_once('.').map(|(n, _)| format!("{n}.ogg")).unwrap_or_else(|| format!("{filename}.ogg"));
                            info!(src_len = bytes.len(), opus_len = opus_bytes.len(), "feishu: converted audio to ogg-opus");
                            (opus_bytes, opus_name, "audio/ogg")
                        }
                        Err(e) => {
                            warn!("feishu: ogg-opus conversion failed, uploading as-is: {e:#}");
                            (bytes, filename.clone(), mime.as_str())
                        }
                    }
                } else {
                    (bytes, filename.clone(), mime.as_str())
                };

                let file_type = if is_media {
                    // Feishu requires file_type "opus" for audio.
                    if mime.starts_with("video/") { "mp4" } else { "opus" }
                } else if mime.contains("pdf") { "pdf" }
                    else if mime.contains("doc") { "doc" }
                    else if mime.contains("sheet") || mime.contains("xls") { "xls" }
                    else if mime.contains("ppt") || mime.contains("presentation") { "ppt" }
                    else { "stream" };

                // All files (including video/audio) upload to /im/v1/files.
                // Video/audio use file_type "mp4"/"mp3" and send as msg_type "media".
                // Documents use their respective file_type and send as msg_type "file".
                let upload_url = format!("{}/im/v1/files", self.api_base());

                let part = match reqwest::multipart::Part::bytes(bytes)
                    .file_name(filename.clone())
                    .mime_str(mime_override)
                {
                    Ok(p) => p,
                    Err(e) => { warn!("feishu: multipart error: {e}"); continue; }
                };
                let mut form = reqwest::multipart::Form::new()
                    .text("file_type", file_type.to_owned())
                    .text("file_name", filename.clone())
                    .part("file", part);
                // Add duration (ms) for video/audio uploads.
                // Feishu requires duration for media uploads (234001 error without it).
                if is_media {
                    let dur = if mime.starts_with("video/") {
                        mp4_duration_ms(path_or_url).unwrap_or(0)
                    } else {
                        // Audio: try ffprobe, fallback to estimate from file size.
                        audio_duration_ms(path_or_url).unwrap_or(0)
                    };
                    // Always send duration for media, default 1000ms if unknown.
                    let dur = if dur > 0 { dur } else { 1000 };
                    form = form.text("duration", dur.to_string());
                    info!(duration_ms = dur, "feishu: uploading media with duration");
                }

                let upload_resp = self.client
                    .post(&upload_url)
                    .bearer_auth(&token)
                    .multipart(form)
                    .send()
                    .await;

                let file_key = match upload_resp {
                    Ok(r) => match r.json::<serde_json::Value>().await {
                        Ok(body) => {
                            if let Some(k) = body.pointer("/data/file_key").and_then(|v| v.as_str()) {
                                k.to_owned()
                            } else {
                                warn!("feishu: upload missing file_key: {body}");
                                continue;
                            }
                        }
                        Err(e) => { warn!("feishu: upload parse error: {e}"); continue; }
                    },
                    Err(e) => { warn!("feishu: upload failed: {e}"); continue; }
                };

                // Send: video/audio as "media", others as "file".
                let id_type = if msg.target_id.starts_with("ou_") { "open_id" }
                    else if msg.target_id.starts_with("on_") { "union_id" }
                    else if msg.target_id.starts_with("oc_") { "chat_id" }
                    else { "chat_id" };
                let send_url = format!("{}/im/v1/messages?receive_id_type={id_type}", self.api_base());
                let (msg_type, content) = if is_media {
                    if mime.starts_with("audio/") {
                        // Audio: send as "audio" msg_type with file_key + duration.
                        let dur_ms = audio_duration_ms(path_or_url).unwrap_or(1000);
                        // Feishu audio duration is in milliseconds as string.
                        let s = serde_json::json!({"file_key": file_key, "duration": dur_ms}).to_string();
                        info!(content = %s, duration_ms = dur_ms, "feishu: sending audio message");
                        ("audio", s)
                    } else {
                        // Video: send as "media" msg_type with file_key + file_name.
                        let mut media_json = serde_json::json!({"file_key": file_key, "file_name": filename});
                        let api = self.api_base().to_owned();
                        if let Some(cover_key) = extract_and_upload_cover(path_or_url, &self.client, &api, &token).await {
                            media_json["image_key"] = serde_json::json!(cover_key);
                        }
                        let s = media_json.to_string();
                        info!(content = %s, "feishu: sending media message");
                        ("media", s)
                    }
                } else {
                    ("file", serde_json::json!({"file_key": file_key}).to_string())
                };

                let token2 = match self.get_token().await {
                    Ok(t) => t,
                    Err(e) => { warn!("feishu: token error for file send: {e}"); continue; }
                };
                match self.client
                    .post(&send_url)
                    .bearer_auth(&token2)
                    .json(&serde_json::json!({
                        "receive_id": msg.target_id,
                        "msg_type": msg_type,
                        "content": content,
                    }))
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => {
                        debug!("feishu: {msg_type} message sent: {filename}");
                    }
                    Ok(r) => {
                        let status = r.status();
                        let err = r.text().await.unwrap_or_default();
                        warn!("feishu: {msg_type} send failed {status}: {err}");
                    }
                    Err(e) => { warn!("feishu: {msg_type} send error: {e}"); }
                }
            }

            Ok(())
        })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            info!("feishu: starting WebSocket mode");
            loop {
                match self.ws_connect_loop().await {
                    Ok(_) => info!("feishu: WS connection ended, reconnecting..."),
                    Err(e) => warn!("feishu: WS error: {e:#}, reconnecting in 5s"),
                }
                sleep(Duration::from_secs(5)).await;
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

    fn init_crypto() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }

    #[test]
    fn channel_name() {
        init_crypto();
        let ch = FeishuChannel::new(
            "app_id",
            "app_secret",
            vec![],
            Arc::new(|_, _, _, _, _, _| {}),
        );
        assert_eq!(ch.name(), "feishu");
    }

    #[test]
    fn sender_id_extraction() {
        let msg = FeishuMessage {
            message_id: "m1".into(),
            msg_type: "text".into(),
            body: None,
            sender: Some(MessageSender {
                sender_id: Some(SenderIdInfo {
                    open_id: Some("ou_abc123".into()),
                    user_id: None,
                    union_id: None,
                }),
                sender_type: Some("user".into()),
            }),
            chat_id: Some("oc_test".into()),
            create_time: "1700000000000".into(),
        };
        assert_eq!(FeishuChannel::sender_id(&msg), "ou_abc123");
    }

    #[test]
    fn bot_sender_detected() {
        let msg = FeishuMessage {
            message_id: "m2".into(),
            msg_type: "text".into(),
            body: None,
            sender: Some(MessageSender {
                sender_id: None,
                sender_type: Some("app".into()),
            }),
            chat_id: None,
            create_time: String::new(),
        };
        assert!(FeishuChannel::is_bot_sender(&msg));
    }

    #[test]
    fn user_sender_not_bot() {
        let msg = FeishuMessage {
            message_id: "m3".into(),
            msg_type: "text".into(),
            body: None,
            sender: Some(MessageSender {
                sender_id: None,
                sender_type: Some("user".into()),
            }),
            chat_id: None,
            create_time: String::new(),
        };
        assert!(!FeishuChannel::is_bot_sender(&msg));
    }

    #[test]
    fn text_content_parse() {
        let raw = r#"{"text":"hello world"}"#;
        let parsed: TextContent = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.text.as_deref(), Some("hello world"));
    }

    #[test]
    fn feishu_chunk_limit() {
        let limit = platform_chunk_limit("feishu");
        assert!(limit >= 4000);
    }

    #[test]
    fn ws_event_json_data() {
        // Verify parsing of a WS frame with JSON-string data field
        let frame = r#"{"type":"event","data":"{\"header\":{\"event_type\":\"im.message.receive_v1\"},\"event\":{\"message\":{\"message_type\":\"text\",\"content\":\"{\\\"text\\\":\\\"hello\\\"}\",\"chat_id\":\"oc_test\",\"chat_type\":\"p2p\"},\"sender\":{\"sender_type\":\"user\",\"sender_id\":{\"open_id\":\"ou_xxx\"}}}}"}"#;
        let val: serde_json::Value = serde_json::from_str(frame).unwrap();
        let frame_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
        assert_eq!(frame_type, "event");

        let data_str = val.get("data").and_then(|v| v.as_str()).unwrap();
        let event: serde_json::Value = serde_json::from_str(data_str).unwrap();
        let event_type = event
            .pointer("/header/event_type")
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(event_type, "im.message.receive_v1");
    }

    #[test]
    fn ws_pong_frame_ignored() {
        let frame = r#"{"type":"pong"}"#;
        let val: serde_json::Value = serde_json::from_str(frame).unwrap();
        let frame_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
        assert_eq!(frame_type, "pong");
    }

    #[test]
    fn base64_decode_valid() {
        // base64 of '{"hello":"world"}'
        use base64::Engine;
        let json_str = r#"{"hello":"world"}"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(json_str);
        let decoded = base64_decode_json(&encoded).unwrap();
        assert_eq!(decoded.get("hello").and_then(|v| v.as_str()), Some("world"));
    }

    #[test]
    fn base64_decode_invalid() {
        assert!(base64_decode_json("not-valid-base64!!!").is_none());
    }
}

// ---------------------------------------------------------------------------
// FeishuNotifier for ACP notifications
// ---------------------------------------------------------------------------

use crate::acp::notification::{Notification, NotificationPriority, NotificationSink};

pub struct FeishuNotifier {
    app_id: String,
    app_secret: String,
    brand: String,
    target_chat_id: String,
    client: Client,
}

impl FeishuNotifier {
    pub fn new(app_id: &str, app_secret: &str, target_chat_id: &str, brand: &str) -> Self {
        Self {
            app_id: app_id.to_string(),
            app_secret: app_secret.to_string(),
            brand: brand.to_string(),
            target_chat_id: target_chat_id.to_string(),
            client: Client::new(),
        }
    }

    async fn get_token(&self) -> Result<String> {
        let url = format!("{}/auth/v3/tenant_access_token/internal", self.api_base());
        let body = json!({
            "app_id": self.app_id,
            "app_secret": self.app_secret,
        });
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("feishu: get token")?;
        let token_resp: FeishuTokenResponse =
            resp.json().await.context("feishu: parse token response")?;
        token_resp
            .tenant_access_token
            .context("feishu: no token in response")
    }

    fn api_base(&self) -> String {
        if self.brand == "lark" {
            LARK_API_BASE.to_string()
        } else {
            FEISHU_API_BASE.to_string()
        }
    }

    async fn send_text(&self, text: &str) -> Result<()> {
        let token = self.get_token().await?;
        let id_type = if self.target_chat_id.starts_with("ou_") { "open_id" }
            else if self.target_chat_id.starts_with("on_") { "union_id" }
            else if self.target_chat_id.starts_with("oc_") { "chat_id" }
            else { "chat_id" };
        let url = format!("{}/im/v1/messages?receive_id_type={id_type}", self.api_base());

        let card_payload = build_feishu_card(text, &self.brand);
        let card_str =
            serde_json::to_string(&card_payload["card"]).context("feishu: serialize card")?;

        let body = json!({
            "receive_id": self.target_chat_id,
            "msg_type": "interactive",
            "content": card_str,
        });

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .context("feishu: send notification")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("feishu: send_notification failed {status}: {body}");
        }

        Ok(())
    }
}

impl NotificationSink for FeishuNotifier {
    fn name(&self) -> &str {
        "feishu"
    }

    fn priority_filter(&self) -> NotificationPriority {
        NotificationPriority::Medium
    }

    fn send(&self, notification: &Notification) -> BoxFuture<'_, Result<()>> {
        let text = if notification.burn_after_read {
            format!(
                "**[阅后即焚]**\n\n**{}**\n\n{}\n\n_session_id: {}_",
                notification.title,
                notification.body,
                notification.session_id.as_deref().unwrap_or("N/A")
            )
        } else {
            format!(
                "**{}**\n\n{}\n\n_session_id: {}_",
                notification.title,
                notification.body,
                notification.session_id.as_deref().unwrap_or("N/A")
            )
        };

        Box::pin(async move { self.send_text(&text).await })
    }
}

/// Extract a cover frame from video via ffmpeg, upload to Feishu images API, return image_key.
/// Returns None if ffmpeg is not available or extraction fails.
async fn extract_and_upload_cover(
    video_path: &str,
    client: &reqwest::Client,
    api_base: &str,
    token: &str,
) -> Option<String> {
    let cover_path = format!("{}.cover.jpg", video_path);
    // Run ffmpeg to extract first frame at 1s.
    let output = std::process::Command::new("ffmpeg")
        .args(["-y", "-i", video_path, "-ss", "00:00:01", "-frames:v", "1", "-q:v", "2", &cover_path])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        // Retry at 0s (video might be shorter than 1s).
        let _ = std::process::Command::new("ffmpeg")
            .args(["-y", "-i", video_path, "-ss", "00:00:00", "-frames:v", "1", "-q:v", "2", &cover_path])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
    }
    let cover_bytes = std::fs::read(&cover_path).ok()?;
    // Clean up temp file.
    let _ = std::fs::remove_file(&cover_path);
    if cover_bytes.is_empty() { return None; }

    // Upload to Feishu images API.
    let part = reqwest::multipart::Part::bytes(cover_bytes)
        .file_name("cover.jpg")
        .mime_str("image/jpeg").ok()?;
    let form = reqwest::multipart::Form::new()
        .text("image_type", "message")
        .part("image", part);
    let upload_url = format!("{}/im/v1/images", api_base);
    let resp = client.post(&upload_url)
        .bearer_auth(token)
        .multipart(form)
        .send().await.ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;
    let key = body.pointer("/data/image_key")?.as_str()?;
    tracing::info!(image_key = %key, "feishu: video cover uploaded");
    Some(key.to_owned())
}

/// Extract duration in milliseconds from an MP4 file by parsing the moov/mvhd atom.
/// Returns None if the file is not MP4 or parsing fails.
fn mp4_duration_ms(path: &str) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let file_len = f.metadata().ok()?.len();
    let mut pos: u64 = 0;

    // Find moov atom.
    let moov_start = loop {
        if pos >= file_len { return None; }
        f.seek(SeekFrom::Start(pos)).ok()?;
        let mut header = [0u8; 8];
        f.read_exact(&mut header).ok()?;
        let size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as u64;
        let tag = &header[4..8];
        if tag == b"moov" {
            break pos;
        }
        if size < 8 { return None; }
        pos += size;
    };

    // Find mvhd inside moov.
    f.seek(SeekFrom::Start(moov_start + 8)).ok()?;
    let mut moov_buf = [0u8; 8];
    let moov_end = moov_start + {
        f.seek(SeekFrom::Start(moov_start)).ok()?;
        let mut h = [0u8; 4];
        f.read_exact(&mut h).ok()?;
        u32::from_be_bytes(h) as u64
    };

    let mut scan = moov_start + 8;
    while scan < moov_end {
        f.seek(SeekFrom::Start(scan)).ok()?;
        f.read_exact(&mut moov_buf).ok()?;
        let atom_size = u32::from_be_bytes([moov_buf[0], moov_buf[1], moov_buf[2], moov_buf[3]]) as u64;
        if &moov_buf[4..8] == b"mvhd" {
            // mvhd: version(1) + flags(3) + create(4) + modify(4) + timescale(4) + duration(4)
            // version 1: create(8) + modify(8) + timescale(4) + duration(8)
            let mut ver = [0u8; 1];
            f.read_exact(&mut ver).ok()?;
            if ver[0] == 0 {
                let mut buf = [0u8; 16]; // skip create+modify (8), then timescale(4)+duration(4)
                f.seek(SeekFrom::Start(scan + 8 + 1 + 3)).ok()?; // after version+flags
                f.read_exact(&mut buf).ok()?;
                let timescale = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
                let duration = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]);
                if timescale > 0 {
                    return Some((duration as u64) * 1000 / (timescale as u64));
                }
            } else {
                let mut buf = [0u8; 28]; // skip create+modify (16), then timescale(4)+duration(8)
                f.seek(SeekFrom::Start(scan + 8 + 1 + 3)).ok()?;
                f.read_exact(&mut buf).ok()?;
                let timescale = u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]);
                let duration = u64::from_be_bytes([buf[20], buf[21], buf[22], buf[23], buf[24], buf[25], buf[26], buf[27]]);
                if timescale > 0 {
                    return Some(duration * 1000 / (timescale as u64));
                }
            }
            return None;
        }
        if atom_size < 8 { break; }
        scan += atom_size;
    }
    None
}

/// Get audio file duration in milliseconds using ffprobe.
/// Falls back to estimate from file size if ffprobe is not available.
fn audio_duration_ms(path: &str) -> Option<u64> {
    // Try ffprobe first.
    let output = std::process::Command::new("ffprobe")
        .args(["-v", "quiet", "-show_entries", "format=duration",
               "-of", "default=noprint_wrappers=1:nokey=1", path])
        .output()
        .ok()?;
    if output.status.success() {
        let s = String::from_utf8_lossy(&output.stdout);
        if let Ok(secs) = s.trim().parse::<f64>() {
            return Some((secs * 1000.0) as u64);
        }
    }
    // Fallback: estimate from file size (mp3 ~128kbps = 16KB/s).
    let size = std::fs::metadata(path).ok()?.len();
    Some(size * 1000 / 16_000)
}
