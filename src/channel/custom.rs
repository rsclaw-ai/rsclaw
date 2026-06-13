//! Custom user-defined channels (webhook and websocket).
//!
//! Allows users to connect any platform without writing code, using JSON path
//! extraction for inbound parsing and template-based outbound replies.

use base64::Engine as _;
use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use futures::{SinkExt as _, StreamExt as _, future::BoxFuture};
use reqwest::Client;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tracing::{debug, info, warn};

use super::{Channel, OutboundMessage};
use crate::{
    channel::retry::{SendRetry, send_with_retry},
    config::schema::CustomChannelConfig,
};

// ---------------------------------------------------------------------------
// Simple JSON path extractor
// ---------------------------------------------------------------------------

/// Extract a value from a JSON tree using a simple path notation.
///
/// Supported syntax:
/// - `$.foo.bar`     -> value["foo"]["bar"]
/// - `$.foo[0].bar`  -> value["foo"][0]["bar"]
/// - `foo.bar`       -> value["foo"]["bar"] (leading "$." is optional)
pub fn json_path_extract<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let path = path.strip_prefix("$.").unwrap_or(path);
    if path.is_empty() {
        return Some(root);
    }

    let mut current = root;
    for segment in path.split('.') {
        if segment.is_empty() {
            continue;
        }
        // Check for array indexing: "foo[0]"
        if let Some(bracket_pos) = segment.find('[') {
            let key = &segment[..bracket_pos];
            if !key.is_empty() {
                current = current.get(key)?;
            }
            // Parse index(es) like [0] or [0][1]
            let rest = &segment[bracket_pos..];
            for part in rest.split('[') {
                let part = part.trim_end_matches(']');
                if part.is_empty() {
                    continue;
                }
                let idx: usize = part.parse().ok()?;
                current = current.get(idx)?;
            }
        } else {
            current = current.get(segment)?;
        }
    }
    Some(current)
}

/// Convert a JSON value to a plain string (unquoted for strings, raw for
/// others).
fn value_as_string(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Template engine
// ---------------------------------------------------------------------------

/// Replace `{{sender}}`, `{{chat_id}}`, `{{reply}}`, `{{is_group}}`,
/// `{{images}}`, `{{files}}` in a template.
fn render_template(
    template: &str,
    sender: &str,
    chat_id: &str,
    reply: &str,
    is_group: bool,
    image_urls_json: &str,
    files_json: &str,
) -> String {
    template
        .replace("{{sender}}", sender)
        .replace("{{chat_id}}", chat_id)
        .replace("{{reply}}", &escape_json_string(reply))
        .replace("{{is_group}}", if is_group { "true" } else { "false" })
        .replace("{{images}}", image_urls_json)
        .replace("{{files}}", files_json)
}

/// Expand `${VAR}` references to environment variables.
fn expand_env_vars(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var_name = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                var_name.push(c);
            }
            if let Ok(val) = std::env::var(&var_name) {
                result.push_str(&val);
            }
        } else {
            result.push(ch);
        }
    }
    result
}

/// Escape a string for embedding in a JSON string value.
fn escape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Inbound message parsing (shared by webhook and websocket)
// ---------------------------------------------------------------------------

/// Parsed inbound message from a custom channel.
#[derive(Debug, Clone)]
pub struct ParsedMessage {
    pub text: String,
    pub sender: String,
    pub group_id: Option<String>,
    /// Raw image URLs extracted from inbound payload (to be downloaded by
    /// gateway code).
    pub image_urls: Vec<String>,
    /// File download URL extracted from inbound payload.
    pub file_url: Option<String>,
    /// Filename for the file attachment.
    pub file_name: Option<String>,
}

/// Extract a value as a string from JSON. For arrays, join elements.
fn extract_string_value<'a>(root: &'a Value, path: &str) -> Option<String> {
    let v = json_path_extract(root, path)?;
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().filter_map(|e| match e {
                Value::String(s) => Some(s.clone()),
                _ => None,
            }).collect();
            if parts.is_empty() { None } else { Some(parts.join(",")) }
        }
        other => Some(other.to_string()),
    }
}

