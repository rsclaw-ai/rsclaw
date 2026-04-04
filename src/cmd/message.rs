use anyhow::{Context, Result};

use crate::cli::message::*;
use crate::config;

/// Resolve gateway URL and auth token from config.
fn gateway_client() -> Result<(reqwest::Client, String, String)> {
    let cfg = config::load().context("failed to load config — run `rsclaw setup` first")?;
    let port = cfg.gateway.port;
    let token = cfg
        .gateway
        .auth_token
        .clone()
        .unwrap_or_default();
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

fn stub(action: &str) {
    println!("{action}: not yet implemented via gateway API");
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

pub async fn cmd_message(sub: MessageCommand) -> Result<()> {
    match sub {
        MessageCommand::Send(args) => message_send(args).await,
        MessageCommand::Read(args) => message_read(args).await,
        MessageCommand::Broadcast(args) => { stub("broadcast"); let _ = args; Ok(()) }
        MessageCommand::Edit(args) => message_edit(args).await,
        MessageCommand::Delete(args) => message_delete(args).await,
        MessageCommand::Pin(args) => { stub("pin"); let _ = args; Ok(()) }
        MessageCommand::Unpin(args) => { stub("unpin"); let _ = args; Ok(()) }
        MessageCommand::Pins(args) => { stub("pins"); let _ = args; Ok(()) }
        MessageCommand::React(args) => { stub("react"); let _ = args; Ok(()) }
        MessageCommand::Reactions(args) => { stub("reactions"); let _ = args; Ok(()) }
        MessageCommand::Poll(args) => { stub("poll"); let _ = args; Ok(()) }
        MessageCommand::Search(args) => { stub("search"); let _ = args; Ok(()) }
        MessageCommand::Thread(sub) => { stub(&format!("thread {sub:?}")); Ok(()) }
        MessageCommand::Voice(sub) => { stub(&format!("voice {sub:?}")); Ok(()) }
        MessageCommand::Sticker(sub) => { stub(&format!("sticker {sub:?}")); Ok(()) }
        MessageCommand::Emoji(sub) => { stub(&format!("emoji {sub:?}")); Ok(()) }
        MessageCommand::Ban(args) => { stub("ban"); let _ = args; Ok(()) }
        MessageCommand::Kick(args) => { stub("kick"); let _ = args; Ok(()) }
        MessageCommand::Timeout(args) => { stub("timeout"); let _ = args; Ok(()) }
        MessageCommand::Member(sub) => { stub(&format!("member {sub:?}")); Ok(()) }
        MessageCommand::Role(sub) => { stub(&format!("role {sub:?}")); Ok(()) }
        MessageCommand::Permissions(args) => { stub("permissions"); let _ = args; Ok(()) }
        MessageCommand::Channel(sub) => { stub(&format!("channel {sub:?}")); Ok(()) }
        MessageCommand::Event(sub) => { stub(&format!("event {sub:?}")); Ok(()) }
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

async fn message_edit(args: MessageEditArgs) -> Result<()> {
    let mut body = serde_json::json!({
        "target": args.target,
        "messageId": args.message_id,
        "message": args.message,
    });
    if let Some(ch) = &args.channel {
        body["channel"] = serde_json::Value::String(ch.clone());
    }
    post("edit", body).await
}

async fn message_delete(args: MessageDeleteArgs) -> Result<()> {
    let mut body = serde_json::json!({
        "target": args.target,
        "messageId": args.message_id,
    });
    if let Some(ch) = &args.channel {
        body["channel"] = serde_json::Value::String(ch.clone());
    }
    post("delete", body).await
}
