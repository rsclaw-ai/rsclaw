use anyhow::Result;

use super::config_json::{load_config_json, set_nested_value};
use super::style::*;
use crate::{
    cli::{HeartbeatCommand, SystemCommand},
    config,
};

pub async fn cmd_system(sub: SystemCommand) -> Result<()> {
    match sub {
        SystemCommand::Event => {
            banner(&format!("rsclaw system event v{}", env!("RSCLAW_BUILD_VERSION")));
            kv("stream", &dim("/api/v1/stream (SSE)"));
            println!(
                "  {}",
                dim("Use `rsclaw gateway start` and connect to the SSE endpoint")
            );
        }
        SystemCommand::Presence => {
            banner(&format!("rsclaw system presence v{}", env!("RSCLAW_BUILD_VERSION")));
            let hb_file = config::loader::base_dir().join("var/data/heartbeat.json");
            if hb_file.exists() {
                let raw = std::fs::read_to_string(&hb_file)?;
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                    let ts = v["lastSeen"].as_str().unwrap_or("unknown");
                    kv("last seen", &bold(ts));
                    return Ok(());
                }
            }
            warn_msg("no heartbeat data found");
        }
        SystemCommand::Heartbeat(sub) => match sub {
            HeartbeatCommand::Last => {
                let hb_file = config::loader::base_dir().join("var/data/heartbeat.json");
                if hb_file.exists() {
                    let raw = std::fs::read_to_string(&hb_file)?;
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                        let ts = v["lastSeen"].as_str().unwrap_or("unknown");
                        kv("last heartbeat", &bold(ts));
                        return Ok(());
                    }
                }
                warn_msg("no heartbeat data");
            }
            HeartbeatCommand::Enable => {
                let (path, mut val) = load_config_json()?;
                set_nested_value(&mut val, "system.heartbeat.enabled", true.into())?;
                std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
                ok("heartbeat enabled");
            }
            HeartbeatCommand::Disable => {
                let (path, mut val) = load_config_json()?;
                set_nested_value(&mut val, "system.heartbeat.enabled", false.into())?;
                std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
                ok("heartbeat disabled");
            }
        },
    }
    Ok(())
}