/// Parse an inbound JSON payload using the custom channel config paths.
pub fn parse_inbound(cfg: &CustomChannelConfig, body: &str) -> Option<ParsedMessage> {
    let val: Value = serde_json::from_str(body).ok()?;

    // Apply filter: if filter_path is set, check value matches filter_value.
    if let Some(ref fp) = cfg.filter_path {
        let extracted = json_path_extract(&val, fp)?;
        if let Some(ref fv) = cfg.filter_value {
            if value_as_string(extracted) != *fv {
                return None;
            }
        }
    }

    // Extract text.
    let text = if let Some(ref tp) = cfg.text_path {
        let v = json_path_extract(&val, tp)?;
        value_as_string(v)
    } else {
        // Fallback: use entire body as text.
        body.to_owned()
    };

    if text.is_empty() {
        return None;
    }

    // Extract sender.
    let sender = if let Some(ref sp) = cfg.sender_path {
        json_path_extract(&val, sp)
            .map(value_as_string)
            .unwrap_or_else(|| "unknown".to_owned())
    } else {
        "unknown".to_owned()
    };

    // Extract group ID (optional).
    let group_id = cfg
        .group_path
        .as_ref()
        .and_then(|gp| json_path_extract(&val, gp).map(value_as_string));

    // Extract image URLs.
    let mut image_urls: Vec<String> = Vec::new();
    if let Some(ref ip) = cfg.image_url_path {
        if let Some(url) = extract_string_value(&val, ip) {
            image_urls.push(url);
        }
    }
    if let Some(ref ip) = cfg.image_urls_path {
        if let Some(val) = json_path_extract(&val, ip) {
            match val {
                Value::Array(arr) => {
                    for v in arr {
                        if let Value::String(s) = v {
                            if !s.is_empty() {
                                image_urls.push(s.clone());
                            }
                        }
                    }
                }
                Value::String(s) => image_urls.push(s.clone()),
                _ => {}
            }
        }
    }

    // Extract file URL and name.
    let file_url = cfg
        .file_url_path
        .as_ref()
        .and_then(|fp| extract_string_value(&val, fp));
    let file_name = cfg
        .file_name_path
        .as_ref()
        .and_then(|np| extract_string_value(&val, np));

    Some(ParsedMessage {
        text,
        sender,
        group_id,
        image_urls,
        file_url,
        file_name,
    })
}

// ---------------------------------------------------------------------------
// CustomWebhookChannel
// ---------------------------------------------------------------------------

/// A custom webhook channel: receives messages via POST /hooks/{name},
/// sends replies via HTTP to reply_url.
pub struct CustomWebhookChannel {
    pub cfg: CustomChannelConfig,
    client: Client,
    #[allow(clippy::type_complexity)]
    on_message:
        Arc<dyn Fn(String, String, String, Vec<crate::agent::registry::ImageAttachment>, Vec<crate::agent::registry::FileAttachment>, bool) + Send + Sync>,
}

impl CustomWebhookChannel {
    pub fn new(
        cfg: CustomChannelConfig,
        on_message: Arc<
            dyn Fn(String, String, String, Vec<crate::agent::registry::ImageAttachment>, Vec<crate::agent::registry::FileAttachment>, bool) + Send + Sync,
        >,
    ) -> Self {
        Self {
            cfg,
            client: crate::config::build_proxy_client()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            on_message,
        }
    }

