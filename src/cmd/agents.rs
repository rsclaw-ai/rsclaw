use anyhow::Result;

use super::config_json::{load_config_json, set_nested_value};
use super::style::*;
use crate::{cli::AgentsCommand, config};

pub async fn cmd_agents(sub: AgentsCommand) -> Result<()> {
    match sub {
        AgentsCommand::List => {
            banner(&format!("rsclaw agents v{}", env!("RSCLAW_BUILD_VERSION")));
            let config = config::load()?;
            println!(
                "  {:<16} {:<24} {:<10} {}",
                bold("ID"),
                bold("MODEL"),
                bold("DEFAULT"),
                bold("CHANNELS")
            );
            for a in &config.agents.list {
                let default_marker = if a.default == Some(true) {
                    green("yes")
                } else {
                    dim("no")
                };
                let channels = a
                    .channels
                    .as_ref()
                    .map(|c| c.join(", "))
                    .unwrap_or_else(|| "all".to_owned());
                let model = a
                    .model
                    .as_ref()
                    .and_then(|m| m.primary.as_deref())
                    .unwrap_or("(default)");
                println!(
                    "  {:<16} {:<24} {:<10} {}",
                    cyan(&a.id),
                    model,
                    default_marker,
                    channels
                );
            }
        }
        AgentsCommand::Add { name } => {
            let (path, mut val) = load_config_json()?;
            let new_agent = serde_json::json!({ "id": name, "default": false });
            if let Some(list) = val
                .pointer_mut("/agents/list")
                .and_then(|v| v.as_array_mut())
            {
                if list.iter().any(|a| a["id"].as_str() == Some(&name)) {
                    anyhow::bail!("agent '{name}' already exists");
                }
                list.push(new_agent);
            } else {
                set_nested_value(&mut val, "agents.list", serde_json::json!([new_agent]))?;
            }
            std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
            ok(&format!("added agent '{}'", cyan(&name)));
        }
        AgentsCommand::Delete { id } => {
            let (path, mut val) = load_config_json()?;
            if let Some(list) = val
                .pointer_mut("/agents/list")
                .and_then(|v| v.as_array_mut())
            {
                let before = list.len();
                list.retain(|a| a["id"].as_str() != Some(&id));
                if list.len() == before {
                    anyhow::bail!("agent '{id}' not found");
                }
            }
            std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
            ok(&format!("deleted agent '{}'", cyan(&id)));
        }
        AgentsCommand::Bindings => {
            banner(&format!("rsclaw agent bindings v{}", env!("RSCLAW_BUILD_VERSION")));
            let config = config::load()?;
            if config.agents.bindings.is_empty() {
                warn_msg("no bindings configured");
            } else {
                println!(
                    "  {:<14} {:<14} {:<14} {}",
                    bold("CHANNEL"),
                    bold("PEER"),
                    bold("AGENT"),
                    bold("PRIORITY")
                );
                for b in &config.agents.bindings {
                    let agent = b.agent_id.as_str();
                    let channel = b.match_.channel.as_deref().unwrap_or("*");
                    let peer = b.match_.peer_id.as_deref().unwrap_or("*");
                    let prio = b.priority.unwrap_or(0);
                    println!(
                        "  {:<14} {:<14} {:<14} {}",
                        channel,
                        peer,
                        cyan(agent),
                        dim(&prio.to_string())
                    );
                }
            }
        }
        AgentsCommand::Bind(args) => {
            let (path, mut val) = load_config_json()?;
            let mut binding = serde_json::json!({
                "agentId": args.agent_id,
                "match": {}
            });
            if let Some(ch) = &args.channel {
                binding["match"]["channel"] = ch.clone().into();
            }
            if let Some(peer) = &args.peer_id {
                binding["match"]["peerId"] = peer.clone().into();
            }
            if let Some(grp) = &args.group_id {
                binding["match"]["groupId"] = grp.clone().into();
            }
            if let Some(prio) = args.priority {
                binding["priority"] = prio.into();
            }
            if let Some(arr) = val.pointer_mut("/bindings").and_then(|v| v.as_array_mut()) {
                arr.push(binding);
            } else {
                val["bindings"] = serde_json::json!([binding]);
            }
            std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
            ok(&format!("added binding for agent '{}'", cyan(&args.agent_id)));
        }
        AgentsCommand::Unbind { binding_id } => {
            // binding_id is the agent_id for simplicity (remove first match)
            let (path, mut val) = load_config_json()?;
            if let Some(arr) = val.pointer_mut("/bindings").and_then(|v| v.as_array_mut()) {
                let before = arr.len();
                arr.retain(|b| b["agentId"].as_str() != Some(&binding_id));
                if arr.len() == before {
                    anyhow::bail!("no binding found for agent '{binding_id}'");
                }
            }
            std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
            ok(&format!("removed binding for '{}'", cyan(&binding_id)));
        }
        AgentsCommand::SetIdentity {
            id,
            name,
            theme,
            emoji,
            avatar,
        } => {
            let (path, mut val) = load_config_json()?;
            if let Some(list) = val
                .pointer_mut("/agents/list")
                .and_then(|v| v.as_array_mut())
            {
                let agent = list
                    .iter_mut()
                    .find(|a| a["id"].as_str() == Some(&id))
                    .ok_or_else(|| anyhow::anyhow!("agent '{id}' not found"))?;
                if let Some(name) = name {
                    agent["name"] = name.into();
                }
                if let Some(theme) = theme {
                    agent["theme"] = theme.into();
                }
                if let Some(emoji) = emoji {
                    agent["emoji"] = emoji.into();
                }
                if let Some(avatar) = avatar {
                    agent["avatar"] = avatar.into();
                }
            } else {
                anyhow::bail!("no agents.list found in config");
            }
            std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
            ok(&format!("updated identity for agent '{}'", cyan(&id)));
        }
    }
    Ok(())
}
