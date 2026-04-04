//! CLI interactive channel.
//!
//! Reads lines from stdin and sends replies to stdout.
//! Useful for development, testing, and `rsclaw tui`.

use std::sync::Arc;

use anyhow::Result;
use futures::future::BoxFuture;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::debug;

use super::{Channel, OutboundMessage};

pub const CLI_CHANNEL_NAME: &str = "cli";
/// Synthetic peer ID for CLI usage.
pub const CLI_PEER_ID: &str = "cli_user";

// ---------------------------------------------------------------------------
// CliChannel
// ---------------------------------------------------------------------------

pub struct CliChannel {
    /// Called with (peer_id, text) when a line arrives on stdin.
    on_message: Arc<dyn Fn(String, String) + Send + Sync>,
}

impl CliChannel {
    pub fn new(on_message: Arc<dyn Fn(String, String) + Send + Sync>) -> Self {
        Self { on_message }
    }
}

impl Channel for CliChannel {
    fn name(&self) -> &str {
        CLI_CHANNEL_NAME
    }

    fn send(&self, msg: OutboundMessage) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let mut stdout = tokio::io::stdout();
            stdout.write_all(msg.text.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
            if !msg.images.is_empty() {
                tracing::debug!("cli: image sending not yet implemented");
            }
            Ok(())
        })
    }

    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let stdin = tokio::io::stdin();
            let reader = BufReader::new(stdin);
            let mut lines = reader.lines();

            while let Some(line) = lines.next_line().await? {
                let trimmed = line.trim().to_owned();
                if trimmed.is_empty() {
                    continue;
                }
                debug!(text = %trimmed, "CLI message received");
                (self.on_message)(CLI_PEER_ID.to_owned(), trimmed);
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

    use super::*;

    #[tokio::test]
    async fn send_writes_to_stdout() {
        // We can't easily capture stdout in tests, but we can verify
        // that send() doesn't error.
        let ch = CliChannel::new(Arc::new(|_, _| {}));
        let result = ch
            .send(OutboundMessage {
                target_id: CLI_PEER_ID.to_owned(),
                is_group: false,
                text: "hello from test".to_owned(),
                reply_to: None,
                images: vec![],
            })
            .await;
        assert!(result.is_ok());
    }

    #[test]
    fn channel_name_is_cli() {
        let ch = CliChannel::new(Arc::new(|_, _| {}));
        assert_eq!(ch.name(), "cli");
    }
}
