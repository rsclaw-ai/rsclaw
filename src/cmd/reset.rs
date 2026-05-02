use anyhow::Result;

use super::style::*;
use crate::{
    cli::ResetArgs,
    config,
};

pub async fn cmd_reset(args: ResetArgs) -> Result<()> {
    let base_dir = config::loader::base_dir();

    let scope = args.scope.as_deref().unwrap_or("full");

    if args.dry_run {
        banner(&format!("rsclaw reset (dry run) v{}", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")));
        match scope {
            "config" => {
                if let Some(path) = config::loader::detect_config_path() {
                    warn_msg(&format!("would remove config: {}", bold(&path.display().to_string())));
                } else {
                    warn_msg("no config file found");
                }
            }
            "full" => {
                if base_dir.exists() {
                    warn_msg(&format!(
                        "would remove state dir: {}",
                        bold(&base_dir.display().to_string())
                    ));
                } else {
                    warn_msg(&format!(
                        "state dir not found: {}",
                        dim(&base_dir.display().to_string())
                    ));
                }
            }
            other => anyhow::bail!("unknown reset scope: {other} (use 'config' or 'full')"),
        }
        return Ok(());
    }

    banner(&format!("rsclaw reset v{}", option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")));
    println!("  {}", red("WARNING: This is a destructive operation!"));
    println!();

    match scope {
        "config" => {
            if let Some(path) = config::loader::detect_config_path() {
                println!("  {} {}", red("removing"), bold(&path.display().to_string()));
                std::fs::remove_file(&path)?;
                ok(&format!("removed config: {}", dim(&path.display().to_string())));
            } else {
                warn_msg("no config file found");
            }
        }
        "full" => {
            if base_dir.exists() {
                println!("  {} {}", red("removing"), bold(&base_dir.display().to_string()));
                std::fs::remove_dir_all(&base_dir)?;
                ok(&format!("removed state dir: {}", dim(&base_dir.display().to_string())));
            } else {
                warn_msg(&format!(
                    "state dir not found: {}",
                    dim(&base_dir.display().to_string())
                ));
            }
        }
        other => anyhow::bail!("unknown reset scope: {other} (use 'config' or 'full')"),
    }
    Ok(())
}
