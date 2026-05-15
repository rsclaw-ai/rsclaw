//! Chat delivery wrapper.

use std::sync::Arc;

use anyhow::{anyhow, Result};

use crate::channel::{ChannelManager, OutboundMessage};

/// Push `body` to (channel, account, peer) using the same ChannelManager
/// that /loop / cron use.
///
/// When `account` is `Some`, we first try the account-keyed channel name
/// (`feishu/main`, `wechat/biz1`, …) so multi-account chat channels route
/// the delivery through the SAME app that received the originating
/// `/watch` command — open IDs are per-app in feishu, so cross-app sends
/// fail with 99992361 "open_id cross app". If the account-keyed channel
/// isn't registered we fall back to the bare name.
///
/// Re-maps the legacy `ws` channel name to `desktop` (same hack as cron's
/// send_delivery at src/cron/mod.rs:1884).
pub async fn deliver(
    channels: &Arc<ChannelManager>,
    channel: &str,
    account: Option<&str>,
    peer: &str,
    body: String,
) -> Result<()> {
    let resolved = if channel == "ws" { "desktop" } else { channel };
    let ch = match account {
        Some(acct) => {
            let keyed = format!("{resolved}/{acct}");
            channels.get(&keyed).or_else(|| channels.get(resolved))
        }
        None => channels.get(resolved),
    };
    let ch = ch.ok_or_else(|| anyhow!("watch: channel `{channel}` not registered"))?;
    let msg = OutboundMessage {
        target_id: peer.to_owned(),
        text: body,
        account: account.map(str::to_owned),
        ..Default::default()
    };
    ch.send(msg).await
}
