use anyhow::Result;

use crate::cli::directory::{DirectoryCommand, GroupsCommand, PeersCommand};
use crate::config;

/// Build gateway base URL from config.
fn gateway_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

pub async fn cmd_directory(sub: DirectoryCommand) -> Result<()> {
    let cfg = config::load().ok();
    let port = cfg.as_ref().map_or(18888, |c| c.gateway.port);
    let auth_token = cfg
        .as_ref()
        .and_then(|c| c.gateway.auth_token.as_deref())
        .unwrap_or("");
    let base = gateway_url(port);
    let client = reqwest::Client::new();

    match sub {
        DirectoryCommand::Self_ { channel } => {
            let url = format!("{base}/api/v1/directory/{channel}/self");
            let resp = client
                .get(&url)
                .header("Authorization", format!("Bearer {auth_token}"))
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    let body: serde_json::Value = r.json().await.unwrap_or_default();
                    let id = body["id"].as_str().unwrap_or("unknown");
                    let name = body["name"].as_str().unwrap_or("unknown");
                    let handle = body["handle"].as_str().unwrap_or("-");
                    println!("id:     {id}");
                    println!("name:   {name}");
                    println!("handle: {handle}");
                }
                _ => {
                    anyhow::bail!(
                        "gateway not reachable at port {port}; start it with: rsclaw gateway start"
                    );
                }
            }
        }
        DirectoryCommand::Peers(peers_cmd) => match peers_cmd {
            PeersCommand::List { channel, query } => {
                let mut url = format!("{base}/api/v1/directory/{channel}/peers");
                if let Some(ref q) = query {
                    // Simple percent-encode for query param safety.
                    let encoded: String = q
                        .bytes()
                        .flat_map(|b| {
                            if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' {
                                vec![b as char]
                            } else {
                                format!("%{b:02X}").chars().collect()
                            }
                        })
                        .collect();
                    url.push_str(&format!("?query={encoded}"));
                }
                let resp = client
                    .get(&url)
                    .header("Authorization", format!("Bearer {auth_token}"))
                    .send()
                    .await;

                match resp {
                    Ok(r) if r.status().is_success() => {
                        let body: serde_json::Value = r.json().await.unwrap_or_default();
                        if let Some(arr) = body.as_array() {
                            if arr.is_empty() {
                                println!("no peers found");
                            } else {
                                for p in arr {
                                    let id = p["id"].as_str().unwrap_or("-");
                                    let name = p["name"].as_str().unwrap_or("-");
                                    let handle = p["handle"].as_str().unwrap_or("-");
                                    println!("{id}  name:{name}  handle:{handle}");
                                }
                            }
                        }
                    }
                    _ => {
                        anyhow::bail!(
                            "gateway not reachable at port {port}; start it with: rsclaw gateway start"
                        );
                    }
                }
            }
        },
        DirectoryCommand::Groups(groups_cmd) => match groups_cmd {
            GroupsCommand::List { channel } => {
                let url = format!("{base}/api/v1/directory/{channel}/groups");
                let resp = client
                    .get(&url)
                    .header("Authorization", format!("Bearer {auth_token}"))
                    .send()
                    .await;

                match resp {
                    Ok(r) if r.status().is_success() => {
                        let body: serde_json::Value = r.json().await.unwrap_or_default();
                        if let Some(arr) = body.as_array() {
                            if arr.is_empty() {
                                println!("no groups found");
                            } else {
                                for g in arr {
                                    let id = g["id"].as_str().unwrap_or("-");
                                    let name = g["name"].as_str().unwrap_or("-");
                                    let count = g["memberCount"]
                                        .as_u64()
                                        .map(|n| n.to_string())
                                        .unwrap_or_else(|| "-".to_owned());
                                    println!("{id}  name:{name}  members:{count}");
                                }
                            }
                        }
                    }
                    _ => {
                        anyhow::bail!(
                            "gateway not reachable at port {port}; start it with: rsclaw gateway start"
                        );
                    }
                }
            }
            GroupsCommand::Members { channel, group_id } => {
                let url =
                    format!("{base}/api/v1/directory/{channel}/groups/{group_id}/members");
                let resp = client
                    .get(&url)
                    .header("Authorization", format!("Bearer {auth_token}"))
                    .send()
                    .await;

                match resp {
                    Ok(r) if r.status().is_success() => {
                        let body: serde_json::Value = r.json().await.unwrap_or_default();
                        if let Some(arr) = body.as_array() {
                            if arr.is_empty() {
                                println!("no members found");
                            } else {
                                for m in arr {
                                    let id = m["id"].as_str().unwrap_or("-");
                                    let name = m["name"].as_str().unwrap_or("-");
                                    let role = m["role"].as_str().unwrap_or("-");
                                    println!("{id}  name:{name}  role:{role}");
                                }
                            }
                        }
                    }
                    _ => {
                        anyhow::bail!(
                            "gateway not reachable at port {port}; start it with: rsclaw gateway start"
                        );
                    }
                }
            }
        },
    }
    Ok(())
}
