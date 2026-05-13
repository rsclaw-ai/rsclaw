//! Chat delivery wrapper.

use std::sync::Arc;

use anyhow::{anyhow, Result};

use crate::channel::{ChannelManager, OutboundMessage};

/// Push `body` to (channel, peer) using the same ChannelManager that /loop /
/// cron use. Re-maps the legacy `ws` channel name to `desktop` (same hack as
/// cron's send_delivery at src/cron/mod.rs:1884).
pub async fn deliver(
    channels: &Arc<ChannelManager>,
    channel: &str,
    peer: &str,
    body: String,
) -> Result<()> {
    let resolved = if channel == "ws" { "desktop" } else { channel };
    let ch = channels
        .get(resolved)
        .ok_or_else(|| anyhow!("watch: channel `{channel}` not registered"))?;
    let msg = OutboundMessage {
        target_id: peer.to_owned(),
        text: body,
        ..Default::default()
    };
    ch.send(msg).await
}