    /// Handle an inbound webhook POST.
    pub async fn handle_webhook(&self, body: &str) {
        if let Some(parsed) = parse_inbound(&self.cfg, body) {
            let is_group = parsed.group_id.is_some();
            // chat_id = group_id when present (so the agent reply lands in the
            // group), else the sender's DM identity.
            let chat_id = parsed
                .group_id
                .clone()
                .unwrap_or_else(|| parsed.sender.clone());
            // Download images/files from URLs extracted by parse_inbound.
            let images = download_images(&self.client, &parsed.image_urls).await;
            let files = download_and_package_file(
                &self.client,
                &parsed.file_url,
                parsed.file_name.as_deref().unwrap_or("file.bin"),
            )
            .await;
            (self.on_message)(
                parsed.sender,
                parsed.text,
                chat_id,
                images,
                files,
                is_group,
            );
        } else {
            debug!(channel = %self.cfg.name, "custom webhook: inbound message did not match filter/paths");
        }
    }

    /// Send an outbound reply via HTTP.
    async fn send_reply(&self, msg: &OutboundMessage) -> Result<()> {
        let reply_url = match &self.cfg.reply_url {
            Some(u) => expand_env_vars(u),
            None => {
                debug!(channel = %self.cfg.name, "no reply_url configured, skipping outbound");
                return Ok(());
            }
        };

        let template = self.cfg.reply_template.as_deref().unwrap_or(
            r#"{"sender":"{{sender}}","chat_id":"{{chat_id}}","text":"{{reply}}","is_group":{{is_group}}}"#,
        );

        // Build {{images}} and {{files}} JSON arrays from the reply message.
        let image_urls_json = if msg.images.is_empty() {
            "[]".to_owned()
        } else {
            serde_json::to_string(&msg.images).unwrap_or_else(|_| "[]".to_owned())
        };
        let files_json = if msg.files.is_empty() {
            "[]".to_owned()
        } else {
            let file_entries: Vec<serde_json::Value> = msg
                .files
                .iter()
                .map(|(name, mime, path)| {
                    serde_json::json!({
                        "filename": name,
                        "mime_type": mime,
                        "url": path,
                    })
                })
                .collect();
            serde_json::to_string(&file_entries).unwrap_or_else(|_| "[]".to_owned())
        };

        let body = render_template(
            template,
            &msg.target_id,
            &msg.target_id,
            &msg.text,
            msg.is_group,
            &image_urls_json,
            &files_json,
        );

        let method = self
            .cfg
            .reply_method
            .as_deref()
            .unwrap_or("POST")
            .to_uppercase();

        // A custom reply_url is a user-controlled webhook with no standard
        // idempotency key, so retry only reliably protects against pre-commit
        // resets; a post-commit reset may deliver twice. The body is held
        // constant across attempts so a webhook that does dedupe can.
        let resp = send_with_retry("custom", &SendRetry::default(), || {
            let mut req = match method.as_str() {
                "PUT" => self.client.put(&reply_url),
                "PATCH" => self.client.patch(&reply_url),
                _ => self.client.post(&reply_url),
            };
            req = req
                .header("Content-Type", "application/json")
                .body(body.clone());
            if let Some(ref headers) = self.cfg.reply_headers {
                for (k, v) in headers {
                    req = req.header(k.as_str(), expand_env_vars(v));
                }
            }
            req
        })
        .await
        .context("custom webhook reply HTTP send")?;
        if !resp.status().is_success() {
            warn!(
                channel = %self.cfg.name,
                status = %resp.status(),
                "custom webhook reply returned non-2xx"
            );
        }
        Ok(())
    }
}

impl Channel for CustomWebhookChannel {
    fn name(&self) -> &str {
        &self.cfg.name
    }

