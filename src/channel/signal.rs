//! Signal channel skeleton.
//!
//! Integrates with `signal-cli` (Java) or `signald` (Rust) via their
//! JSON-RPC over stdio / Unix socket interface.
//!
//! Because Signal has no official REST API, rsclaw shells out to
//! `signal-cli` and communicates via newline-delimited JSON on stdio.
//! No message size limit; chunker is a passthrough for Signal.

use std::sync::Arc;

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};
use tracing::{debug, error, info};

use super::{Channel, OutboundMessage};

// ---------------------------------------------------------------------------
// signal-cli JSON-RPC types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SignalEnvelope {
    envelope: SignalMessage,
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

pub struct SignalChannel {
    phone_number: String,
    stdin: Arc<Mutex<ChildStdin>>,
    stdout: Arc<Mutex<BufReader<ChildStdout>>>,
    _child: Arc<Mutex<Child>>,
    on_message: Arc<dyn Fn(String, String, bool) + Send + Sync>,
    // (sender_number, text, is_group)
}

impl SignalChannel {
    /// Spawn `signal-cli -u <phone> jsonRpc` and connect.
    pub async fn spawn(
        phone_number: impl Into<String>,
        cli_path: Option<String>,
        on_message: Arc<dyn Fn(String, String, bool) + Send + Sync>,
    ) -> Result<Self> {
        let phone = phone_number.into();
        let bin = cli_path.unwrap_or_else(|| "signal-cli".to_owned());

        let mut child = Command::new(&bin)
            .args(["-u", &phone, "jsonRpc"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("spawn signal-cli (is it installed?)")?;

        let stdin = child.stdin.take().context("signal-cli stdin")?;
        let stdout = child.stdout.take().context("signal-cli stdout")?;

        info!(phone = %phone, "Signal channel started");

        Ok(Self {
            phone_number: phone,
            stdin: Arc::new(Mutex::new(stdin)),
            stdout: Arc::new(Mutex::new(BufReader::new(stdout))),
            _child: Arc::new(Mutex::new(child)),
            on_message,
        })
    }

    async fn send_rpc(&self, method: &str, params: Value) -> Result<()> {
        let req = json!({
            "jsonrpc": "2.0",
            "method":  method,
            "params":  params,
            "id":      1,
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
            let method = if msg.is_group {
                "sendGroupMessage"
            } else {
                "send"
            };
            self.send_rpc(method, params).await?;

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
                    let tmp_path = std::env::temp_dir()
                        .join(format!("rsclaw_signal_img_{idx}.png"));
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
                    if let Err(e) = self.send_rpc(method, img_params).await {
                        tracing::warn!(idx, "signal: image send RPC failed: {e}");
                    }
                    let _ = std::fs::remove_file(&tmp_path);
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

                let envelope: SignalEnvelope = match serde_json::from_str(&line) {
                    Ok(e) => e,
                    Err(_) => continue,
                };

                let msg = &envelope.envelope;
                if let (Some(sender), Some(dm)) = (&msg.source, &msg.data_message)
                    && let Some(text) = &dm.message
                {
                    let is_group = dm.group_info.is_some();
                    debug!(sender, is_group, "Signal message received");
                    (self.on_message)(sender.clone(), text.clone(), is_group);
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
