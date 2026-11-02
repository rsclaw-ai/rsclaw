use anyhow::Result;
use rsclaw_cli::approvals::{AllowlistCommand, ApprovalsCommand};

use super::config_json::{load_config_json, set_nested_value};

pub async fn cmd_approvals(sub: ApprovalsCommand) -> Result<()> {
    match sub {
        // The gateway has no exec-approvals HTTP endpoint; approvals live in
        // the config file (`execApprovals`), which hot-reload picks up.
        ApprovalsCommand::Get => {
            let (_path, val) = load_config_json()?;
            let approvals = val.pointer("/execApprovals").cloned().unwrap_or_default();
            println!("{}", serde_json::to_string_pretty(&approvals)?);
        }
        ApprovalsCommand::Set { file } => {
            let contents = std::fs::read_to_string(&file)?;
            let payload: serde_json::Value = serde_json::from_str(&contents)?;
            let (path, mut val) = load_config_json()?;
            set_nested_value(&mut val, "execApprovals", payload)?;
            std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
            println!("exec approvals written to config file");
        }
        ApprovalsCommand::Allowlist(allowlist_cmd) => match allowlist_cmd {
            AllowlistCommand::Add { agent, pattern } => {
                let (path, mut val) = load_config_json()?;
                let key = format!("execApprovals.allowlist.{agent}");
                let existing = val
                    .pointer(&format!("/execApprovals/allowlist/{agent}"))
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();

                if existing.iter().any(|v| v.as_str() == Some(&pattern)) {
                    println!("pattern '{pattern}' already in allowlist for agent '{agent}'");
                    return Ok(());
                }

                let mut list: Vec<serde_json::Value> = existing;
                list.push(serde_json::Value::String(pattern.clone()));
                set_nested_value(&mut val, &key, serde_json::json!(list))?;
                std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
                println!("added pattern '{pattern}' to allowlist for agent '{agent}'");
            }
            AllowlistCommand::Remove { agent, pattern } => {
                let (path, mut val) = load_config_json()?;
                let key = format!("execApprovals.allowlist.{agent}");
                let existing = val
                    .pointer(&format!("/execApprovals/allowlist/{agent}"))
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();

                let before = existing.len();
                let filtered: Vec<serde_json::Value> = existing
                    .into_iter()
                    .filter(|v| v.as_str() != Some(&pattern))
                    .collect();

                if filtered.len() == before {
                    anyhow::bail!("pattern '{pattern}' not found in allowlist for agent '{agent}'");
                }

                set_nested_value(&mut val, &key, serde_json::json!(filtered))?;
                std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
                println!("removed pattern '{pattern}' from allowlist for agent '{agent}'");
            }
        },
    }
    Ok(())
}