    fn send(&self, msg: OutboundMessage) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { self.send_reply(&msg).await })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        // Webhook channels are passive -- they wait for POST requests.
        // Nothing to poll. Just keep alive.
        Box::pin(async move {
            info!(channel = %self.cfg.name, "custom webhook channel ready");
            // Block forever (channel stays alive as long as the gateway runs).
            futures::future::pending::<()>().await;
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// CustomWebSocketChannel
// ---------------------------------------------------------------------------

/// A custom WebSocket channel: connects to an external WS server, handles auth,
/// heartbeat, inbound parsing, and outbound reply frames.
pub struct CustomWebSocketChannel {
    pub cfg: CustomChannelConfig,
    client: Client,
    #[allow(clippy::type_complexity)]
    on_message:
        Arc<dyn Fn(String, String, String, Vec<crate::agent::registry::ImageAttachment>, Vec<crate::agent::registry::FileAttachment>, bool) + Send + Sync>,
    /// Sender half for outbound messages.
    ws_tx: Mutex<Option<mpsc::Sender<String>>>,
}

impl CustomWebSocketChannel {
    pub fn new(
        cfg: CustomChannelConfig,
        on_message: Arc<
            dyn Fn(String, String, String, Vec<crate::agent::registry::ImageAttachment>, Vec<crate::agent::registry::FileAttachment>, bool) + Send + Sync,
        >,
    ) -> Self {
        Self {
            client: crate::config::build_proxy_client()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            cfg,
            on_message,
            ws_tx: Mutex::new(None),
        }
    }

    /// Build the WS connect request with optional custom headers.
    async fn connect_ws(
        &self,
    ) -> Result<(
        futures::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            WsMessage,
        >,
        futures::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
    )> {
        let url = self.cfg.ws_url.as_deref().ok_or_else(|| {
            anyhow::anyhow!("custom WS channel '{}': ws_url is required", self.cfg.name)
        })?;
        let url = expand_env_vars(url);

        if let Some(ref headers_map) = self.cfg.ws_headers {
            use tokio_tungstenite::tungstenite::http::Request;
            let mut req = Request::builder().uri(&url);
            for (k, v) in headers_map {
                req = req.header(k.as_str(), expand_env_vars(v).as_str());
            }
            let req = req.body(()).context("custom WS: failed to build request")?;
            let (stream, _resp) = connect_async(req)
                .await
                .with_context(|| format!("custom WS connect to {url}"))?;
            Ok(stream.split())
        } else {
            let (stream, _resp) = connect_async(&url)
                .await
                .with_context(|| format!("custom WS connect to {url}"))?;
            Ok(stream.split())
        }
    }

    /// Send auth frame and validate response.
    async fn authenticate(
        &self,
        write: &mut futures::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            WsMessage,
        >,
        read: &mut futures::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
    ) -> Result<()> {
        let auth_frame = match &self.cfg.auth_frame {
            Some(f) => expand_env_vars(f),
            None => return Ok(()), // No auth required.
        };

        info!(channel = %self.cfg.name, "sending auth frame");
        write
            .send(WsMessage::Text(auth_frame.into()))
            .await
            .context("custom WS: failed to send auth frame")?;

        // If auth_success_path is configured, wait for a response and check it.
        if let Some(ref success_path) = self.cfg.auth_success_path {
            let resp = tokio::time::timeout(Duration::from_secs(10), read.next())
                .await
                .context("custom WS: auth response timeout")?
                .ok_or_else(|| anyhow::anyhow!("custom WS: connection closed during auth"))?
                .context("custom WS: error reading auth response")?;

            let text = match resp {
                WsMessage::Text(t) => t.to_string(),
                WsMessage::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                _ => bail!("custom WS: unexpected frame type in auth response"),
            };

            let val: Value = serde_json::from_str(&text)
                .context("custom WS: auth response is not valid JSON")?;

            if let Some(extracted) = json_path_extract(&val, success_path) {
                if let Some(ref expected) = self.cfg.auth_success_value {
                    if value_as_string(extracted) != *expected {
                        bail!(
                            "custom WS auth failed: expected '{}' at '{}', got '{}'",
                            expected,
                            success_path,
                            value_as_string(extracted)
                        );
                    }
                }
                info!(channel = %self.cfg.name, "WS auth successful");
            } else {
                bail!(
                    "custom WS auth failed: path '{}' not found in response",
                    success_path
                );
            }
        }

        Ok(())
    }

    /// Send a reply frame on the WS connection.
    fn format_reply(&self, msg: &OutboundMessage) -> Option<String> {
        let template = self.cfg.reply_frame.as_ref()?;
        let image_urls_json = if msg.images.is_empty() {
            "[]".to_owned()
        } else {
            serde_json::to_string(&msg.images).unwrap_or_else(|_| "[]".to_owned())
        };
        let files_json = if msg.files.is_empty() {
            "[]".to_owned()
        } else {
            let file_entries: Vec<serde_json::Value> = msg
                .files
                .iter()
                .map(|(name, mime, path)| {
                    serde_json::json!({
                        "filename": name,
                        "mime_type": mime,
                        "url": path,
                    })
                })
                .collect();
            serde_json::to_string(&file_entries).unwrap_or_else(|_| "[]".to_owned())
        };
        Some(render_template(
            template,
            &msg.target_id,
            &msg.target_id,
            &msg.text,
            msg.is_group,
            &image_urls_json,
            &files_json,
        ))
    }
}

impl Channel for CustomWebSocketChannel {
    fn name(&self) -> &str {
        &self.cfg.name
    }

    fn send(&self, msg: OutboundMessage) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if let Some(frame) = self.format_reply(&msg) {
                let guard = self.ws_tx.lock().await;
                if let Some(ref tx) = *guard {
                    tx.send(frame)
                        .await
                        .context("custom WS: failed to enqueue reply frame")?;
                } else {
                    warn!(channel = %self.cfg.name, "WS not connected, dropping reply");
                }
            } else {
                debug!(channel = %self.cfg.name, "no reply_frame template configured");
            }
            Ok(())
        })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            loop {
                info!(channel = %self.cfg.name, "custom WS channel connecting...");

                match self.run_once().await {
                    Ok(()) => {
                        info!(channel = %self.cfg.name, "custom WS channel disconnected cleanly");
                    }
                    Err(e) => {
                        warn!(channel = %self.cfg.name, error = %e, "custom WS channel error");
                    }
                }

                // Reconnect after a delay.
                info!(channel = %self.cfg.name, "reconnecting in 5s...");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        })
    }
}

