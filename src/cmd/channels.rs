use anyhow::Result;

use super::{
    config_json::{load_config_json, remove_nested_value, set_nested_value},
    style::*,
};
use crate::{cli::ChannelsCommand, config};

pub async fn cmd_channels(sub: ChannelsCommand) -> Result<()> {
    let config = config::load()?;
    match sub {
        ChannelsCommand::List | ChannelsCommand::Status => {
            banner(&format!(
                "rsclaw channels v{}",
                env!("RSCLAW_BUILD_VERSION")
            ));
            let ch = &config.channel.channels;
            let is_on = |b: Option<&crate::config::schema::ChannelBase>| {
                b.is_some_and(|b| b.enabled.unwrap_or(true))
            };
            println!("  {:<14} {}", bold("CHANNEL"), bold("STATUS"));
            if is_on(ch.telegram.as_ref().map(|c| &c.base)) {
                println!("  {:<14} {}", "telegram", green("enabled"));
            }
            if is_on(ch.discord.as_ref().map(|c| &c.base)) {
                println!("  {:<14} {}", "discord", green("enabled"));
            }
            if is_on(ch.slack.as_ref().map(|c| &c.base)) {
                println!("  {:<14} {}", "slack", green("enabled"));
            }
            if is_on(ch.whatsapp.as_ref().map(|c| &c.base)) {
                println!("  {:<14} {}", "whatsapp", green("enabled"));
            }
            if is_on(ch.signal.as_ref().map(|c| &c.base)) {
                println!("  {:<14} {}", "signal", green("enabled"));
            }
            if is_on(ch.line.as_ref().map(|c| &c.base)) {
                println!("  {:<14} {}", "line", green("enabled"));
            }
            if is_on(ch.zalo.as_ref().map(|c| &c.base)) {
                println!("  {:<14} {}", "zalo", green("enabled"));
            }
            if is_on(ch.matrix.as_ref().map(|c| &c.base)) {
                println!("  {:<14} {}", "matrix", green("enabled"));
            }
            println!("  {:<14} {}", "cli", dim("always"));
        }
        ChannelsCommand::Logs { channel } => {
            let log_file = config::loader::log_file();
            if !log_file.exists() {
                warn_msg("no gateway.log found -- is the gateway running?");
                return Ok(());
            }
            let content = std::fs::read_to_string(&log_file)?;
            let filter = channel.as_deref().unwrap_or("").to_lowercase();
            for line in content.lines() {
                if filter.is_empty() || line.to_lowercase().contains(&filter) {
                    println!("{line}");
                }
            }
        }
        ChannelsCommand::Add { channel } => {
            let (path, mut val) = load_config_json()?;
            let key = format!("channels.{channel}.enabled");
            set_nested_value(&mut val, &key, serde_json::Value::Bool(true))?;
            std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
            ok(&format!(
                "enabled channel '{}' -- set the required token in {}",
                cyan(&channel),
                dim(&path.display().to_string())
            ));
        }
        ChannelsCommand::Remove { channel } => {
            let (path, mut val) = load_config_json()?;
            if let Some(channels) = val.get_mut("channels").and_then(|v| v.as_object_mut()) {
                channels.remove(&channel);
            }
            std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
            ok(&format!("removed channel '{}'", cyan(&channel)));
        }
        ChannelsCommand::Login { channel } => {
            banner(&format!(
                "rsclaw channel login v{}",
                env!("RSCLAW_BUILD_VERSION")
            ));
            match channel.as_str() {
                "wechat" | "weixin" | "openclaw-weixin" => {
                    kv("channel", &cyan("WeChat Personal"));
                    let client = reqwest::Client::new();
                    let (_url, qrcode) =
                        crate::channel::wechat::WeChatPersonalChannel::start_qr_login(&client)
                            .await?;
                    let (_token, bot_id) =
                        crate::channel::wechat::WeChatPersonalChannel::wait_qr_login(
                            &client, &qrcode,
                        )
                        .await?;
                    ok(&format!("login successful, bot_id={}", bold(&bot_id)));
                    kv("token saved", "rsclaw.json5");
                    println!("  {}", dim("Restart gateway to activate."));
                }
                "feishu" | "lark" | "openclaw-lark" => {
                    let client = reqwest::Client::new();
                    let brand = if channel == "lark" { "lark" } else { "feishu" };
                    let (app_id, app_secret, actual_brand) =
                        crate::channel::auth::feishu_auth::onboard(&client, brand).await?;

                    // Update config with the new feishu credentials
                    let (path, mut val) = load_config_json()?;
                    let channels = val.as_object_mut().and_then(|o| {
                        o.entry("channels")
                            .or_insert(serde_json::json!({}))
                            .as_object_mut()
                    });
                    if let Some(channels) = channels {
                        channels.insert(
                            "openclaw-lark".to_owned(),
                            serde_json::json!({
                                "enabled": true,
                                "appId": app_id,
                                "appSecret": app_secret,
                                "brand": actual_brand,
                                "connectionMode": "websocket",
                                "dmPolicy": "open",
                                "groupPolicy": "open",
                            }),
                        );
                    }
                    std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
                    ok(&format!(
                        "config saved to {}",
                        dim(&path.display().to_string())
                    ));
                    println!("  {}", dim("Restart gateway to activate."));
                }
                "dingtalk" => {
                    let config = config::load()?;
                    let dt = config.channel.channels.dingtalk.as_ref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "dingtalk not configured -- add channels.dingtalk section to config"
                        )
                    })?;
                    let app_key = dt
                        .app_key
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("channels.dingtalk.appKey not set"))?;
                    let app_secret = dt
                        .app_secret
                        .as_ref()
                        .and_then(|s| s.as_plain())
                        .ok_or_else(|| anyhow::anyhow!("channels.dingtalk.appSecret not set"))?;
                    let client = reqwest::Client::new();
                    crate::channel::auth::dingtalk_auth::login(&client, app_key, app_secret, None)
                        .await?;
                }
                "telegram" => kv(
                    "action",
                    &format!(
                        "set {} in your config",
                        cyan("channels.telegram.botToken = \"${TELEGRAM_BOT_TOKEN}\"")
                    ),
                ),
                "discord" => kv(
                    "action",
                    &format!(
                        "set {} in your config",
                        cyan("channels.discord.token = \"${DISCORD_BOT_TOKEN}\"")
                    ),
                ),
                "slack" => kv(
                    "action",
                    &format!(
                        "set {} in your config",
                        cyan("channels.slack.botToken and channels.slack.appToken")
                    ),
                ),
                "whatsapp" => kv(
                    "action",
                    &format!("set {} in your config", cyan("channels.whatsapp.apiKey")),
                ),
                "signal" => kv(
                    "action",
                    &format!("set {} in your config", cyan("channels.signal.phoneNumber")),
                ),
                _ => kv("action", "set the required credentials in your config"),
            }
        }
        ChannelsCommand::Logout { channel } => {
            let (path, mut val) = load_config_json()?;
            for key in ["botToken", "token", "appToken", "apiKey"] {
                let full = format!("channels.{channel}.{key}");
                remove_nested_value(&mut val, &full);
            }
            std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
            ok(&format!(
                "removed credentials for channel '{}'",
                cyan(&channel)
            ));
        }

        ChannelsCommand::Pair { code } => {
            // Approve a pairing code by calling the running gateway's API.
            let config = config::load()?;
            let port = std::env::var("RSCLAW_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(config.gateway.port);
            let api_url = format!("http://127.0.0.1:{port}/api/v1/channels/pair");
            let client = reqwest::Client::new();
            let auth_token_val = config
                .gateway
                .auth_token
                .clone()
                .or_else(|| std::env::var("RSCLAW_AUTH_TOKEN").ok())
                .unwrap_or_default();
            let auth_token = auth_token_val.as_str();

            let resp = client
                .post(&api_url)
                .header("Authorization", format!("Bearer {auth_token}"))
                .json(&serde_json::json!({ "code": code }))
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    let body: serde_json::Value = r.json().await.unwrap_or_default();
                    let peer = body["peerId"].as_str().unwrap_or("unknown");
                    let channel = body["channel"].as_str().unwrap_or("unknown");
                    ok(&format!(
                        "approved peer {} on {}",
                        bold(peer),
                        cyan(channel)
                    ));

                    // Also persist to openclaw-compatible credentials file.
                    persist_allow_from(channel, peer);
                }
                Ok(r) => {
                    let status = r.status();
                    let body = r.text().await.unwrap_or_default();
                    if status.as_u16() == 401 {
                        err_msg(
                            "unauthorized -- check RSCLAW_AUTH_TOKEN or gateway.auth.token config",
                        );
                    } else {
                        err_msg(&format!("pair failed ({status}): {body}"));
                    }
                }
                Err(_) => {
                    err_msg(&format!("gateway not reachable at port {port}"));
                    println!(
                        "      Pairing codes can only be approved while the gateway is running."
                    );
                    println!(
                        "      Start the gateway first: {}",
                        bold("rsclaw gateway start")
                    );
                }
            }
        }

        ChannelsCommand::Unpair { channel, peer } => {
            let mut changed = false;

            // 1. Remove from allowFrom credentials file.
            let rs_path = rsclaw_allow_from_path(&channel);
            if rs_path.exists()
                && let Ok(content) = std::fs::read_to_string(&rs_path)
                && let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&content)
                && let Some(arr) = val.get_mut("allowFrom").and_then(|v| v.as_array_mut())
            {
                let before = arr.len();
                arr.retain(|v| v.as_str() != Some(&peer));
                if arr.len() < before {
                    std::fs::write(&rs_path, serde_json::to_string_pretty(&val)?)?;
                    changed = true;
                }
            }

            // 2. Call gateway API to revoke from memory + redb (gateway holds the redb
            //    lock).
            let config = crate::config::load().ok();
            let port = std::env::var("RSCLAW_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or_else(|| config.as_ref().map_or(18888, |c| c.gateway.port));
            let auth_token = config
                .as_ref()
                .and_then(|c| c.gateway.auth_token.as_deref())
                .unwrap_or("");
            let api_url = format!("http://127.0.0.1:{port}/api/v1/channels/unpair");
            match reqwest::Client::new()
                .post(&api_url)
                .header("Authorization", format!("Bearer {auth_token}"))
                .json(&serde_json::json!({ "channel": channel, "peerId": peer }))
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => {
                    changed = true;
                }
                Ok(_) | Err(_) => {
                    // Gateway not running -- delete from redb directly.
                    let data_dir = crate::config::loader::base_dir().join("var/data");
                    if let Ok(store) = crate::store::redb_store::RedbStore::open(
                        &data_dir.join("redb/data.redb"),
                        crate::sys::detect_memory_tier(),
                    ) {
                        if store.delete_pairing(&channel, &peer).is_ok() {
                            changed = true;
                        }
                    }
                }
            }

            if changed {
                ok(&format!(
                    "revoked peer {} from {}",
                    bold(&peer),
                    cyan(&channel)
                ));
            } else {
                warn_msg(&format!(
                    "peer {} not found in {} allowFrom or pairing list",
                    bold(&peer),
                    cyan(&channel)
                ));
            }
        }

        ChannelsCommand::Capabilities { channel } => {
            banner(&format!(
                "rsclaw channel capabilities v{}",
                env!("RSCLAW_BUILD_VERSION")
            ));
            let caps = match channel.as_str() {
                "telegram" => vec![
                    "text",
                    "image",
                    "audio",
                    "video",
                    "document",
                    "sticker",
                    "location",
                    "inline-query",
                    "dm",
                    "group",
                ],
                "discord" => vec![
                    "text", "image", "audio", "video", "embed", "reaction", "dm", "group", "thread",
                ],
                "slack" => vec![
                    "text",
                    "image",
                    "file",
                    "block-kit",
                    "reaction",
                    "dm",
                    "group",
                    "thread",
                ],
                "whatsapp" => vec![
                    "text", "image", "audio", "video", "document", "location", "dm", "group",
                ],
                "signal" => vec!["text", "image", "audio", "video", "dm", "group"],
                "feishu" | "lark" => {
                    vec!["text", "image", "file", "interactive-card", "dm", "group"]
                }
                "dingtalk" => vec![
                    "text",
                    "image",
                    "file",
                    "markdown",
                    "action-card",
                    "dm",
                    "group",
                ],
                "qq" => vec!["text", "image", "audio", "dm", "group"],
                "wechat" => vec!["text", "image", "file", "dm", "group"],
                "wecom" => vec!["text", "image", "file", "markdown", "dm", "group"],
                "line" => vec!["text", "image", "dm", "group"],
                "zalo" => vec!["text", "dm"],
                "matrix" => vec!["text", "image", "dm", "group"],
                "cli" => vec!["text", "image", "dm"],
                _ => vec!["text", "dm"],
            };
            kv("channel", &cyan(&channel));
            kv("capabilities", &caps.join(", "));
        }
        ChannelsCommand::Resolve { channel, name } => {
            let config = config::load()?;
            let port = config.gateway.port;
            let auth_token_val = config
                .gateway
                .auth_token
                .clone()
                .or_else(|| std::env::var("RSCLAW_AUTH_TOKEN").ok())
                .unwrap_or_default();
            let auth_token = auth_token_val.as_str();
            let url = format!("http://127.0.0.1:{port}/api/v1/channels/{channel}/resolve");
            let client = reqwest::Client::new();
            match client
                .get(&url)
                .header("Authorization", format!("Bearer {auth_token}"))
                .query(&[("name", &name)])
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    let body: serde_json::Value = resp.json().await.unwrap_or_default();
                    let id = body["id"].as_str().unwrap_or("(not found)");
                    ok(&format!(
                        "{}: {} -> {}",
                        cyan(&channel),
                        bold(&name),
                        green(id)
                    ));
                }
                Ok(resp) => {
                    err_msg(&format!("resolve failed: HTTP {}", resp.status()));
                }
                Err(_) => {
                    err_msg("gateway not reachable -- start the gateway first");
                }
            }
        }
        ChannelsCommand::Paired { channel } => {
            banner(&format!(
                "rsclaw paired peers v{}",
                env!("RSCLAW_BUILD_VERSION")
            ));
            let channels_to_check: Vec<String> = if let Some(ch) = channel {
                vec![ch]
            } else {
                vec![
                    "telegram", "discord", "slack", "whatsapp", "qq", "feishu", "dingtalk",
                    "wechat", "wecom", "matrix", "line", "zalo",
                ]
                .into_iter()
                .map(String::from)
                .collect()
            };

            // Open redb to read persisted pairings.
            let data_dir = crate::config::loader::base_dir().join("var/data");
            let redb_store = crate::store::redb_store::RedbStore::open(
                &data_dir.join("redb/data.redb"),
                crate::sys::detect_memory_tier(),
            )
            .ok();

            let mut found_any = false;
            for ch in &channels_to_check {
                let mut peers: Vec<String> = Vec::new();

                // 1. From allowFrom credentials file.
                let rs_path = rsclaw_allow_from_path(ch);
                if let Ok(content) = std::fs::read_to_string(&rs_path)
                    && let Ok(val) = serde_json::from_str::<serde_json::Value>(&content)
                    && let Some(arr) = val.get("allowFrom").and_then(|v| v.as_array())
                {
                    for item in arr {
                        if let Some(id) = item.as_str()
                            && !peers.contains(&id.to_owned())
                        {
                            peers.push(id.to_owned());
                        }
                    }
                }

                // 2. From redb pairing store.
                if let Some(ref store) = redb_store {
                    if let Ok(redb_peers) = store.list_pairings(ch) {
                        for p in redb_peers {
                            if !peers.contains(&p) {
                                peers.push(p);
                            }
                        }
                    }
                }

                if !peers.is_empty() {
                    found_any = true;
                    println!("  {}:", cyan(ch));
                    for p in &peers {
                        println!("    {}", bold(p));
                    }
                }
            }
            if !found_any {
                warn_msg("no approved peers found");
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Credentials file helpers
// ---------------------------------------------------------------------------

fn rsclaw_allow_from_path(channel: &str) -> std::path::PathBuf {
    crate::config::loader::base_dir().join(format!("credentials/{channel}-default-allowFrom.json"))
}

/// Persist an approved peer to the credentials file (both openclaw and rsclaw
/// paths).
pub fn persist_allow_from_pub(channel: &str, peer_id: &str) {
    persist_allow_from(channel, peer_id);
}

fn persist_allow_from(channel: &str, peer_id: &str) {
    // Write to rsclaw credential path.
    for path in [rsclaw_allow_from_path(channel)] {
        let mut val = if let Ok(content) = std::fs::read_to_string(&path) {
            serde_json::from_str(&content)
                .unwrap_or(serde_json::json!({"version": 1, "allowFrom": []}))
        } else {
            serde_json::json!({"version": 1, "allowFrom": []})
        };

        if let Some(arr) = val.get_mut("allowFrom").and_then(|v| v.as_array_mut()) {
            let peer_val = serde_json::Value::String(peer_id.to_owned());
            if !arr.contains(&peer_val) {
                arr.push(peer_val);
            }
        }

        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(
            &path,
            serde_json::to_string_pretty(&val).unwrap_or_default(),
        );
    }
}
