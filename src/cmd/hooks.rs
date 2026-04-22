use anyhow::Result;

use super::config_json::load_config_json;
use super::style::*;
use crate::{cli::HooksCommand, config};

pub async fn cmd_hooks(sub: HooksCommand) -> Result<()> {
    let config = config::load()?;
    match sub {
        HooksCommand::List => {
            banner(&format!("rsclaw hooks v{}", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")));
            match config.ops.hooks.as_ref() {
                Some(h) if h.enabled => {
                    let mappings = h.mappings.as_deref().unwrap_or(&[]);
                    if mappings.is_empty() {
                        warn_msg("hooks enabled -- no mappings configured");
                    } else {
                        println!(
                            "  {:<20} {}",
                            bold("PATH"),
                            bold("AGENT")
                        );
                        for m in mappings {
                            let path = m.match_.path.as_deref().unwrap_or("(any)");
                            let agent = m.agent_id.as_deref().unwrap_or("main");
                            println!("  {:<20} {}", cyan(&format!("/{path}")), agent);
                        }
                    }
                }
                _ => warn_msg("hooks disabled"),
            }
        }
        HooksCommand::Check => match config.ops.hooks.as_ref() {
            Some(h) if h.enabled => {
                ok(&format!(
                    "hooks enabled, token:{}",
                    if h.token.is_some() { green("set") } else { yellow("unset") }
                ));
            }
            _ => warn_msg("hooks disabled"),
        },
        HooksCommand::Info { id } => {
            banner(&format!("rsclaw hook info: /{id}"));
            let mappings = config
                .ops
                .hooks
                .as_ref()
                .and_then(|h| h.mappings.as_deref())
                .unwrap_or(&[]);
            let m = mappings
                .iter()
                .find(|m| m.match_.path.as_deref() == Some(&id))
                .ok_or_else(|| anyhow::anyhow!("no hook mapping for path '/{id}'"))?;
            let agent = m.agent_id.as_deref().unwrap_or("main");
            let method = m.match_.method.as_deref().unwrap_or("*");
            let key = m.session_key.as_deref().unwrap_or("(auto)");
            kv("path", &cyan(&format!("/{id}")));
            kv("method", method);
            kv("action", &format!("{:?}", m.action));
            kv("agent", agent);
            kv("session", key);
        }
        HooksCommand::Enable { id } => {
            let (path, mut val) = load_config_json()?;
            hooks_set_mapping_enabled(&mut val, &id, true)?;
            std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
            ok(&format!("hook mapping '{}' enabled", cyan(&format!("/{id}"))));
        }
        HooksCommand::Disable { id } => {
            let (path, mut val) = load_config_json()?;
            hooks_set_mapping_enabled(&mut val, &id, false)?;
            std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
            ok(&format!("hook mapping '{}' disabled", cyan(&format!("/{id}"))));
        }
        HooksCommand::Install => {
            banner(&format!("rsclaw hooks install v{}", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")));
            let port = config.gateway.port;
            let path = config
                .ops
                .hooks
                .as_ref()
                .and_then(|h| h.path.as_deref())
                .unwrap_or("/api/v1/hook");
            let token_set = config.ops.hooks.as_ref().is_some_and(|h| h.token.is_some());
            kv("webhook URL", &bold(&format!("http://127.0.0.1:{port}{path}")));
            if !token_set {
                warn_msg("set hooks.token in config to require authentication");
            } else {
                ok("token configured (set Authorization header on incoming requests)");
            }
        }
        HooksCommand::Update => {
            // Re-validate config; gateway picks up changes on next request (no hot reload
            // for mappings).
            config::validator::validate(&config::load()?)?;
            ok("hooks config validated -- restart the gateway to apply mapping changes");
        }
    }
    Ok(())
}

/// Enable or disable a hook mapping by path id.
/// Since the schema has no per-mapping `enabled` field, this adds/removes a
/// `"_disabled": true` sentinel key so the gateway router can skip it.
pub fn hooks_set_mapping_enabled(
    val: &mut serde_json::Value,
    id: &str,
    enabled: bool,
) -> Result<()> {
    let mappings = val
        .pointer_mut("/hooks/mappings")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| anyhow::anyhow!("no hook mappings configured"))?;
    let entry = mappings
        .iter_mut()
        .find(|m| m["match"]["path"].as_str() == Some(id))
        .ok_or_else(|| anyhow::anyhow!("no hook mapping for path '/{id}'"))?;
    if enabled {
        if let Some(obj) = entry.as_object_mut() {
            obj.remove("_disabled");
        }
    } else {
        entry["_disabled"] = true.into();
    }
    Ok(())
}