impl CustomWebSocketChannel {
    async fn run_once(self: &Arc<Self>) -> Result<()> {
        let (mut write, mut read) = self.connect_ws().await?;

        // Auth.
        self.authenticate(&mut write, &mut read).await?;

        info!(channel = %self.cfg.name, "custom WS channel connected");

        // Set up outbound sender.
        let (out_tx, mut out_rx) = mpsc::channel::<String>(64);
        {
            let mut guard = self.ws_tx.lock().await;
            *guard = Some(out_tx);
        }

        // Heartbeat setup.
        let hb_interval = self.cfg.heartbeat_interval.unwrap_or(0);
        let hb_frame = self.cfg.heartbeat_frame.clone();
        let mut hb_timer = if hb_interval > 0 && hb_frame.is_some() {
            Some(tokio::time::interval(Duration::from_secs(hb_interval)))
        } else {
            None
        };

        loop {
            tokio::select! {
                // Inbound WS message.
                frame = read.next() => {
                    match frame {
                        Some(Ok(WsMessage::Text(text))) => {
                            let text_str: &str = &text;
                            if let Some(parsed) = parse_inbound(&self.cfg, text_str) {
                                let is_group = parsed.group_id.is_some();
                                let chat_id = parsed
                                    .group_id
                                    .clone()
                                    .unwrap_or_else(|| parsed.sender.clone());
                                let images =
                                    download_images(&self.client, &parsed.image_urls).await;
                                let files = download_and_package_file(
                                    &self.client,
                                    &parsed.file_url,
                                    parsed.file_name.as_deref().unwrap_or("file.bin"),
                                )
                                .await;
                                (self.on_message)(
                                    parsed.sender,
                                    parsed.text,
                                    chat_id,
                                    images,
                                    files,
                                    is_group,
                                );
                            }
                        }
                        Some(Ok(WsMessage::Binary(data))) => {
                            let text = String::from_utf8_lossy(&data);
                            if let Some(parsed) = parse_inbound(&self.cfg, &text) {
                                let is_group = parsed.group_id.is_some();
                                let chat_id = parsed
                                    .group_id
                                    .clone()
                                    .unwrap_or_else(|| parsed.sender.clone());
                                let images =
                                    download_images(&self.client, &parsed.image_urls).await;
                                let files = download_and_package_file(
                                    &self.client,
                                    &parsed.file_url,
                                    parsed.file_name.as_deref().unwrap_or("file.bin"),
                                )
                                .await;
                                (self.on_message)(
                                    parsed.sender,
                                    parsed.text,
                                    chat_id,
                                    images,
                                    files,
                                    is_group,
                                );
                            }
                        }
                        Some(Ok(WsMessage::Close(_))) => {
                            info!(channel = %self.cfg.name, "WS close frame received");
                            break;
                        }
                        Some(Ok(WsMessage::Ping(data))) => {
                            let _ = write.send(WsMessage::Pong(data)).await;
                        }
                        Some(Ok(_)) => {} // Pong, Frame — ignore
                        Some(Err(e)) => {
                            warn!(channel = %self.cfg.name, error = %e, "WS read error");
                            break;
                        }
                        None => {
                            info!(channel = %self.cfg.name, "WS stream ended");
                            break;
                        }
                    }
                }

                // Outbound reply frame.
                Some(frame) = out_rx.recv() => {
                    if let Err(e) = write.send(WsMessage::Text(frame.into())).await {
                        warn!(channel = %self.cfg.name, error = %e, "WS write error");
                        break;
                    }
                }

                // Heartbeat.
                _ = async {
                    match hb_timer.as_mut() {
                        Some(t) => t.tick().await,
                        None => futures::future::pending().await,
                    }
                } => {
                    if let Some(ref frame) = hb_frame {
                        let expanded = expand_env_vars(frame);
                        if let Err(e) = write.send(WsMessage::Text(expanded.into())).await {
                            warn!(channel = %self.cfg.name, error = %e, "WS heartbeat send error");
                            break;
                        }
                    }
                }
            }
        }

        // Clear the sender so we don't try to send on a dead connection.
        {
            let mut guard = self.ws_tx.lock().await;
            *guard = None;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Download helpers
// ---------------------------------------------------------------------------

/// Max bytes accepted for a single inbound attachment from an untrusted URL.
/// Caps memory blowup from malicious or oversized payloads.
const MAX_REMOTE_ATTACHMENT_BYTES: u64 = 32 * 1024 * 1024;

/// Reject loopback / private / link-local / unspecified IPs so a malicious
/// upstream payload can't redirect us at the gateway's own internal services.
fn ip_is_safe(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast())
        }
        std::net::IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                return false;
            }
            // fe80::/10 link-local and fc00::/7 ULA.
            let seg = v6.segments()[0];
            if (seg & 0xffc0) == 0xfe80 || (seg & 0xfe00) == 0xfc00 {
                return false;
            }
            // IPv4-mapped (::ffff:0:0/96) — re-check the embedded v4.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return ip_is_safe(std::net::IpAddr::V4(v4));
            }
            true
        }
    }
}

