use anyhow::{Context, Result};

use crate::cli::message::*;
use crate::config;

/// Resolve gateway URL and auth token from config.
fn gateway_client() -> Result<(reqwest::Client, String, String)> {
    let cfg = config::load().context("failed to load config -- run `rsclaw setup` first")?;
    let port = cfg.gateway.port;
    let token = cfg.gateway.auth_token.unwrap_or_default();
    let base = format!("http://127.0.0.1:{port}");
    Ok((reqwest::Client::new(), base, token))
}

/// POST helper — sends JSON body and prints the response.
async fn post(endpoint: &str, body: serde_json::Value) -> Result<()> {
    let (client, base, token) = gateway_client()?;
    let url = format!("{base}/api/v1/message/{endpoint}");
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let text = r.text().await.unwrap_or_default();
            if text.is_empty() {
                println!("[ok]");
            } else {
                println!("{text}");
            }
        }
        Ok(r) => {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            println!("[error] {status}: {text}");
        }
        Err(e) => {
            println!("[error] gateway not reachable: {e}");
            println!("        start the gateway first: rsclaw gateway start");
        }
    }
    Ok(())
}

/// GET helper — sends query and prints the response.
async fn get(endpoint: &str, query: &[(&str, &str)]) -> Result<()> {
    let (client, base, token) = gateway_client()?;
    let url = format!("{base}/api/v1/message/{endpoint}");
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .query(query)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let text = r.text().await.unwrap_or_default();
            if text.is_empty() {
                println!("[ok]");
            } else {
                println!("{text}");
            }
        }
        Ok(r) => {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            println!("[error] {status}: {text}");
        }
        Err(e) => {
            println!("[error] gateway not reachable: {e}");
            println!("        start the gateway first: rsclaw gateway start");
        }
    }
    Ok(())
}

fn unsupported(action: &str) {
    eprintln!("[unsupported] `message {action}` is not implemented. Supported: send, read, broadcast");
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

pub async fn cmd_message(sub: MessageCommand) -> Result<()> {
    match sub {
        MessageCommand::Send(args) => message_send(args).await,
        MessageCommand::Read(args) => message_read(args).await,
        MessageCommand::Broadcast(args) => message_broadcast(args).await,
        MessageCommand::Edit(args) => { unsupported("edit"); let _ = args; Ok(()) }
        MessageCommand::Delete(args) => { unsupported("delete"); let _ = args; Ok(()) }
        MessageCommand::Pin(args) => { unsupported("pin"); let _ = args; Ok(()) }
        MessageCommand::Unpin(args) => { unsupported("unpin"); let _ = args; Ok(()) }
        MessageCommand::Pins(args) => { unsupported("pins"); let _ = args; Ok(()) }
        MessageCommand::React(args) => { unsupported("react"); let _ = args; Ok(()) }
        MessageCommand::Reactions(args) => { unsupported("reactions"); let _ = args; Ok(()) }
        MessageCommand::Poll(args) => { unsupported("poll"); let _ = args; Ok(()) }
        MessageCommand::Search(args) => { unsupported("search"); let _ = args; Ok(()) }
        MessageCommand::Thread(sub) => { unsupported(&format!("thread {sub:?}")); Ok(()) }
        MessageCommand::Voice(sub) => { unsupported(&format!("voice {sub:?}")); Ok(()) }
        MessageCommand::Sticker(sub) => { unsupported(&format!("sticker {sub:?}")); Ok(()) }
        MessageCommand::Emoji(sub) => { unsupported(&format!("emoji {sub:?}")); Ok(()) }
        MessageCommand::Ban(args) => { unsupported("ban"); let _ = args; Ok(()) }
        MessageCommand::Kick(args) => { unsupported("kick"); let _ = args; Ok(()) }
        MessageCommand::Timeout(args) => { unsupported("timeout"); let _ = args; Ok(()) }
        MessageCommand::Member(sub) => { unsupported(&format!("member {sub:?}")); Ok(()) }
        MessageCommand::Role(sub) => { unsupported(&format!("role {sub:?}")); Ok(()) }
        MessageCommand::Permissions(args) => { unsupported("permissions"); let _ = args; Ok(()) }
        MessageCommand::Channel(sub) => { unsupported(&format!("channel {sub:?}")); Ok(()) }
        MessageCommand::Event(sub) => { unsupported(&format!("event {sub:?}")); Ok(()) }
    }
}

// ---------------------------------------------------------------------------
// Implemented operations
// ---------------------------------------------------------------------------

async fn message_send(args: MessageSendArgs) -> Result<()> {
    let mut body = serde_json::json!({
        "target": args.target,
        "message": args.message,
    });
    if let Some(ch) = &args.channel {
        body["channel"] = serde_json::Value::String(ch.clone());
    }
    if let Some(media) = &args.media {
        body["media"] = serde_json::Value::String(media.clone());
    }
    if let Some(reply) = &args.reply_to {
        body["replyTo"] = serde_json::Value::String(reply.clone());
    }
    post("send", body).await
}

async fn message_read(args: MessageReadArgs) -> Result<()> {
    let limit_str = args.limit.unwrap_or(20).to_string();
    let mut query: Vec<(&str, &str)> = vec![
        ("target", &args.target),
        ("limit", &limit_str),
    ];
    let ch;
    if let Some(ref c) = args.channel {
        ch = c.clone();
        query.push(("channel", &ch));
    }
    get("read", &query).await
}

async fn message_broadcast(args: MessageBroadcastArgs) -> Result<()> {
    let mut body = serde_json::json!({
        "targets": args.targets,
        "message": args.message,
    });
    if let Some(ch) = &args.channel {
        body["channel"] = serde_json::Value::String(ch.clone());
    }
    post("broadcast", body).await
}
