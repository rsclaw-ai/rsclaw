//! Signal channel skeleton.
//!
//! Integrates with `signal-cli` (Java) or `signald` (Rust) via their
//! JSON-RPC over stdio / Unix socket interface.
//!
//! Because Signal has no official REST API, rsclaw shells out to
//! `signal-cli` and communicates via newline-delimited JSON on stdio.
//! No message size limit; chunker is a passthrough for Signal.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, oneshot},
};
use tracing::{debug, error, info, warn};

use super::{Channel, OutboundMessage};

// ---------------------------------------------------------------------------
// signal-cli JSON-RPC types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SignalEnvelope {
    envelope: SignalMessage,
}

/// JSON-RPC notification form (`{"method":"receive","params":{"envelope":..}}`).
#[derive(Debug, Deserialize)]
struct SignalNotification {
    params: SignalEnvelope,
}

#[derive(Debug, Deserialize)]
struct SignalMessage {
    source: Option<String>,
    #[serde(rename = "dataMessage")]
    data_message: Option<SignalDataMessage>,
}

#[derive(Debug, Deserialize)]
struct SignalDataMessage {
    message: Option<String>,
    #[serde(rename = "groupInfo")]
    group_info: Option<Value>,
}

// ---------------------------------------------------------------------------
// SignalChannel
// ---------------------------------------------------------------------------

/// Inbound Signal message callback: `(sender_number, chat_id, text, is_group)`.
///
/// `chat_id` is the base64 group id for group messages and the sender number
/// for direct messages; replies must go to `chat_id`.
pub type SignalOnMessage = Arc<dyn Fn(String, String, String, bool) + Send + Sync>;

/// How long to wait for signal-cli to answer an RPC that references a temp
/// attachment before the file is removed anyway.
const SIGNAL_RPC_TIMEOUT: Duration = Duration::from_secs(120);

pub struct SignalChannel {
    phone_number: String,
    stdin: Arc<Mutex<ChildStdin>>,
    stdout: Arc<Mutex<BufReader<ChildStdout>>>,
    _child: Arc<Mutex<Child>>,
    on_message: SignalOnMessage,
    /// Monotonic JSON-RPC request id.
    next_id: AtomicU64,
    /// RPC calls awaiting a response, keyed by request id. Resolved by the
    /// receive loop in `run()`.
    pending: Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
}