/// Download up to `MAX_REMOTE_ATTACHMENT_BYTES` from `url`, streaming the
/// body. Hardens against SSRF + DNS rebinding by pre-resolving the hostname,
/// rejecting if ANY resolved IP is unsafe, then pinning reqwest to the
/// validated address set via `resolve_to_addrs` so the actual TCP connect
/// can't end up at a different IP than the one we checked.
///
/// `_client` is unused — we build a per-request client so the DNS pin can be
/// scoped to this single download. Channel-level client pooling isn't useful
/// here since each webhook attachment hits a different host.
async fn download_remote_attachment(_client: &Client, url: &str) -> Result<Vec<u8>> {
    let parsed = url::Url::parse(url).context("parse attachment url")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("custom channel: blocked non-http(s) URL {url}");
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("custom channel: URL has no host: {url}"))?
        .to_owned();
    let host_lower = host.to_ascii_lowercase();
    if matches!(
        host_lower.as_str(),
        "localhost" | "ip6-localhost" | "ip6-loopback"
    ) {
        bail!("custom channel: blocked loopback host {host}");
    }
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("custom channel: URL has no port: {url}"))?;

    // Resolve every candidate IP up-front. Bail if ANY is unsafe — even one
    // private-range record in a round-robin set lets an attacker exploit
    // reqwest's choice. This closes the DNS-rebinding window for the duration
    // of the pinned request below.
    let lookup_target = format!("{host}:{port}");
    let addrs: Vec<std::net::SocketAddr> =
        match tokio::net::lookup_host(lookup_target.as_str()).await {
            Ok(it) => it.collect(),
            Err(e) => bail!("custom channel: DNS lookup failed for {host}: {e}"),
        };
    if addrs.is_empty() {
        bail!("custom channel: no DNS records for {host}");
    }
    for addr in &addrs {
        if !ip_is_safe(addr.ip()) {
            bail!(
                "custom channel: DNS resolved {host} to unsafe IP {}",
                addr.ip()
            );
        }
    }

    // Build a per-request client pinned to the validated address set. After
    // this call reqwest will only TCP-connect to one of `addrs`; a malicious
    // resolver flipping records before the request fires cannot redirect us.
    let client = crate::config::build_proxy_client()
        .timeout(Duration::from_secs(30))
        .resolve_to_addrs(&host, &addrs)
        .build()
        .context("build pinned reqwest client")?;

    let resp = client.get(url).send().await.context("send request")?;
    if !resp.status().is_success() {
        bail!("attachment download failed: {}", resp.status());
    }
    if let Some(len) = resp.content_length() {
        if len > MAX_REMOTE_ATTACHMENT_BYTES {
            bail!(
                "attachment too large: content-length {} > {} bytes",
                len,
                MAX_REMOTE_ATTACHMENT_BYTES
            );
        }
    }
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read chunk")?;
        if buf.len() as u64 + chunk.len() as u64 > MAX_REMOTE_ATTACHMENT_BYTES {
            bail!(
                "attachment exceeded {} bytes during streaming",
                MAX_REMOTE_ATTACHMENT_BYTES
            );
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Download an image URL and package as ImageAttachment.
async fn download_and_package_image(
    client: &Client,
    url: &str,
) -> Option<crate::agent::registry::ImageAttachment> {
    let bytes = download_remote_attachment(client, url).await.ok()?;
    if bytes.is_empty() {
        return None;
    }
    let mime = mime_guess::from_path(url).first_or_octet_stream().to_string();
    let (final_bytes, final_mime) =
        rsclaw_util::downscale_image_for_vision(&bytes, &mime, 1 * 1024 * 1024, 1920, 85)
            .unwrap_or_else(|_| (bytes, mime));
    let b64 = base64::engine::general_purpose::STANDARD.encode(&final_bytes);
    let data_url = format!("data:{final_mime};base64,{b64}");
    Some(crate::agent::registry::ImageAttachment {
        data: data_url,
        mime_type: final_mime,
        source_path: None,
    })
}

/// Download multiple image URLs.
async fn download_images(
    client: &Client,
    urls: &[String],
) -> Vec<crate::agent::registry::ImageAttachment> {
    let mut images = Vec::new();
    for url in urls {
        if let Some(img) = download_and_package_image(client, url).await {
            images.push(img);
        }
    }
    images
}

/// Download a file and package as FileAttachment.
async fn download_and_package_file(
    client: &Client,
    file_url: &Option<String>,
    filename: &str,
) -> Vec<crate::agent::registry::FileAttachment> {
    let Some(url) = file_url else { return vec![] };
    match download_remote_attachment(client, url).await {
        Ok(bytes) if !bytes.is_empty() => {
            let mime = mime_guess::from_path(filename)
                .first_or_octet_stream()
                .to_string();
            vec![crate::agent::registry::FileAttachment {
                filename: filename.to_owned(),
                data: bytes,
                mime_type: mime,
            }]
        }
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_path_simple() {
        let val: Value = serde_json::json!({
            "type": "message",
            "data": {
                "text": "hello",
                "from": { "id": "user123" }
            }
        });

        assert_eq!(
            json_path_extract(&val, "$.type").unwrap(),
            &Value::String("message".to_owned())
        );
        assert_eq!(
            json_path_extract(&val, "$.data.text").unwrap(),
            &Value::String("hello".to_owned())
        );
        assert_eq!(
            json_path_extract(&val, "$.data.from.id").unwrap(),
            &Value::String("user123".to_owned())
        );
        assert!(json_path_extract(&val, "$.nonexistent").is_none());
    }

    #[test]
    fn json_path_array_index() {
        let val: Value = serde_json::json!({
            "items": [
                { "name": "first" },
                { "name": "second" }
            ]
        });

        assert_eq!(
            json_path_extract(&val, "$.items[0].name").unwrap(),
            &Value::String("first".to_owned())
        );
        assert_eq!(
            json_path_extract(&val, "$.items[1].name").unwrap(),
            &Value::String("second".to_owned())
        );
        assert!(json_path_extract(&val, "$.items[5].name").is_none());
    }

    #[test]
    fn json_path_no_dollar_prefix() {
        let val: Value = serde_json::json!({"foo": {"bar": 42}});
        assert_eq!(
            json_path_extract(&val, "foo.bar").unwrap(),
            &Value::Number(42.into())
        );
    }

    #[test]
    fn template_rendering() {
        let result = render_template(
            r#"{"to":"{{sender}}","msg":"{{reply}}","group":{{is_group}}}"#,
            "user1",
            "chat1",
            "hello world",
            true,
            "[]",
            "[]",
        );
        assert_eq!(result, r#"{"to":"user1","msg":"hello world","group":true}"#);
    }

    #[test]
    fn template_escapes_json() {
        let result = render_template(
            r#"{"text":"{{reply}}"}"#,
            "",
            "",
            "line1\nline2\"quoted\"",
            false,
            "[]",
            "[]",
        );
        assert_eq!(result, r#"{"text":"line1\nline2\"quoted\""}"#);
    }

    #[test]
    fn env_var_expansion() {
        unsafe {
            std::env::set_var("_RSCLAW_TEST_VAR", "test_value");
        }
        let result = expand_env_vars("prefix-${_RSCLAW_TEST_VAR}-suffix");
        assert_eq!(result, "prefix-test_value-suffix");
        unsafe {
            std::env::remove_var("_RSCLAW_TEST_VAR");
        }
    }

    #[test]
    fn parse_inbound_with_filter() {
        let cfg = CustomChannelConfig {
            name: "test".to_owned(),
            channel_type: "webhook".to_owned(),
            base: Default::default(),
            ws_url: None,
            ws_headers: None,
            auth_frame: None,
            auth_success_path: None,
            auth_success_value: None,
            heartbeat_interval: None,
            heartbeat_frame: None,
            filter_path: Some("$.type".to_owned()),
            filter_value: Some("message".to_owned()),
            text_path: Some("$.data.text".to_owned()),
            sender_path: Some("$.data.from".to_owned()),
            group_path: None,
            image_url_path: None,
            image_urls_path: None,
            file_url_path: None,
            file_name_path: None,
            reply_url: None,
            reply_method: None,
            reply_template: None,
            reply_headers: None,
            reply_frame: None,
        };

        // Matching message.
        let body = r#"{"type":"message","data":{"text":"hello","from":"user1"}}"#;
        let parsed = parse_inbound(&cfg, body).unwrap();
        assert_eq!(parsed.text, "hello");
        assert_eq!(parsed.sender, "user1");
        assert!(parsed.group_id.is_none());

        // Non-matching type.
        let body2 = r#"{"type":"heartbeat","data":{}}"#;
        assert!(parse_inbound(&cfg, body2).is_none());
    }
}
