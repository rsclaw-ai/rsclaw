//! Custom user-defined channels (webhook and websocket).
//!
//! Allows users to connect any platform without writing code, using JSON path
//! extraction for inbound parsing and template-based outbound replies.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use futures::{SinkExt as _, StreamExt as _, future::BoxFuture};
use reqwest::Client;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tracing::{debug, info, warn};

use super::{Channel, OutboundMessage};
use crate::config::schema::CustomChannelConfig;

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

/// Convert a JSON value to a plain string (unquoted for strings, raw for others).
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

/// Replace `{{sender}}`, `{{chat_id}}`, `{{reply}}`, `{{is_group}}` in a template.
fn render_template(
    template: &str,
    sender: &str,
    chat_id: &str,
    reply: &str,
    is_group: bool,
) -> String {
    template
        .replace("{{sender}}", sender)
        .replace("{{chat_id}}", chat_id)
        .replace("{{reply}}", &escape_json_string(reply))
        .replace("{{is_group}}", if is_group { "true" } else { "false" })
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

    Some(ParsedMessage {
        text,
        sender,
        group_id,
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
    on_message: Arc<dyn Fn(String, String, bool) + Send + Sync>,
}

impl CustomWebhookChannel {
    pub fn new(
        cfg: CustomChannelConfig,
        on_message: Arc<dyn Fn(String, String, bool) + Send + Sync>,
    ) -> Self {
        Self {
            cfg,
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            on_message,
        }
    }

    /// Handle an inbound webhook POST.
    pub fn handle_webhook(&self, body: &str) {
        if let Some(parsed) = parse_inbound(&self.cfg, body) {
            let is_group = parsed.group_id.is_some();
            (self.on_message)(parsed.sender, parsed.text, is_group);
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

        let body = render_template(
            template,
            &msg.target_id,
            &msg.target_id,
            &msg.text,
            msg.is_group,
        );

        let method = self
            .cfg
            .reply_method
            .as_deref()
            .unwrap_or("POST")
            .to_uppercase();

        let mut req = match method.as_str() {
            "PUT" => self.client.put(&reply_url),
            "PATCH" => self.client.patch(&reply_url),
            _ => self.client.post(&reply_url),
        };

        req = req.header("Content-Type", "application/json").body(body);

        if let Some(ref headers) = self.cfg.reply_headers {
            for (k, v) in headers {
                req = req.header(k.as_str(), expand_env_vars(v));
            }
        }

        let resp = req.send().await.context("custom webhook reply HTTP send")?;
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
    #[allow(clippy::type_complexity)]
    on_message: Arc<dyn Fn(String, String, bool) + Send + Sync>,
    /// Sender half for outbound messages.
    ws_tx: Mutex<Option<mpsc::Sender<String>>>,
}

impl CustomWebSocketChannel {
    pub fn new(
        cfg: CustomChannelConfig,
        on_message: Arc<dyn Fn(String, String, bool) + Send + Sync>,
    ) -> Self {
        Self {
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
        Some(render_template(
            template,
            &msg.target_id,
            &msg.target_id,
            &msg.text,
            msg.is_group,
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
                                (self.on_message)(parsed.sender, parsed.text, is_group);
                            }
                        }
                        Some(Ok(WsMessage::Binary(data))) => {
                            let text = String::from_utf8_lossy(&data);
                            if let Some(parsed) = parse_inbound(&self.cfg, &text) {
                                let is_group = parsed.group_id.is_some();
                                (self.on_message)(parsed.sender, parsed.text, is_group);
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
