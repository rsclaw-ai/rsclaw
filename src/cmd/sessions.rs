use std::io::Write;

use anyhow::{Result, bail};

use super::style::*;
use crate::cli::SessionsCommand;

/// Resolve the gateway base URL.
fn gateway_url() -> String {
    let base = crate::config::loader::base_dir();
    // Try to read port from config file.
    let config_path = base.join("rsclaw.json5");
    let port = crate::config::loader::load_json5(&config_path)
        .ok()
        .and_then(|c| c.gateway.as_ref()?.port)
        .unwrap_or(18888);
    format!("http://127.0.0.1:{port}")
}

/// Resolve auth token from config.
fn gateway_token() -> String {
    let base = crate::config::loader::base_dir();
    let config_path = base.join("rsclaw.json5");
    crate::config::loader::load_json5(&config_path)
        .ok()
        .and_then(|c| c.gateway.as_ref()?.auth.as_ref()?.token.as_ref()?.as_plain().map(str::to_owned))
        .or_else(|| std::env::var("RSCLAW_AUTH_TOKEN").ok())
        .unwrap_or_default()
}

/// GET a JSON endpoint from the running gateway.
async fn api_get(path: &str) -> Result<serde_json::Value> {
    let url = format!("{}/api/v1{path}", gateway_url());
    let token = gateway_token();
    let client = reqwest::Client::new();
    let mut req = client.get(&url);
    if !token.is_empty() {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let resp = req.send().await
        .map_err(|e| anyhow::anyhow!("gateway not reachable ({url}): {e}"))?;
    if !resp.status().is_success() {
        bail!("gateway returned {}: {}", resp.status(), resp.text().await.unwrap_or_default());
    }
    Ok(resp.json().await?)
}

pub async fn cmd_sessions(sub: SessionsCommand) -> Result<()> {
    match sub {
        SessionsCommand::List(args) => {
            let data = api_get("/sessions").await?;
            let sessions = data["sessions"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>())
                .unwrap_or_default();

            if sessions.is_empty() {
                if args.json {
                    println!("[]");
                } else {
                    banner(&format!("rsclaw sessions v{}", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")));
                    warn_msg("no sessions");
                }
            } else if args.json {
                let arr: Vec<serde_json::Value> = sessions
                    .iter()
                    .map(|s| serde_json::json!({"id": s}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr)?);
            } else {
                banner(&format!("rsclaw sessions v{}", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")));
                kv("total", &bold(&sessions.len().to_string()));
                println!();
                for s in &sessions {
                    item("-", &cyan(s));
                }
            }
        }
        SessionsCommand::Export(args) => {
            let data = api_get("/sessions").await?;
            let sessions = data["sessions"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>())
                .unwrap_or_default();

            let target_sessions: Vec<String> = if let Some(ref key) = args.session {
                sessions.into_iter().filter(|s| s == key).collect()
            } else {
                sessions.into_iter().take(args.limit).collect()
            };

            let mut writer: Box<dyn std::io::Write> = if let Some(ref path) = args.output {
                Box::new(std::io::BufWriter::new(std::fs::File::create(path)?))
            } else {
                Box::new(std::io::stdout().lock())
            };

            let mut total = 0usize;
            for session_key in &target_sessions {
                let path = format!("/sessions/{}/messages", urlencoding::encode(session_key));
                match api_get(&path).await {
                    Ok(messages) => {
                        let record = serde_json::json!({
                            "session": session_key,
                            "messages": messages,
                        });
                        writeln!(writer, "{}", serde_json::to_string(&record)?)?;
                        total += 1;
                    }
                    Err(e) => eprintln!("skip {session_key}: {e}"),
                }
            }

            if args.output.is_some() {
                eprintln!("exported {} session(s)", total);
            }
        }
        SessionsCommand::Cleanup(_args) => {
            // Cleanup requires write access — must be done via gateway API.
            // For now, inform the user to use /clear or /new from the chat.
            warn_msg("cleanup via CLI is not supported while gateway is running. Use /clear or /new from chat.");
        }
    }
    Ok(())
}
