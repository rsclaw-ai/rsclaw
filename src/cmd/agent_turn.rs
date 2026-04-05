use anyhow::Result;

use crate::cli::agent_turn::AgentTurnArgs;
use crate::config;

pub async fn cmd_agent_turn(args: AgentTurnArgs) -> Result<()> {
    let cfg = config::load().ok();
    let port = cfg.as_ref().map_or(18888, |c| c.gateway.port);
    let auth_token = cfg
        .as_ref()
        .and_then(|c| c.gateway.auth_token.as_deref())
        .unwrap_or("");

    // Build the request payload.
    let mut payload = serde_json::json!({});

    if let Some(ref to) = args.to {
        payload["to"] = serde_json::Value::String(to.clone());
    }
    if let Some(ref message) = args.message {
        payload["message"] = serde_json::Value::String(message.clone());
    }
    if args.deliver {
        payload["deliver"] = serde_json::Value::Bool(true);
    }
    if let Some(ref thinking) = args.thinking {
        payload["thinking"] = serde_json::Value::String(thinking.clone());
    }
    if args.local {
        payload["local"] = serde_json::Value::Bool(true);
    }
    if let Some(ref channel) = args.channel {
        payload["channel"] = serde_json::Value::String(channel.clone());
    }
    if let Some(ref agent) = args.agent {
        payload["agent"] = serde_json::Value::String(agent.clone());
    }
    if let Some(ref session_id) = args.session_id {
        payload["sessionId"] = serde_json::Value::String(session_id.clone());
    }
    if let Some(timeout) = args.timeout {
        payload["timeout"] = serde_json::Value::Number(serde_json::Number::from(timeout));
    }
    if let Some(ref reply_to) = args.reply_to {
        payload["replyTo"] = serde_json::Value::String(reply_to.clone());
    }
    if let Some(ref reply_channel) = args.reply_channel {
        payload["replyChannel"] = serde_json::Value::String(reply_channel.clone());
    }
    if let Some(ref reply_account) = args.reply_account {
        payload["replyAccount"] = serde_json::Value::String(reply_account.clone());
    }

    let url = format!("http://127.0.0.1:{port}/api/v1/agent/turn");
    let client = reqwest::Client::new();
    let timeout_dur = std::time::Duration::from_secs(args.timeout.unwrap_or(120));

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {auth_token}"))
        .timeout(timeout_dur)
        .json(&payload)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let body: serde_json::Value = r.json().await.unwrap_or_default();
            if args.json {
                println!("{}", serde_json::to_string_pretty(&body)?);
            } else {
                // Print the reply text.
                let reply = body["reply"]
                    .as_str()
                    .or_else(|| body["content"].as_str())
                    .or_else(|| body["text"].as_str())
                    .unwrap_or("(no reply)");
                println!("{reply}");

                if args.deliver {
                    let delivered = body["delivered"].as_bool().unwrap_or(false);
                    if delivered {
                        println!("\n[delivered to channel]");
                    } else {
                        println!("\n[delivery not confirmed]");
                    }
                }
            }
        }
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            anyhow::bail!("gateway returned {status}: {body}");
        }
        Err(e) => {
            anyhow::bail!(
                "gateway not reachable at port {port}: {e}\nstart it with: rsclaw gateway start"
            );
        }
    }

    Ok(())
}