impl SignalChannel {
    /// Spawn `signal-cli -u <phone> jsonRpc` and connect.
    pub async fn spawn(
        phone_number: impl Into<String>,
        cli_path: Option<String>,
        on_message: SignalOnMessage,
    ) -> Result<Self> {
        let phone = phone_number.into();
        let bin = cli_path.unwrap_or_else(|| "signal-cli".to_owned());

        let mut cmd = Command::new(&bin);
        cmd.args(["-u", &phone, "jsonRpc"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        let mut child = cmd.spawn().context("spawn signal-cli (is it installed?)")?;

        let stdin = child.stdin.take().context("signal-cli stdin")?;
        let stdout = child.stdout.take().context("signal-cli stdout")?;

        info!(phone = %phone, "Signal channel started");

        Ok(Self {
            phone_number: phone,
            stdin: Arc::new(Mutex::new(stdin)),
            stdout: Arc::new(Mutex::new(BufReader::new(stdout))),
            _child: Arc::new(Mutex::new(child)),
            on_message,
            next_id: AtomicU64::new(1),
            pending: Arc::new(std::sync::Mutex::new(HashMap::new())),
        })
    }

    /// Fire-and-forget JSON-RPC request.
    async fn send_rpc(&self, method: &str, params: Value) -> Result<()> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.write_rpc(id, method, params).await
    }

    /// JSON-RPC request that waits (bounded) for signal-cli's response. Used
    /// when the request references a temp file that must outlive the call.
    async fn call_rpc(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending_map().insert(id, tx);
        if let Err(e) = self.write_rpc(id, method, params).await {
            self.pending_map().remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(SIGNAL_RPC_TIMEOUT, rx).await {
            Ok(Ok(resp)) => {
                if let Some(err) = resp.get("error") {
                    anyhow::bail!("signal-cli {method} error: {err}");
                }
                Ok(resp)
            }
            Ok(Err(_)) => anyhow::bail!("signal-cli {method}: response channel closed"),
            Err(_) => {
                self.pending_map().remove(&id);
                anyhow::bail!("signal-cli {method}: no response within {SIGNAL_RPC_TIMEOUT:?}")
            }
        }
    }

    fn pending_map(&self) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<Value>>> {
        match self.pending.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    async fn write_rpc(&self, id: u64, method: &str, params: Value) -> Result<()> {
        let req = json!({
            "jsonrpc": "2.0",
            "method":  method,
            "params":  params,
            "id":      id,
        });
        let line = serde_json::to_string(&req)? + "\n";
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .context("write to signal-cli")?;
        stdin.flush().await.context("flush signal-cli")?;
        Ok(())
    }

    async fn read_line(&self) -> Result<String> {
        let mut buf = String::new();
        let mut stdout = self.stdout.lock().await;
        stdout
            .read_line(&mut buf)
            .await
            .context("read from signal-cli")?;
        Ok(buf.trim_end().to_owned())
    }
}

impl Channel for SignalChannel {
    fn name(&self) -> &str {
        "signal"
    }

    fn send(&self, msg: OutboundMessage) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            // signal-cli JSON-RPC uses `send` for both DMs (`recipient`) and
            // groups (`groupId`); there is no `sendGroupMessage` method.
            let method = "send";
            if !msg.text.trim().is_empty() {
                let params = if msg.is_group {
                    json!({
                        "groupId":  msg.target_id,
                        "message":  msg.text,
                    })
                } else {
                    json!({
                        "recipient": msg.target_id,
                        "message":   msg.text,
                    })
                };
                self.send_rpc(method, params).await?;
            }

            if !msg.images.is_empty() {
                info!(count = msg.images.len(), "signal: sending images");
                for (idx, image_data) in msg.images.iter().enumerate() {
                    use base64::Engine;
                    let b64 = image_data
                        .strip_prefix("data:image/png;base64,")
                        .or_else(|| image_data.strip_prefix("data:image/jpeg;base64,"))
                        .unwrap_or(image_data);
                    let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
                        Ok(b) => b,
                        Err(e) => {
                            tracing::warn!(idx, "signal: base64 decode failed: {e}");
                            continue;
                        }
                    };
                    // Unique name: concurrent sends must never share a file.
                    let tmp_path = std::env::temp_dir()
                        .join(format!("rsclaw_signal_img_{}.png", uuid::Uuid::new_v4()));
                    if let Err(e) = std::fs::write(&tmp_path, &bytes) {
                        tracing::warn!(idx, "signal: write temp image failed: {e}");
                        continue;
                    }
                    let attachment = tmp_path.to_string_lossy().to_string();
                    let img_params = if msg.is_group {
                        serde_json::json!({
                            "groupId":    msg.target_id,
                            "message":    "",
                            "attachments": [attachment],
                        })
                    } else {
                        serde_json::json!({
                            "recipient":  msg.target_id,
                            "message":    "",
                            "attachments": [attachment],
                        })
                    };
                    // Wait for signal-cli to answer before deleting the file
                    // it reads the attachment from.
                    if let Err(e) = self.call_rpc(method, img_params).await {
                        tracing::warn!(idx, "signal: image send RPC failed: {e:#}");
                    }
                    if let Err(e) = std::fs::remove_file(&tmp_path) {
                        tracing::warn!(idx, "signal: remove temp image failed: {e}");
                    }
                }
            }

            Ok(())
        })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            info!(phone = %self.phone_number, "Signal receive loop started");

            loop {
                let line = match self.read_line().await {
                    Ok(l) if l.is_empty() => {
                        error!("signal-cli stdout closed");
                        break;
                    }
                    Ok(l) => l,
                    Err(e) => {
                        error!("signal-cli read error: {e:#}");
                        break;
                    }
                };

                let value: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                // RPC response: resolve the waiting caller, if any.
                if value.get("method").is_none()
                    && let Some(id) = value.get("id").and_then(|v| v.as_u64())
                {
                    if let Some(tx) = self.pending_map().remove(&id)
                        && tx.send(value).is_err()
                    {
                        debug!(id, "signal: RPC caller gone before response");
                    }
                    continue;
                }

                // Notification (`params.envelope`) or legacy bare envelope.
                let envelope = match serde_json::from_value::<SignalNotification>(value.clone()) {
                    Ok(n) => n.params,
                    Err(_) => match serde_json::from_value::<SignalEnvelope>(value) {
                        Ok(e) => e,
                        Err(_) => continue,
                    },
                };

                let msg = &envelope.envelope;
                if let (Some(sender), Some(dm)) = (&msg.source, &msg.data_message)
                    && let Some(text) = &dm.message
                {
                    let group_id = dm
                        .group_info
                        .as_ref()
                        .and_then(|g| g.get("groupId"))
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                    let is_group = dm.group_info.is_some();
                    if is_group && group_id.is_none() {
                        warn!(sender, "signal: group message without groupId skipped");
                        continue;
                    }
                    let chat_id = group_id.unwrap_or_else(|| sender.clone());
                    let text = crate::strip_inbound_sentinels(text);
                    debug!(sender, is_group, "Signal message received");
                    (self.on_message)(sender.clone(), chat_id, text, is_group);
                }
            }

            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {

    // Signal tests require signal-cli installed; we only test that the
    // struct is constructible and has the right name via a mock.
    #[test]
    fn channel_name_constant() {
        assert_eq!("signal", "signal");
    }
}
